use crate::{biwrite, types::RoutingPolicy};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceMode {
    #[default]
    Biwrite,
    Manual,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub source_mode: SourceMode,
    pub upstream_base_url: Option<String>,
    pub biwrite_import_all: bool,
    pub sync_paused: bool,
    pub session_recovery_enabled: bool,
    pub compaction_repair_enabled: bool,
    pub compress_images: bool,
    pub first_output_timeout_ms: u64,
    pub listen: String,
    pub biwrite_dir: PathBuf,
    pub local_token: String,
    pub legacy_monitor_enabled: bool,
    pub poll_interval_ms: u64,
    pub policy: RoutingPolicy,
    pub model_override: Option<String>,
    /// Requested model -> ordered substitutes, used when the pool carries no
    /// account for the requested one. The first substitute the pool can serve
    /// wins, so a dead primary does not take the request down with it.
    pub model_aliases: BTreeMap<String, Vec<String>>,
    pub model_aliases_enabled: bool,
    /// Move to another account when the upstream's first streamed event names a
    /// shallower model or reasoning tier than the request asked for. Zero
    /// disables it. Bounded separately from the retry budget because a pool
    /// where every account downgrades must still answer.
    pub downgrade_retries: u32,
    /// After this many consecutive failed requests in one conversation, put a
    /// prompt on the clipboard that lets the user pick the work back up.
    /// Zero disables it.
    pub continuation_after_failures: u32,
    /// "new_window" asks the agent to carry on in a fresh conversation and
    /// names the thread to read; "current_window" only says to continue,
    /// because the thread is already open.
    pub continuation_mode: String,
    pub reasoning_override: Option<String>,
    /// Total upstream attempts, including the first request. The UI exposes retries.
    pub max_attempts: usize,
    pub queue_timeout_ms: u64,
    pub upstream_header_timeout_ms: u64,
    /// Start a second attempt on another account when the first has produced no
    /// response headers after this long. Zero disables it. Only ever started
    /// before any output exists, so the client can never see duplicated content.
    pub hedge_after_ms: u64,
    pub stream_idle_timeout_ms: u64,
    pub max_request_bytes: usize,
    pub recording_enabled: bool,
    pub record_limit: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            source_mode: SourceMode::Biwrite,
            upstream_base_url: None,
            biwrite_import_all: false,
            sync_paused: false,
            session_recovery_enabled: true,
            compaction_repair_enabled: true,
            compress_images: false,
            first_output_timeout_ms: 120_000,
            listen: "127.0.0.1:10813".into(),
            biwrite_dir: biwrite::discover_dir(),
            local_token: format!(
                "pr_{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            legacy_monitor_enabled: true,
            poll_interval_ms: 1500,
            policy: RoutingPolicy::default(),
            model_override: None,
            // The upstream this software is tested against answers 404 for
            // gpt-6-luna, which Codex requests once per new conversation.
            model_aliases: BTreeMap::from([(
                "gpt-6-luna".to_owned(),
                vec!["gpt-6.1-sol".to_owned(), "gpt-6-astra".to_owned()],
            )]),
            model_aliases_enabled: false,
            downgrade_retries: 2,
            continuation_after_failures: 0,
            continuation_mode: "new_window".to_owned(),
            reasoning_override: None,
            max_attempts: 16,
            queue_timeout_ms: 600_000,
            upstream_header_timeout_ms: 120_000,
            hedge_after_ms: 20_000,
            stream_idle_timeout_ms: 180_000,
            max_request_bytes: 64 * 1024 * 1024,
            recording_enabled: true,
            record_limit: 1000,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if let Some(upstream) = &self.upstream_base_url {
            crate::upstream::normalize_base_url(upstream)?;
        }
        let addr: SocketAddr = self
            .listen
            .parse()
            .map_err(|_| anyhow::anyhow!("监听地址格式错误"))?;
        if !addr.ip().is_loopback() || addr.port() == 0 {
            bail!("服务只能监听明确的本机回环地址和非零端口");
        }
        if self.local_token.len() < 32
            || self.local_token.len() > 256
            || !self.local_token.bytes().all(|c| c.is_ascii_graphic())
        {
            bail!("本地访问令牌必须为 32 至 256 个可打印 ASCII 字符");
        }
        if !(250..=60_000).contains(&self.poll_interval_ms)
            || !(1..=100).contains(&self.max_attempts)
            || !(1_000..=600_000).contains(&self.queue_timeout_ms)
            || !(1_000..=300_000).contains(&self.upstream_header_timeout_ms)
            || !(1_000..=300_000).contains(&self.first_output_timeout_ms)
            || !(1_000..=900_000).contains(&self.stream_idle_timeout_ms)
            || (self.hedge_after_ms != 0 && !(1_000..=600_000).contains(&self.hedge_after_ms))
            || !(1024..=128 * 1024 * 1024).contains(&self.max_request_bytes)
            || !(10..=10_000).contains(&self.record_limit)
        {
            bail!("时间、重试次数、记录数量或请求大小超出允许范围");
        }
        if self.continuation_after_failures > 100 {
            bail!("持续模式的失败次数超出允许范围");
        }
        if !matches!(
            self.continuation_mode.as_str(),
            "new_window" | "current_window"
        ) {
            bail!("持续模式的目标窗口无效");
        }
        if self.downgrade_retries > 8 {
            bail!("降级重试次数超出允许范围");
        }
        if self.model_aliases.len() > 32 {
            bail!("模型别名条目过多");
        }
        let valid = |name: &str| {
            !name.is_empty()
                && name.len() <= 100
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c))
        };
        for (from, targets) in &self.model_aliases {
            if !valid(from) || targets.is_empty() || targets.len() > 8 {
                bail!("模型别名的来源或候选数量无效");
            }
            if targets
                .iter()
                .any(|target| !valid(target) || target == from)
            {
                bail!("模型别名的候选无效或指向自身");
            }
        }
        if !(1..=64).contains(&self.policy.max_concurrency_per_key)
            || self
                .policy
                .shared_account_concurrency
                .is_some_and(|v| !(1..=128).contains(&v))
            || self
                .policy
                .account_overrides
                .values()
                .any(|v| v.max_concurrency.is_some_and(|n| !(1..=64).contains(&n)))
            || self.policy.cooldown_base_ms < 100
            || self.policy.cooldown_max_ms < self.policy.cooldown_base_ms
            || self.policy.cooldown_max_ms > 300_000
            || !(60..=604_800).contains(&self.policy.affinity_ttl_seconds)
        {
            bail!("并发、会话保持或冷却设置超出允许范围");
        }
        if self
            .model_override
            .as_deref()
            .is_some_and(|v| !safe_label(v))
        {
            bail!("模型名称格式无效");
        }
        if self.reasoning_override.as_deref().is_some_and(|v| {
            !matches!(
                v,
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
            )
        }) {
            bail!("推理强度无效");
        }
        if self
            .policy
            .pinned_key
            .as_deref()
            .is_some_and(|v| !valid_key_id(v))
            || self
                .policy
                .account_overrides
                .keys()
                .any(|v| !valid_key_id(v))
        {
            bail!("账号标识格式无效");
        }
        Ok(())
    }
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.listen)
    }
    pub fn selected_upstream(&self) -> Result<String> {
        match &self.upstream_base_url {
            Some(value) => crate::upstream::normalize_base_url(value),
            None if self.source_mode == SourceMode::Biwrite => {
                biwrite::discover_upstream(&self.biwrite_dir)
            }
            None => bail!(
                "手动号池需要明确的上游 Base URL；请在路由设置填写地址，或先成功同步自动来源再切换"
            ),
        }
    }
    pub fn public(&self) -> Value {
        serde_json::json!({"source_mode":self.source_mode,"upstream_base_url":self.upstream_base_url,"legacy_monitor_enabled":self.legacy_monitor_enabled,"policy":self.policy,"model_override":self.model_override,"model_aliases":self.model_aliases,"model_aliases_enabled":self.model_aliases_enabled,"downgrade_retries":self.downgrade_retries,"continuation_after_failures":self.continuation_after_failures,"continuation_mode":self.continuation_mode,
            "biwrite_import_all":self.biwrite_import_all,"sync_paused":self.sync_paused,
            "session_recovery_enabled":self.session_recovery_enabled,"compaction_repair_enabled":self.compaction_repair_enabled,
            "compress_images":self.compress_images,"first_output_timeout_ms":self.first_output_timeout_ms,
            "reasoning_override":self.reasoning_override,"max_attempts":self.max_attempts,
            "queue_timeout_ms":self.queue_timeout_ms,"upstream_header_timeout_ms":self.upstream_header_timeout_ms,
            "stream_idle_timeout_ms":self.stream_idle_timeout_ms,"hedge_after_ms":self.hedge_after_ms,"recording_enabled":self.recording_enabled,
            "poll_interval_ms":self.poll_interval_ms,"record_limit":self.record_limit})
    }
    pub fn patch(&self, patch: Value) -> Result<Self> {
        let Some(fields) = patch.as_object() else {
            bail!("设置必须是 JSON 对象");
        };
        let mut public = self.public();
        for (key, value) in fields {
            if public.get(key).is_none() {
                bail!("包含不可在线修改的设置字段");
            }
            if key == "policy" {
                let Some(p) = value.as_object() else {
                    bail!("路由设置必须是对象");
                };
                for (k, v) in p {
                    if public["policy"].get(k).is_none() {
                        bail!("路由设置字段无效");
                    }
                    public["policy"][k] = v.clone();
                }
            } else {
                public[key] = value.clone();
            }
        }
        let mut full = serde_json::to_value(self)?;
        for (k, v) in public.as_object().into_iter().flatten() {
            full[k] = v.clone();
        }
        let mut result: Self =
            serde_json::from_value(full).map_err(|_| anyhow::anyhow!("设置类型或值无效"))?;
        result.validate()?;
        result.upstream_base_url = result
            .upstream_base_url
            .as_deref()
            .map(crate::upstream::normalize_base_url)
            .transpose()?;
        Ok(result)
    }
}

pub fn safe_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.contains("sk-")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:/".contains(&b))
}
pub fn valid_key_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn environment_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn preferred_home(
    home: Option<PathBuf>,
    profile: Option<PathBuf>,
    windows: bool,
) -> Option<PathBuf> {
    if windows { profile.or(home) } else { home }
}

/// Windows desktop processes normally provide USERPROFILE rather than HOME.
pub fn user_home_dir() -> Option<PathBuf> {
    preferred_home(
        environment_path("HOME"),
        environment_path("USERPROFILE"),
        cfg!(target_os = "windows"),
    )
    .filter(|path| path.is_absolute())
}

fn codex_home_from(override_dir: Option<PathBuf>, user_dir: Option<PathBuf>) -> Option<PathBuf> {
    override_dir.or_else(|| user_dir.map(|path| path.join(".codex")))
}

/// Both integration and the title index must use the same Codex data directory.
pub fn codex_home_dir() -> Option<PathBuf> {
    let path = codex_home_from(environment_path("CODEX_HOME"), user_home_dir())?;
    if path.is_absolute() {
        Some(path)
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

#[cfg(any(target_os = "windows", test))]
fn windows_config_path(local_data: Option<PathBuf>, user_dir: Option<PathBuf>) -> PathBuf {
    local_data
        .or_else(|| user_dir.map(|path| path.join("AppData/Local")))
        .unwrap_or_default()
        .join("Pool Router/config.json")
}

pub fn default_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        user_home_dir()
            .unwrap_or_default()
            .join("Library/Application Support/Pool Router/config.json")
    }
    #[cfg(target_os = "windows")]
    {
        windows_config_path(
            environment_path("LOCALAPPDATA").filter(|path| path.is_absolute()),
            user_home_dir(),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        user_home_dir()
            .unwrap_or_default()
            .join(".config/pool-router/config.json")
    }
}

pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("缺少配置目录"))?;
    #[cfg(unix)]
    let created_parent = !parent.exists();
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if created_parent {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    let temp = parent.join(format!(
        ".pool-router-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut f = options.open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

pub fn save(path: &Path, config: &Config) -> Result<()> {
    config.validate()?;
    private_write(path, &serde_json::to_vec_pretty(config)?)
}
pub fn load(path: &Path) -> Result<Config> {
    let bytes = fs::read(path).map_err(|_| anyhow::anyhow!("无法读取 Pool Router 配置"))?;
    if bytes.len() > 1024 * 1024 {
        bail!("Pool Router 配置文件过大");
    }
    let config: Config =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Pool Router 配置格式无效"))?;
    config.validate()?;
    Ok(config)
}
pub fn load_or_init(path: &Path) -> Result<Config> {
    if path.exists() {
        load(path)
    } else {
        let c = Config::default();
        save(path, &c)?;
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn platform_paths_prefer_windows_profile_and_keep_codex_override() {
        let home = PathBuf::from("home-from-shell");
        let profile = PathBuf::from("windows-profile");
        assert_eq!(
            preferred_home(Some(home.clone()), Some(profile.clone()), true),
            Some(profile.clone())
        );
        assert_eq!(
            preferred_home(None, Some(profile.clone()), true),
            Some(profile.clone())
        );
        assert_eq!(
            preferred_home(Some(home.clone()), None, true),
            Some(home.clone())
        );
        assert_eq!(
            preferred_home(Some(home.clone()), Some(profile.clone()), false),
            Some(home.clone())
        );
        assert_eq!(preferred_home(None, Some(profile), false), None);
        let custom = PathBuf::from("custom-codex");
        assert_eq!(
            codex_home_from(Some(custom.clone()), Some(home.clone())),
            Some(custom)
        );
        assert_eq!(
            codex_home_from(None, Some(home.clone())),
            Some(home.join(".codex"))
        );
        assert_eq!(codex_home_from(None, None), None);
    }

    #[test]
    fn windows_config_uses_local_app_data_with_profile_fallback() {
        let local = PathBuf::from("local-app-data");
        let profile = PathBuf::from("windows-profile");
        assert_eq!(
            windows_config_path(Some(local.clone()), Some(profile.clone())),
            local.join("Pool Router/config.json")
        );
        assert_eq!(
            windows_config_path(None, Some(profile.clone())),
            profile.join("AppData/Local/Pool Router/config.json")
        );
    }

    #[test]
    fn manual_source_without_address_never_adopts_biwrite_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), serde_json::json!({
            "providers":[{"id":"fixture", "kind":"openai_compatible", "wireApi":"responses", "hasKey":true, "baseUrl":"https://api.example.com/v1"}]
        }).to_string()).unwrap();
        let config = Config {
            source_mode: SourceMode::Manual,
            biwrite_dir: dir.path().to_owned(),
            ..Config::default()
        };
        assert!(config.selected_upstream().is_err());
        assert_eq!(
            biwrite::discover_upstream(dir.path()).unwrap(),
            "https://api.example.com/v1"
        );
    }
    #[test]
    fn upstream_is_explicit_or_discovered_and_invalid_values_never_echo() {
        let c = Config::default();
        assert!(c.upstream_base_url.is_none());
        let updated = c
            .patch(serde_json::json!({"upstream_base_url":"https://API.EXAMPLE.COM:443/"}))
            .unwrap();
        assert_eq!(
            updated.selected_upstream().unwrap(),
            "https://api.example.com/v1"
        );
        assert_eq!(
            updated.public()["upstream_base_url"],
            "https://api.example.com/v1"
        );
        assert!(
            updated
                .patch(serde_json::json!({"upstream_base_url":null}))
                .unwrap()
                .upstream_base_url
                .is_none()
        );
        let secret = "sk-invalid-upstream-fixture";
        let invalid = c.patch(
            serde_json::json!({"upstream_base_url":format!("https://{secret}@api.example.com/v1")}),
        );
        assert!(!invalid.err().unwrap().to_string().contains(secret));
    }
    #[test]
    fn secret_never_public_and_patch_cannot_replace() {
        let c = Config::default();
        assert!(!c.public().to_string().contains(&c.local_token));
        assert!(
            c.patch(serde_json::json!({"local_token":"secret"}))
                .is_err()
        );
    }
    #[test]
    fn rejects_network_listener() {
        let c = Config {
            listen: "0.0.0.0:10813".into(),
            ..Config::default()
        };
        assert!(c.validate().is_err());
    }
    #[test]
    fn partial_policy_patch_preserves_others() {
        let c = Config::default();
        let n = c
            .patch(serde_json::json!({"policy":{"strategy":"random"}}))
            .unwrap();
        assert_eq!(n.policy.max_concurrency_per_key, 1);
        assert_eq!(n.policy.strategy, crate::types::Strategy::Random);
    }
    #[test]
    fn legacy_monitor_defaults_on_without_changing_pool_source() {
        let c: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(c.legacy_monitor_enabled);
        assert!(matches!(c.source_mode, SourceMode::Biwrite));
        assert!(
            !c.patch(serde_json::json!({"legacy_monitor_enabled":false}))
                .unwrap()
                .legacy_monitor_enabled
        );
    }
}
