#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "windows")]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if !windows::entry().await {
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("Pool Router 图形启动器仅适用于 Windows；请使用 pool-router 命令或 macOS 启动器。");
    std::process::exit(1);
}

#[cfg(any(target_os = "windows", test))]
mod common {
    use anyhow::{Context, Result, bail};
    use std::{
        ffi::OsString,
        fs::{File, OpenOptions, TryLockError},
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    pub struct Options {
        pub background: bool,
        pub config_path: Option<PathBuf>,
    }

    pub fn parse_arguments(arguments: impl IntoIterator<Item = OsString>) -> Result<Options> {
        let mut arguments = arguments.into_iter();
        let mut options = Options {
            background: false,
            config_path: None,
        };
        while let Some(argument) = arguments.next() {
            if argument == "--background" && !options.background {
                options.background = true;
            } else if argument == "--config" && options.config_path.is_none() {
                let path = arguments.next().context("--config 缺少路径")?;
                if path.is_empty() {
                    bail!("--config 路径不能为空");
                }
                options.config_path = Some(PathBuf::from(path));
            } else {
                bail!("用法：pool-router-launcher.exe [--background] [--config 路径]");
            }
        }
        Ok(options)
    }

    /// The OS releases the file lock when a launcher exits or crashes. Keep the
    /// file itself in place so concurrent launchers always lock the same inode.
    pub async fn startup_lock(path: &Path, timeout: Duration) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .context("无法打开后台启动锁")?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) if started.elapsed() < timeout => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(TryLockError::WouldBlock) => {
                    bail!("另一个启动器仍在启动后台服务，请稍后重试");
                }
                Err(TryLockError::Error(_)) => bail!("无法取得后台启动锁"),
            }
        }
    }

    pub async fn healthy(client: &reqwest::Client, listen: &str) -> bool {
        let Ok(response) = client.get(format!("http://{listen}/health")).send().await else {
            return false;
        };
        if !response.status().is_success() {
            return false;
        }
        let Ok(value) = response.json::<serde_json::Value>().await else {
            return false;
        };
        value["service"].as_str() == Some("pool-router") && value["status"].as_str() == Some("ok")
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        #[test]
        fn parses_background_and_config_without_shell_interpretation() {
            let path = OsString::from(r#"C:\Users\Test & User\Pool "Router"\config.json"#);
            let options = parse_arguments([
                OsString::from("--background"),
                OsString::from("--config"),
                path.clone(),
            ])
            .unwrap();
            assert!(options.background);
            assert_eq!(options.config_path, Some(PathBuf::from(path)));
            let defaults = parse_arguments([]).unwrap();
            assert!(!defaults.background);
            assert!(defaults.config_path.is_none());
            assert!(parse_arguments([OsString::from("--config")]).is_err());
            assert!(parse_arguments([OsString::from("--unknown-secret")]).is_err());
            let error = parse_arguments([OsString::from("--unknown-secret")])
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains("--unknown-secret"));
        }

        #[tokio::test]
        async fn startup_lock_serializes_launches_and_releases_on_drop() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("startup.lock");
            let first = startup_lock(&path, Duration::ZERO).await.unwrap();
            assert!(startup_lock(&path, Duration::ZERO).await.is_err());
            drop(first);
            let second = startup_lock(&path, Duration::ZERO).await.unwrap();
            drop(second);
        }

        #[tokio::test]
        async fn health_requires_the_expected_service_and_status() {
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            for (body, expected) in [
                (r#"{"service":"pool-router","status":"ok"}"#, true),
                (r#"{"service":"another-program","status":"ok"}"#, false),
                (r#"{"service":"pool-router","status":"starting"}"#, false),
            ] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = [0; 2048];
                    let _ = stream.read(&mut request).await.unwrap();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                });
                assert_eq!(healthy(&client, &address.to_string()).await, expected);
                server.await.unwrap();
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::common::{healthy, parse_arguments, startup_lock};
    use anyhow::{Context, Result, bail, ensure};
    use pool_router::config;
    use std::{
        fs::{self, File, OpenOptions},
        io::Write,
        net::SocketAddr,
        os::windows::process::CommandExt,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub async fn entry() -> bool {
        let options = match parse_arguments(std::env::args_os().skip(1)) {
            Ok(options) => options,
            Err(error) => {
                report_failure(&config::default_path(), &error.to_string());
                return false;
            }
        };
        let using_default = options.config_path.is_none();
        let selected_path = options.config_path.unwrap_or_else(config::default_path);
        if using_default && !selected_path.is_absolute() {
            report_failure(
                &selected_path,
                "无法确定用户数据目录，请检查 LOCALAPPDATA 或 USERPROFILE",
            );
            return false;
        }
        let config_path = if selected_path.is_absolute() {
            selected_path
        } else if let Ok(directory) = std::env::current_dir() {
            directory.join(selected_path)
        } else {
            report_failure(&selected_path, "无法确定配置文件的完整路径");
            return false;
        };
        if let Err(error) = launch(&config_path, options.background).await {
            report_failure(&config_path, &error.to_string());
            return false;
        }
        true
    }

    fn append_log(root: &Path, name: &str) -> Result<File> {
        let logs = root.join("logs");
        fs::create_dir_all(&logs).context("无法创建日志目录")?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs.join(name))
            .context("无法打开日志文件")
    }

    fn hidden_command(binary: &Path, root: &Path, log: File) -> Result<Command> {
        let mut command = Command::new(binary);
        command
            .creation_flags(CREATE_NO_WINDOW)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(log.try_clone().context("无法复制日志句柄")?)
            .stderr(log);
        Ok(command)
    }

    async fn launch(config_path: &Path, background: bool) -> Result<()> {
        let root = config_path.parent().context("配置文件缺少父目录")?;
        fs::create_dir_all(root).context("无法创建 Pool Router 数据目录")?;
        let lock = startup_lock(&root.join("launcher.lock"), Duration::from_secs(45)).await?;
        // Initialize only after taking the lock, otherwise concurrent launches
        // could create different local tokens for the same configuration file.
        let cfg = config::load_or_init(config_path).context("无法加载 Pool Router 配置")?;
        let binary = std::env::current_exe()
            .context("无法确定启动器所在目录")?
            .parent()
            .context("启动器缺少父目录")?
            .join("pool-router.exe");
        ensure!(
            binary.is_file(),
            "安装不完整，缺少 pool-router.exe，请重新安装"
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(1))
            .build()
            .context("无法创建本地健康检查客户端")?;
        if !healthy(&client, &cfg.listen).await {
            let address: SocketAddr = cfg.listen.parse().context("本地监听地址无效")?;
            if matches!(
                tokio::time::timeout(
                    Duration::from_secs(1),
                    tokio::net::TcpStream::connect(address)
                )
                .await,
                Ok(Ok(_))
            ) {
                bail!("本地端口已被其他程序占用或服务尚未就绪；请检查后重试");
            }
            let mut child = hidden_command(&binary, root, append_log(root, "service.log")?)?
                .arg("serve")
                .arg("--config")
                .arg(config_path)
                .spawn()
                .context("无法启动 Pool Router 后台服务")?;
            let started = Instant::now();
            loop {
                if healthy(&client, &cfg.listen).await {
                    break;
                }
                if let Some(status) = child.try_wait().context("无法检查后台服务进程")? {
                    bail!("后台服务提前退出（{status}），请查看 logs/service.log");
                }
                if started.elapsed() >= Duration::from_secs(30) {
                    bail!("后台服务未在 30 秒内就绪，请查看 logs/service.log");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            // Dropping our Child handle leaves the user's healthy service running.
            drop(child);
        }
        drop(lock);
        if !background {
            let status = hidden_command(&binary, root, append_log(root, "launcher.log")?)?
                .arg("open")
                .arg("--config")
                .arg(config_path)
                .status()
                .context("无法打开 Pool Router 控制台")?;
            ensure!(status.success(), "控制台未能打开，请查看 logs/launcher.log");
        }
        Ok(())
    }

    fn report_failure(config_path: &Path, error: &str) {
        let root = config_path.parent().unwrap_or_else(|| Path::new("."));
        let log_path = root.join("logs/launcher.log");
        if let Ok(mut log) = append_log(root, "launcher.log") {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or_default();
            let _ = writeln!(log, "[{timestamp}] {error}");
        }
        // A fixed script reads plain message text from an environment variable.
        // Configuration contents and tokens are never placed in logs or arguments.
        let message = format!("{error}\n\n日志位置：{}", log_path.display());
        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let powershell = system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let shown = Command::new(powershell)
            .creation_flags(CREATE_NO_WINDOW)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Add-Type -AssemblyName PresentationFramework; [System.Windows.MessageBox]::Show($env:POOL_ROUTER_LAUNCH_ERROR, 'Pool Router', 'OK', 'Error') | Out-Null",
            ])
            .env("POOL_ROUTER_LAUNCH_ERROR", message)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !shown {
            let _ = Command::new(system_root.join("System32/notepad.exe"))
                .arg(log_path)
                .spawn();
        }
    }
}
