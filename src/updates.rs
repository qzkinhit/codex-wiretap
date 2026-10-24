//! 仅从固定 GitHub 仓库的最新正式 Release 检查和下载更新，不执行安装器。
use anyhow::{Result, ensure};
use futures_util::StreamExt;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, sync::Mutex};

const LATEST_API: &str = "https://api.github.com/repos/qzkinhit/codex-wiretap/releases/latest";
const RELEASE_ROOT: &str = "https://github.com/qzkinhit/codex-wiretap/releases";
const MAC_ASSET: &str = "PoolRouter-macOS-arm64.dmg";
const WINDOWS_ASSET: &str = "PoolRouter-Windows-x64-Setup.exe";
const CHECKSUM_ASSET: &str = "SHA256SUMS";
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Serialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub download_supported: bool,
    pub asset_name: Option<String>,
    pub release_url: Option<String>,
    pub published_at: Option<String>,
    pub message: String,
}

/// 路径仅供进程内部打开已校验的文件，不能直接序列化为 API 响应。
#[derive(Debug)]
pub struct DownloadedUpdate {
    pub version: String,
    pub filename: String,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Version(u64, u64, u64);

impl Version {
    fn parse(raw: &str) -> Result<Self> {
        ensure!(raw.len() <= 64, "更新版本格式无效");
        let raw = raw.strip_prefix('v').unwrap_or(raw);
        let parts = raw.split('.').collect::<Vec<_>>();
        ensure!(parts.len() == 3, "更新版本格式无效");
        let mut numbers = [0_u64; 3];
        for (index, part) in parts.into_iter().enumerate() {
            ensure!(
                !part.is_empty()
                    && part.bytes().all(|byte| byte.is_ascii_digit())
                    && (part == "0" || !part.starts_with('0')),
                "更新版本格式无效"
            );
            numbers[index] = part
                .parse()
                .map_err(|_| anyhow::anyhow!("更新版本格式无效"))?;
        }
        Ok(Self(numbers[0], numbers[1], numbers[2]))
    }

    fn label(self) -> String {
        format!("{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    html_url: String,
    published_at: Option<String>,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

#[derive(Clone)]
struct Asset {
    name: String,
    url: Url,
    size: u64,
    digest: Option<[u8; 32]>,
}

#[derive(Clone)]
struct Release {
    checked_at: Instant,
    version: Version,
    release_url: String,
    published_at: Option<String>,
    installer: Option<Asset>,
    checksums: Option<Asset>,
}

pub struct UpdateService {
    data_dir: PathBuf,
    asset_name: Option<&'static str>,
    release: Mutex<Option<Release>>,
    download_lock: Mutex<()>,
    #[cfg(test)]
    fixture_origin: Option<Url>,
}

impl UpdateService {
    pub fn new(data_dir: PathBuf) -> Self {
        let asset_name = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some(MAC_ASSET)
        } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Some(WINDOWS_ASSET)
        } else {
            None
        };
        Self {
            data_dir,
            asset_name,
            release: Mutex::new(None),
            download_lock: Mutex::new(()),
            #[cfg(test)]
            fixture_origin: None,
        }
    }

    pub async fn check(&self) -> Result<UpdateInfo> {
        let mut cached = self
            .release
            .try_lock()
            .map_err(|_| anyhow::anyhow!("正在检查更新，请稍后重试"))?;
        if cached
            .as_ref()
            .is_none_or(|release| release.checked_at.elapsed() >= CACHE_TTL)
        {
            *cached = Some(self.fetch_release().await?);
        }
        self.info(
            cached
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("更新信息不可用"))?,
        )
    }

    pub async fn download(&self, expected_version: &str) -> Result<DownloadedUpdate> {
        let expected = Version::parse(expected_version)?;
        let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
        ensure!(expected > current, "只能下载比当前版本更新的正式版本");
        let _download = self
            .download_lock
            .try_lock()
            .map_err(|_| anyhow::anyhow!("正在下载更新，请等待当前下载完成"))?;
        let release = {
            let mut cached = self
                .release
                .try_lock()
                .map_err(|_| anyhow::anyhow!("正在检查更新，请稍后重试"))?;
            let previous = cached
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("请先检查更新并确认版本"))?;
            ensure!(
                previous.version == expected,
                "更新版本已变化，请重新检查并确认"
            );
            if previous.checked_at.elapsed() >= CACHE_TTL {
                *cached = Some(self.fetch_release().await?);
            }
            let release = cached
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("更新信息不可用"))?;
            ensure!(
                release.version == expected,
                "更新版本已变化，请重新检查并确认"
            );
            ensure!(
                self.info(release)?.download_supported,
                "当前平台或发布版本没有可校验的安装包"
            );
            release.clone()
        };
        let installer = release
            .installer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("发布版本缺少安装包"))?;
        let client = client()?;
        let expected_hash = match installer.digest {
            Some(digest) => digest,
            None => {
                let sums = release
                    .checksums
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("发布版本缺少可靠的 SHA-256 校验信息"))?;
                let bytes = self.read_asset(&client, sums, MAX_METADATA_BYTES).await?;
                if let Some(digest) = sums.digest {
                    ensure!(
                        sha256(&bytes) == digest,
                        "校验清单的 SHA-256 不匹配，已停止下载"
                    );
                }
                checksum_for(&bytes, &installer.name)?
            }
        };
        let version = release.version.label();
        let directory = self.data_dir.join("updates").join(&version);
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|_| anyhow::anyhow!("无法创建更新下载目录"))?;
        let path = directory.join(&installer.name);
        let mut partial = PartialDownload::new(
            directory.join(format!(".download-{}.partial", uuid::Uuid::new_v4())),
        );
        let result = self
            .download_file(&client, installer, expected_hash, partial.path(), &path)
            .await;
        if let Err(error) = result {
            partial.cleanup().await;
            return Err(error);
        }
        partial.committed();
        Ok(DownloadedUpdate {
            version,
            filename: installer.name.clone(),
            path,
        })
    }

    fn info(&self, release: &Release) -> Result<UpdateInfo> {
        let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
        let newer = release.version > current;
        let verified_asset = release
            .installer
            .as_ref()
            .is_some_and(|asset| asset.digest.is_some() || release.checksums.is_some());
        let download_supported = newer && self.asset_name.is_some() && verified_asset;
        let message = if !newer {
            "当前已是最新版本，或当前版本高于最新正式发布版"
        } else if self.asset_name.is_none() {
            "发现新版本；当前平台请前往 GitHub Release 查看安装方式"
        } else if release.installer.is_none() {
            "发现新版本，但发布版本尚未提供当前平台的安装包"
        } else if !verified_asset {
            "发现新版本，但缺少可靠的 SHA-256 校验信息，已禁用下载"
        } else {
            "发现新版本，可下载并校验安装包"
        };
        Ok(UpdateInfo {
            current_version: current.label(),
            latest_version: Some(release.version.label()),
            update_available: newer,
            download_supported,
            asset_name: release.installer.as_ref().map(|asset| asset.name.clone()),
            release_url: Some(release.release_url.clone()),
            published_at: release.published_at.clone(),
            message: message.into(),
        })
    }

    async fn fetch_release(&self) -> Result<Release> {
        let client = client()?;
        let url = Url::parse(LATEST_API).map_err(|_| anyhow::anyhow!("更新服务地址无效"))?;
        let response = client
            .get(self.request_url(url))
            .timeout(Duration::from_secs(15))
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("无法连接 GitHub 更新服务"))?;
        ensure!(
            response.status().is_success(),
            "GitHub 正式版本信息暂时不可用，请稍后重试"
        );
        let bytes = bounded_body(response, MAX_METADATA_BYTES, None).await?;
        let metadata: GithubRelease = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("GitHub 正式版本信息格式无效"))?;
        ensure!(
            !metadata.draft && !metadata.prerelease,
            "更新服务未返回正式发布版本"
        );
        let version = Version::parse(&metadata.tag_name)?;
        ensure!(
            metadata.html_url == format!("{RELEASE_ROOT}/tag/{}", metadata.tag_name),
            "正式版本页面地址不符合固定仓库要求"
        );
        let installer = self
            .asset_name
            .map(|name| select_asset(&metadata, name, MAX_DOWNLOAD_BYTES))
            .transpose()?
            .flatten();
        let checksums = if installer
            .as_ref()
            .is_some_and(|asset| asset.digest.is_none())
        {
            select_asset(&metadata, CHECKSUM_ASSET, MAX_METADATA_BYTES)?
        } else {
            None
        };
        Ok(Release {
            checked_at: Instant::now(),
            version,
            release_url: metadata.html_url,
            published_at: metadata.published_at.filter(|value| {
                value.len() <= 40
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || b"TZ:+-.".contains(&byte))
            }),
            installer,
            checksums,
        })
    }

    async fn read_asset(&self, client: &Client, asset: &Asset, limit: u64) -> Result<Vec<u8>> {
        let response = self.asset_response(client, asset).await?;
        bounded_body(response, limit, Some(asset.size)).await
    }

    async fn asset_response(&self, client: &Client, asset: &Asset) -> Result<reqwest::Response> {
        let response = client
            .get(self.request_url(asset.url.clone()))
            .header("accept", "application/octet-stream")
            .header("accept-encoding", "identity")
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("无法下载 GitHub 发布资产"))?;
        ensure!(
            response.status().is_success(),
            "GitHub 发布资产暂时不可用，请稍后重试"
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length == asset.size),
            "安装包响应长度与发布清单不一致"
        );
        Ok(response)
    }

    async fn download_file(
        &self,
        client: &Client,
        asset: &Asset,
        expected_hash: [u8; 32],
        partial: &Path,
        destination: &Path,
    ) -> Result<()> {
        let response = self.asset_response(client, asset).await?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(partial)
            .await
            .map_err(|_| anyhow::anyhow!("无法创建临时更新文件"))?;
        let mut stream = response.bytes_stream();
        let mut size = 0_u64;
        let mut digest = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| anyhow::anyhow!("安装包下载中断，请重试"))?;
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("安装包超过大小限制"))?;
            ensure!(
                size <= asset.size && size <= MAX_DOWNLOAD_BYTES,
                "安装包超过发布清单或下载大小限制"
            );
            digest.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|_| anyhow::anyhow!("无法写入临时更新文件"))?;
        }
        ensure!(size == asset.size, "安装包长度与发布清单不一致");
        let actual: [u8; 32] = digest.finalize().into();
        ensure!(
            actual == expected_hash,
            "安装包 SHA-256 校验失败，已拒绝保存更新"
        );
        file.sync_all()
            .await
            .map_err(|_| anyhow::anyhow!("无法保存已校验的更新文件"))?;
        drop(file);
        tokio::fs::rename(partial, destination)
            .await
            .map_err(|_| anyhow::anyhow!("无法完成更新文件保存"))?;
        Ok(())
    }

    fn request_url(&self, url: Url) -> Url {
        #[cfg(test)]
        if let Some(origin) = &self.fixture_origin {
            // 仅测试版本允许将已通过固定仓库校验的 URL 映射到本地 fixture。
            let mut local = origin.clone();
            local.set_path(url.path());
            local.set_query(url.query());
            return local;
        }
        url
    }
}

fn client() -> Result<Client> {
    let builder = Client::builder()
        .user_agent(concat!("PoolRouter/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .no_gzip()
        .no_deflate()
        .no_zstd()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !allowed_redirect(attempt.url()) {
                attempt.error("更新下载重定向地址不被允许")
            } else {
                attempt.follow()
            }
        }));
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder
        .build()
        .map_err(|_| anyhow::anyhow!("无法创建更新下载客户端"))
}

fn allowed_redirect(url: &Url) -> bool {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    match url.host_str() {
        Some(
            "release-assets.githubusercontent.com"
            | "objects.githubusercontent.com"
            | "github-releases.githubusercontent.com",
        ) => true,
        Some("github.com") => url
            .path()
            .starts_with("/qzkinhit/codex-wiretap/releases/download/"),
        _ => false,
    }
}

fn select_asset(metadata: &GithubRelease, name: &str, limit: u64) -> Result<Option<Asset>> {
    let matching = metadata
        .assets
        .iter()
        .filter(|asset| asset.name == name)
        .collect::<Vec<_>>();
    ensure!(matching.len() <= 1, "正式版本包含重复的安装包或校验清单");
    let Some(asset) = matching.first() else {
        return Ok(None);
    };
    ensure!(
        asset.size > 0 && asset.size <= limit,
        "发布资产大小无效或超过下载限制"
    );
    let expected = format!("{RELEASE_ROOT}/download/{}/{name}", metadata.tag_name);
    ensure!(
        asset.browser_download_url == expected,
        "发布资产地址不符合固定仓库要求"
    );
    let url = Url::parse(&expected).map_err(|_| anyhow::anyhow!("发布资产地址格式无效"))?;
    let digest = match asset.digest.as_deref() {
        Some(value) if value.starts_with("sha256:") => Some(parse_hash(&value[7..])?),
        _ => None,
    };
    Ok(Some(Asset {
        name: name.into(),
        url,
        size: asset.size,
        digest,
    }))
}

fn parse_hash(raw: &str) -> Result<[u8; 32]> {
    ensure!(
        raw.len() == 64 && raw.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "SHA-256 校验信息格式无效"
    );
    let mut output = [0_u8; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&raw[index * 2..index * 2 + 2], 16)
            .map_err(|_| anyhow::anyhow!("SHA-256 校验信息格式无效"))?;
    }
    Ok(output)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn checksum_for(bytes: &[u8], filename: &str) -> Result<[u8; 32]> {
    let text = std::str::from_utf8(bytes).map_err(|_| anyhow::anyhow!("SHA256SUMS 格式无效"))?;
    let mut found = None;
    for line in text.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 || fields[1].trim_start_matches('*') != filename {
            continue;
        }
        ensure!(found.is_none(), "SHA256SUMS 包含重复的安装包校验项");
        found = Some(parse_hash(fields[0])?);
    }
    found.ok_or_else(|| anyhow::anyhow!("SHA256SUMS 未包含当前安装包的可靠校验项"))
}

async fn bounded_body(
    response: reqwest::Response,
    limit: u64,
    expected_size: Option<u64>,
) -> Result<Vec<u8>> {
    ensure!(
        response.content_length().is_none_or(|size| size <= limit),
        "更新元数据超过大小限制"
    );
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| anyhow::anyhow!("更新元数据读取失败"))?;
        ensure!(
            (bytes.len() as u64).saturating_add(chunk.len() as u64) <= limit,
            "更新元数据超过大小限制"
        );
        bytes.extend_from_slice(&chunk);
    }
    ensure!(
        expected_size.is_none_or(|size| bytes.len() as u64 == size),
        "更新资产长度与发布清单不一致"
    );
    Ok(bytes)
}

struct PartialDownload(Option<PathBuf>);

impl PartialDownload {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }
    fn path(&self) -> &Path {
        self.0
            .as_deref()
            .expect("partial path exists until committed")
    }
    fn committed(&mut self) {
        self.0 = None;
    }
    async fn cleanup(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

impl Drop for PartialDownload {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            // 请求取消时也清理未校验文件；不在异步执行线程上执行文件操作。
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = tokio::fs::remove_file(path).await;
                });
            } else {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{HeaderMap, StatusCode, Uri},
        response::Response,
        routing::get,
    };
    use serde_json::{Value, json};
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex as StdMutex},
    };
    use tempfile::{TempDir, tempdir};

    const VERSION: &str = "999.0.0";
    const TAG: &str = "v999.0.0";
    const PAYLOAD: &[u8] = b"fixture installer bytes, never executable";

    struct Fixture {
        origin: Url,
        routes: Arc<StdMutex<HashMap<String, Vec<u8>>>>,
        requests: Arc<StdMutex<Vec<(String, bool)>>>,
        task: tokio::task::JoinHandle<()>,
        directory: TempDir,
    }

    #[derive(Clone)]
    struct FixtureState {
        routes: Arc<StdMutex<HashMap<String, Vec<u8>>>>,
        requests: Arc<StdMutex<Vec<(String, bool)>>>,
    }

    async fn fixture_response(
        State(state): State<FixtureState>,
        uri: Uri,
        headers: HeaderMap,
    ) -> Response {
        state
            .requests
            .lock()
            .unwrap()
            .push((uri.path().into(), headers.contains_key("authorization")));
        let body = state.routes.lock().unwrap().get(uri.path()).cloned();
        match body {
            Some(body) => Response::new(Body::from(body)),
            None => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap(),
        }
    }

    impl Fixture {
        async fn new(metadata: Value) -> Self {
            let routes = Arc::new(StdMutex::new(HashMap::new()));
            routes.lock().unwrap().insert(
                Url::parse(LATEST_API).unwrap().path().into(),
                serde_json::to_vec(&metadata).unwrap(),
            );
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let router = Router::new()
                .fallback(get(fixture_response))
                .with_state(FixtureState {
                    routes: routes.clone(),
                    requests: requests.clone(),
                });
            let task = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            Self {
                origin,
                routes,
                requests,
                task,
                directory: tempdir().unwrap(),
            }
        }
        fn service(&self, platform: Option<&'static str>) -> UpdateService {
            let mut service = UpdateService::new(self.directory.path().into());
            service.asset_name = platform;
            service.fixture_origin = Some(self.origin.clone());
            service
        }
        fn asset(&self, name: &str, bytes: &[u8]) {
            self.routes.lock().unwrap().insert(
                format!("/qzkinhit/codex-wiretap/releases/download/{TAG}/{name}"),
                bytes.to_vec(),
            );
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn hash(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }
    fn asset(name: &str, bytes: &[u8], digest: bool) -> Value {
        json!({"name":name,"browser_download_url":format!("{RELEASE_ROOT}/download/{TAG}/{name}"),"size":bytes.len(),"digest":digest.then(|| format!("sha256:{}", hash(bytes)))})
    }
    fn metadata(assets: Vec<Value>) -> Value {
        json!({"tag_name":TAG,"draft":false,"prerelease":false,"html_url":format!("{RELEASE_ROOT}/tag/{TAG}"),"published_at":"2026-10-24T00:00:00Z","assets":assets})
    }

    #[test]
    fn versions_are_numeric_strict_and_cannot_contain_paths() {
        assert!(Version::parse("v1.10.0").unwrap() > Version::parse("1.9.99").unwrap());
        assert!(Version::parse("2.0.0").unwrap() > Version::parse("1.999.999").unwrap());
        for invalid in [
            "1.0",
            "1.0.0-beta",
            "1.0.0+build",
            "01.2.3",
            "../1.0.0",
            "/tmp/1.0.0",
            "1.0.0/../../x",
            "1.0.18446744073709551616",
        ] {
            assert!(Version::parse(invalid).is_err());
        }
    }

    #[tokio::test]
    async fn check_selects_platform_asset_and_caches_without_authorization() {
        let fixture = Fixture::new(metadata(vec![
            asset(MAC_ASSET, PAYLOAD, true),
            asset(WINDOWS_ASSET, PAYLOAD, true),
        ]))
        .await;
        let service = fixture.service(Some(MAC_ASSET));
        let info = service.check().await.unwrap();
        assert!(info.update_available && info.download_supported);
        assert_eq!(info.asset_name.as_deref(), Some(MAC_ASSET));
        assert_eq!(info.latest_version.as_deref(), Some(VERSION));
        service.check().await.unwrap();
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        assert!(
            fixture
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(_, auth)| !auth)
        );
        let windows = fixture.service(Some(WINDOWS_ASSET)).check().await.unwrap();
        assert_eq!(windows.asset_name.as_deref(), Some(WINDOWS_ASSET));
        let unsupported_service = fixture.service(None);
        let unsupported = unsupported_service.check().await.unwrap();
        assert!(unsupported.update_available && !unsupported.download_supported);
        assert!(unsupported.release_url.is_some());
        assert!(unsupported_service.download(VERSION).await.is_err());
    }

    #[tokio::test]
    async fn download_requires_a_checked_new_version_and_supported_verified_asset() {
        for assets in [vec![], vec![asset(MAC_ASSET, PAYLOAD, false)]] {
            let fixture = Fixture::new(metadata(assets)).await;
            let service = fixture.service(Some(MAC_ASSET));
            assert!(service.download(VERSION).await.is_err());
            assert!(!service.check().await.unwrap().download_supported);
            assert!(service.download(VERSION).await.is_err());
            assert!(!fixture.directory.path().join("updates").exists());
        }
        let mut old = metadata(vec![]);
        old["tag_name"] = json!("v0.0.0");
        old["html_url"] = json!(format!("{RELEASE_ROOT}/tag/v0.0.0"));
        let fixture = Fixture::new(old).await;
        let service = fixture.service(Some(MAC_ASSET));
        assert!(!service.check().await.unwrap().update_available);
        assert!(service.download("0.0.0").await.is_err());
    }

    #[tokio::test]
    async fn rejects_draft_prerelease_untrusted_urls_and_changed_version() {
        for field in ["draft", "prerelease"] {
            let mut release = metadata(vec![]);
            release[field] = json!(true);
            assert!(
                Fixture::new(release)
                    .await
                    .service(Some(MAC_ASSET))
                    .check()
                    .await
                    .is_err()
            );
        }
        for url in [
            "https://example.com/installer",
            "http://github.com/qzkinhit/codex-wiretap/releases/download/v999.0.0/PoolRouter-macOS-arm64.dmg",
            "https://github.com/other/repo/releases/download/v999.0.0/PoolRouter-macOS-arm64.dmg",
        ] {
            let mut installer = asset(MAC_ASSET, PAYLOAD, true);
            installer["browser_download_url"] = json!(url);
            let fixture = Fixture::new(metadata(vec![installer])).await;
            assert!(fixture.service(Some(MAC_ASSET)).check().await.is_err());
            assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        }
        let fixture = Fixture::new(metadata(vec![asset(MAC_ASSET, PAYLOAD, true)])).await;
        let service = fixture.service(Some(MAC_ASSET));
        service.check().await.unwrap();
        assert!(service.download("998.0.0").await.is_err());
        assert!(service.download("../../999.0.0").await.is_err());
        service.release.lock().await.as_mut().unwrap().checked_at = Instant::now() - CACHE_TTL;
        let mut changed = metadata(vec![]);
        changed["tag_name"] = json!("v1000.0.0");
        changed["html_url"] = json!(format!("{RELEASE_ROOT}/tag/v1000.0.0"));
        fixture.routes.lock().unwrap().insert(
            Url::parse(LATEST_API).unwrap().path().into(),
            serde_json::to_vec(&changed).unwrap(),
        );
        assert!(service.download(VERSION).await.is_err());
        assert!(!fixture.directory.path().join("updates").exists());
    }

    #[tokio::test]
    async fn correct_digest_saves_fixed_filename_and_wrong_hash_removes_partial() {
        for correct in [true, false] {
            let mut installer = asset(MAC_ASSET, PAYLOAD, true);
            if !correct {
                installer["digest"] = json!(format!("sha256:{}", "0".repeat(64)));
            }
            let fixture = Fixture::new(metadata(vec![installer])).await;
            fixture.asset(MAC_ASSET, PAYLOAD);
            let service = fixture.service(Some(MAC_ASSET));
            service.check().await.unwrap();
            let result = service.download(VERSION).await;
            let directory = fixture.directory.path().join("updates").join(VERSION);
            if correct {
                let downloaded = result.unwrap();
                assert_eq!(downloaded.filename, MAC_ASSET);
                assert_eq!(downloaded.path, directory.join(MAC_ASSET));
                assert_eq!(tokio::fs::read(&downloaded.path).await.unwrap(), PAYLOAD);
                assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
            } else {
                assert!(result.is_err());
                assert_eq!(std::fs::read_dir(directory).unwrap().count(), 0);
            }
            assert!(
                fixture
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(_, auth)| !auth)
            );
        }
    }

    #[tokio::test]
    async fn checksum_file_can_verify_asset_without_github_digest() {
        let sums = format!("{}  {MAC_ASSET}\n", hash(PAYLOAD));
        for checksum_digest in [true, false] {
            let fixture = Fixture::new(metadata(vec![
                asset(MAC_ASSET, PAYLOAD, false),
                asset(CHECKSUM_ASSET, sums.as_bytes(), checksum_digest),
            ]))
            .await;
            fixture.asset(MAC_ASSET, PAYLOAD);
            fixture.asset(CHECKSUM_ASSET, sums.as_bytes());
            let service = fixture.service(Some(MAC_ASSET));
            assert!(service.check().await.unwrap().download_supported);
            let downloaded = service.download(VERSION).await.unwrap();
            assert_eq!(tokio::fs::read(downloaded.path).await.unwrap(), PAYLOAD);
        }
    }

    #[tokio::test]
    async fn rejects_mismatched_download_length_without_leaving_partial() {
        let mut installer = asset(MAC_ASSET, PAYLOAD, true);
        installer["size"] = json!(PAYLOAD.len() + 1);
        let fixture = Fixture::new(metadata(vec![installer])).await;
        fixture.asset(MAC_ASSET, PAYLOAD);
        let service = fixture.service(Some(MAC_ASSET));
        service.check().await.unwrap();
        assert!(service.download(VERSION).await.is_err());
        let directory = fixture.directory.path().join("updates").join(VERSION);
        assert_eq!(std::fs::read_dir(directory).unwrap().count(), 0);
    }

    #[test]
    fn redirect_and_checksum_rules_do_not_accept_arbitrary_hosts_or_paths() {
        for url in [
            "http://release-assets.githubusercontent.com/file",
            "https://github.com.evil.invalid/file",
            "https://github.com/other/repository/file",
            "https://user:secret@release-assets.githubusercontent.com/file",
            "https://release-assets.githubusercontent.com:444/file",
            "file:///tmp/file",
        ] {
            assert!(!allowed_redirect(&Url::parse(url).unwrap()));
        }
        assert!(allowed_redirect(
            &Url::parse("https://release-assets.githubusercontent.com/file?signature=fixture")
                .unwrap()
        ));
        assert!(
            checksum_for(
                format!("{}  ../{MAC_ASSET}\n", hash(PAYLOAD)).as_bytes(),
                MAC_ASSET
            )
            .is_err()
        );
        assert!(
            checksum_for(
                format!(
                    "{}  {MAC_ASSET}\n{}  {MAC_ASSET}\n",
                    hash(PAYLOAD),
                    hash(PAYLOAD)
                )
                .as_bytes(),
                MAC_ASSET
            )
            .is_err()
        );
    }
}
