use anyhow::{Result, bail};
use pool_router::{
    config::{self, Config},
    integration,
    proxy::{self, AppState},
};
use std::{path::PathBuf, time::Duration};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Pool Router：{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut command = "serve".to_string();
    let mut config_path = config::default_path();
    let mut saw_command = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config_path = PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--config 缺少路径"))?,
                )
            }
            "--version" | "-V" => {
                println!("Pool Router {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "serve" | "init" | "open" | "connect" | "restore" | "status" if !saw_command => {
                command = arg;
                saw_command = true;
            }
            _ => bail!("参数无效；运行 pool-router --help 查看用法"),
        }
    }
    if !config_path.is_absolute() {
        config_path = std::env::current_dir()?.join(config_path);
    }
    let cfg = config::load_or_init(&config_path)?;
    match command.as_str() {
        "init" => {
            println!("配置已就绪：{}", config_path.display());
            println!("本地入口：{}", cfg.base_url());
        }
        "restore" => {
            println!(
                "{}",
                integration::restore(
                    config_path
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("配置目录无效"))?
                )?
            );
        }
        "connect" => {
            ensure_service(&cfg).await?;
            println!(
                "{}",
                integration::connect(
                    &cfg.base_url(),
                    &cfg.local_token,
                    config_path
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("配置目录无效"))?,
                    u32::try_from(cfg.max_attempts.saturating_sub(1).max(1)).unwrap_or(15),
                    u32::try_from(cfg.queue_timeout_ms).unwrap_or(600_000),
                )?
            );
        }
        "status" => {
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()?;
            let response = client
                .get(format!("http://{}/api/state", cfg.listen))
                .bearer_auth(&cfg.local_token)
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("本地服务尚未启动"))?;
            if !response.status().is_success() {
                bail!("本地服务认证失败或状态不可用");
            }
            let value: serde_json::Value = response.json().await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        "open" => {
            ensure_service(&cfg).await?;
            let url = format!("http://{}/#token={}", cfg.listen, cfg.local_token);
            #[cfg(target_os = "macos")]
            let mut opener = std::process::Command::new("open");
            #[cfg(target_os = "windows")]
            let mut opener = {
                use std::os::windows::process::CommandExt;
                use std::process::Stdio;
                let system_root = std::env::var_os("SystemRoot")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
                let mut c = std::process::Command::new(system_root.join("System32/rundll32.exe"));
                c.arg("url.dll,FileProtocolHandler");
                c.creation_flags(0x0800_0000)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                c
            };
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let mut opener = std::process::Command::new("xdg-open");
            #[cfg(target_os = "windows")]
            {
                // The shell URL handler may stay alive for the lifetime of the
                // browser or its first-run UI. Opening the dashboard only hands
                // the URL to that handler; it must not wait for the browser to exit.
                drop(opener.arg(url).spawn()?);
                println!("已请求系统浏览器打开本地号池控制台");
            }
            #[cfg(not(target_os = "windows"))]
            {
                if !opener.arg(url).status()?.success() {
                    bail!("无法打开浏览器");
                }
                println!("已打开本地号池控制台");
            }
        }
        _ => serve(cfg, config_path).await?,
    }
    Ok(())
}

async fn ensure_service(cfg: &Config) -> Result<()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()?;
    for _ in 0..20 {
        if let Ok(r) = client
            .get(format!("http://{}/health", cfg.listen))
            .send()
            .await
            && r.status().is_success()
            && let Ok(body) = r.json::<serde_json::Value>().await
            && body["service"].as_str() == Some("pool-router")
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("本地服务尚未启动；请先运行 pool-router serve 或安装 macOS 后台服务")
}

async fn serve(cfg: Config, path: PathBuf) -> Result<()> {
    let addr = cfg.listen.clone();
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|_| anyhow::anyhow!("无法监听 {addr}，请检查端口占用"))?;
    let state = AppState::new(cfg, path, true)?;
    // Populate the in-memory pool once on startup, then honor the saved pause
    // setting for periodic reads. A paused restart must not create an empty pool.
    if state.sync_source_startup().await.is_err() {
        eprintln!("BiWrite 号池暂未读取成功，控制台可查看原因，服务会继续重试");
    }
    let weak = std::sync::Arc::downgrade(&state);
    let sync_task = tokio::spawn(async move {
        loop {
            let Some(s) = weak.upgrade() else {
                break;
            };
            let ms = s.settings().poll_interval_ms;
            let _ = s.sync_source().await;
            drop(s);
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    });
    let usage_weak = std::sync::Arc::downgrade(&state);
    let usage_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let Some(s) = usage_weak.upgrade() else {
                break;
            };
            let dir = s.state_dir();
            let _ = tokio::task::spawn_blocking(move || s.usage.flush(&dir)).await;
        }
    });
    println!("Pool Router {} 已启动：{}", env!("CARGO_PKG_VERSION"), addr);
    println!("运行 pool-router open 打开控制台；默认自动读取 BiWrite，也可切换手动号池");
    let closing = state.clone();
    let server = axum::serve(listener, proxy::router(state)).with_graceful_shutdown(shutdown());
    let result = server.await;
    sync_task.abort();
    usage_task.abort();
    // The timer may have slept through the last requests of this run.
    let _ = closing.usage.flush(&closing.state_dir());
    result?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        if let Ok(mut term) = term {
            tokio::select! {_ = tokio::signal::ctrl_c()=>{},_ = term.recv()=>{}}
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn print_help() {
    println!(
        "Pool Router：BiWrite 号池同步、Codex 调度和监控\n\n用法：pool-router [命令] [--config 路径]\n\n  serve    启动本地服务（默认）\n  init     生成私有本地配置\n  open     打开控制台\n  status   输出脱敏状态\n  connect  备份并直接接入 Codex\n  restore  检查冲突并恢复接入前配置\n\nmacOS 可使用 scripts/macos-install.sh 安装常驻服务和桌面启动器。"
    );
}
