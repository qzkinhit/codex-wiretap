use crate::{
    biwrite,
    config::{self, Config, SourceMode},
    conversations::ConversationIndex,
    integration, manual,
    monitor::{
        self, AttemptRecord, GatewayAction, Observation, RecordStore, RequestRecord, SseObserver,
    },
    recovery::{self, RecoveryKind, SessionRemaps},
    scheduler::{Scheduler, SelectError, Selection},
    types::{ProviderSummary, RoutingPolicy, SourceSnapshot, now_ms},
};
use anyhow::Result;
use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{self, Read},
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Default, Serialize)]
pub struct SourceState {
    pub read_at_ms: u64,
    pub last_error: Option<String>,
    pub warnings: Vec<String>,
    pub providers: Vec<ProviderSummary>,
}

#[derive(Default)]
pub struct Counters {
    requests: AtomicU64,
    successes: AtomicU64,
    failures: AtomicU64,
    active: AtomicU64,
    waiting_streams: AtomicU64,
}

pub struct AppState {
    pub config: RwLock<Config>,
    pub config_path: PathBuf,
    pub scheduler: Scheduler,
    pub records: RecordStore,
    /// Local display metadata only. Titles never enter the persisted request log.
    pub conversations: Arc<ConversationIndex>,
    pub source: RwLock<SourceState>,
    pub client: reqwest::Client,
    legacy_client: reqwest::Client,
    pub started: Instant,
    pub counters: Counters,
    /// Optional connection metadata supplied by embedders or isolated tests.
    pub connection_override: RwLock<Option<integration::ConnectionInfo>>,
    /// Rust-only injection for isolated transport tests. Never exposed through config or APIs.
    pub legacy_upstream_override: RwLock<Option<String>>,
    resolved_upstream: RwLock<Option<String>>,
    session_remaps: SessionRemaps,
    settings_lock: tokio::sync::Mutex<()>,
    source_lock: tokio::sync::Mutex<()>,
}

impl AppState {
    pub fn new(config: Config, config_path: PathBuf, persist_records: bool) -> Result<Arc<Self>> {
        config.validate()?;
        let data_path = persist_records.then(|| {
            config_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("records.jsonl")
        });
        let records = RecordStore::new(data_path, config.record_limit)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;
        let legacy_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .no_gzip()
            .no_deflate()
            .no_zstd()
            .build()?;
        Ok(Arc::new(Self {
            config: RwLock::new(config),
            config_path,
            scheduler: Scheduler::new(),
            records,
            conversations: Arc::new(ConversationIndex::from_environment()),
            source: RwLock::new(SourceState::default()),
            client,
            legacy_client,
            started: Instant::now(),
            counters: Counters::default(),
            connection_override: RwLock::new(None),
            legacy_upstream_override: RwLock::new(None),
            resolved_upstream: RwLock::new(None),
            session_remaps: SessionRemaps::default(),
            settings_lock: tokio::sync::Mutex::new(()),
            source_lock: tokio::sync::Mutex::new(()),
        }))
    }
    pub fn settings(&self) -> Config {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub async fn sync_source(&self) -> Result<()> {
        self.sync_source_with_force(false).await
    }
    pub async fn sync_source_now(&self) -> Result<()> {
        self.sync_source_with_force(true).await
    }
    async fn sync_source_with_force(&self, force: bool) -> Result<()> {
        let _lock = self.source_lock.lock().await;
        let cfg = self.settings();
        if cfg.source_mode == SourceMode::Biwrite && cfg.sync_paused && !force {
            return Ok(());
        }
        let result = self.read_source(&cfg).await;
        let current = self.settings();
        if cfg.source_mode != current.source_mode
            || cfg.upstream_base_url != current.upstream_base_url
            || cfg.biwrite_import_all != current.biwrite_import_all
        {
            return Ok(());
        }
        match result {
            Ok((snapshot, upstream)) => {
                *self
                    .resolved_upstream
                    .write()
                    .unwrap_or_else(|e| e.into_inner()) = Some(upstream);
                self.scheduler.sync_accounts(snapshot.accounts);
                *self.source.write().unwrap_or_else(|e| e.into_inner()) = SourceState {
                    read_at_ms: snapshot.read_at_ms,
                    last_error: None,
                    warnings: snapshot.warnings,
                    providers: snapshot.providers,
                };
                Ok(())
            }
            Err(e) => {
                let mut source = self.source.write().unwrap_or_else(|e| e.into_inner());
                source.last_error = Some(e.to_string());
                if now_ms().saturating_sub(source.read_at_ms) > 30_000 {
                    self.scheduler.sync_accounts(Vec::new());
                }
                Err(e)
            }
        }
    }
    fn state_dir(&self) -> PathBuf {
        self.config_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf()
    }
    async fn read_source(&self, cfg: &Config) -> Result<(SourceSnapshot, String)> {
        let mode = cfg.source_mode;
        let dir = if mode == SourceMode::Biwrite {
            cfg.biwrite_dir.clone()
        } else {
            self.state_dir()
        };
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || {
            let upstream = cfg.selected_upstream()?;
            let snapshot = match mode {
                SourceMode::Biwrite => {
                    biwrite::load_snapshot_with_scope(&dir, &upstream, cfg.biwrite_import_all)
                }
                SourceMode::Manual => manual::load_snapshot(&dir, &upstream),
            }?;
            Ok((snapshot, upstream))
        })
        .await?
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [("content-type", "text/javascript; charset=utf-8")],
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [("content-type", "text/css; charset=utf-8")],
                    include_str!("../web/style.css"),
                )
            }),
        )
        .route(
            "/health",
            get(|| async { Json(json!({"status":"ok","service":"pool-router"})) }),
        )
        .route("/api/state", get(api_state))
        .route("/api/records", get(api_records))
        .route("/api/records/clear", post(api_clear))
        .route("/api/config", post(api_config))
        .route("/api/sync", post(api_sync))
        .route("/api/accounts/{id}/reset", post(api_reset))
        .route("/api/probe", post(api_probe))
        .route("/api/connect", post(api_connect))
        .route("/api/legacy/connect", post(api_legacy_connect))
        .route("/api/legacy/restore", post(api_legacy_restore))
        .route("/api/manual", get(api_manual_list).post(api_manual_add))
        .route("/api/manual/{id}", delete(api_manual_delete))
        .route("/v1/models", get(api_models))
        .route("/v1/responses", post(proxy_handler).get(ws_unavailable))
        .route("/v1/responses/compact", post(proxy_handler))
        .layer(DefaultBodyLimit::max(128 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            access_control,
        ))
        .with_state(state)
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error":{"type":"pool_router_error","code":code,"message":message}})),
    )
        .into_response()
}

fn same_token(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn valid_legacy_key(key: &str) -> bool {
    key.starts_with("sk-")
        && (16..=1024).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn legacy_key<'a>(headers: &'a HeaderMap, cfg: &Config) -> Option<&'a str> {
    let key = bearer(headers)?;
    (cfg.legacy_monitor_enabled
        && !same_token(key.as_bytes(), cfg.local_token.as_bytes())
        && valid_legacy_key(key))
    .then_some(key)
}

fn legacy_path(path: &str) -> bool {
    matches!(path, "/v1/responses" | "/v1/responses/compact")
}

async fn access_control(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let cfg = state.settings();
    let port = cfg
        .listen
        .parse::<std::net::SocketAddr>()
        .map(|a| a.port())
        .unwrap_or(10813);
    let host_valid = |host: &str| {
        let Ok(u) = reqwest::Url::parse(&format!("http://{host}")) else {
            return false;
        };
        matches!(
            u.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
        ) && u.port_or_known_default() == Some(port)
            && u.username().is_empty()
            && u.password().is_none()
            && u.path() == "/"
    };
    if !request
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .is_some_and(host_valid)
    {
        return error(
            StatusCode::FORBIDDEN,
            "invalid_host",
            "仅接受发往本机服务地址的请求",
        );
    }
    if let Some(origin) = request.headers().get("origin") {
        let valid = origin
            .to_str()
            .ok()
            .and_then(|v| reqwest::Url::parse(v).ok())
            .is_some_and(|u| {
                u.scheme() == "http"
                    && u.port_or_known_default() == Some(port)
                    && matches!(
                        u.host_str(),
                        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
                    )
            });
        if !valid {
            return error(StatusCode::FORBIDDEN, "invalid_origin", "浏览器来源不允许");
        }
    }
    let path = request.uri().path();
    if path.starts_with("/api/") || path.starts_with("/v1/") {
        let provided = bearer(request.headers()).unwrap_or("");
        let legacy = request.method() == axum::http::Method::POST
            && legacy_path(path)
            && legacy_key(request.headers(), &cfg).is_some();
        if !same_token(provided.as_bytes(), cfg.local_token.as_bytes()) && !legacy {
            return error(
                StatusCode::UNAUTHORIZED,
                "local_auth_required",
                "请通过 Pool Router 启动器打开面板，或配置本地访问令牌",
            );
        }
    }
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert("cache-control", HeaderValue::from_static("no-store"));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("content-security-policy",HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    response
}

async fn api_state(State(state): State<Arc<AppState>>) -> Json<Value> {
    let cfg = state.settings();
    let accounts = state.scheduler.snapshot(&cfg.policy);
    let mut models: Vec<String> = accounts
        .iter()
        .filter(|a| a.present)
        .flat_map(|a| a.models.clone())
        .collect();
    models.sort();
    models.dedup();
    let base = cfg.base_url();
    let supplied_connection = state
        .connection_override
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let connection = if supplied_connection.is_some() {
        supplied_connection
    } else {
        tokio::task::spawn_blocking(move || integration::inspect_connection(&base))
            .await
            .ok()
    };
    Json(
        json!({"version":env!("CARGO_PKG_VERSION"),"uptime_seconds":state.started.elapsed().as_secs(),
        "source":state.source.read().unwrap_or_else(|e|e.into_inner()).clone(),"accounts":accounts,"models":models,"config":cfg.public(),
        "connection":connection,"stats":{"requests":state.counters.requests.load(Ordering::Relaxed),"successes":state.counters.successes.load(Ordering::Relaxed),"failures":state.counters.failures.load(Ordering::Relaxed),"active":state.counters.active.load(Ordering::Relaxed),"waiting_streams":state.counters.waiting_streams.load(Ordering::Relaxed)}}),
    )
}
async fn api_records(State(s): State<Arc<AppState>>) -> Json<Value> {
    let records = s.records.records();
    let ids = records
        .iter()
        .filter_map(|record| record.conversation_id.clone())
        .collect::<Vec<_>>();
    let index = s.conversations.clone();
    let titles = tokio::task::spawn_blocking(move || index.lookup(&ids))
        .await
        .unwrap_or_default();
    #[derive(Serialize)]
    struct DisplayRecord {
        #[serde(flatten)]
        record: RequestRecord,
        conversation_title: Option<String>,
    }
    let records = records
        .into_iter()
        .map(|record| DisplayRecord {
            conversation_title: record
                .conversation_id
                .as_ref()
                .and_then(|id| titles.get(id))
                .cloned(),
            record,
        })
        .collect::<Vec<_>>();
    Json(json!({"records":records}))
}
async fn api_clear(State(s): State<Arc<AppState>>) -> Response {
    match s.records.clear() {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "record_write_failed",
            "清理本地记录失败",
        ),
    }
}
async fn api_config(State(s): State<Arc<AppState>>, Json(patch): Json<Value>) -> Response {
    let _lock = s.settings_lock.lock().await;
    let _source_lock = s.source_lock.lock().await;
    let previous_config = s.settings();
    let mut updated = match previous_config.patch(patch) {
        Ok(c) => c,
        Err(e) => return error(StatusCode::BAD_REQUEST, "invalid_config", &e.to_string()),
    };
    // A manual pool can keep working after BiWrite closes or its files move.
    // Persist the address already selected for automatic routing when switching.
    if updated.source_mode == SourceMode::Manual
        && previous_config.source_mode != SourceMode::Manual
        && updated.upstream_base_url.is_none()
    {
        updated.upstream_base_url = s
            .resolved_upstream
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
    }
    let source_changed = previous_config.source_mode != updated.source_mode
        || previous_config.upstream_base_url != updated.upstream_base_url
        || (updated.source_mode == SourceMode::Biwrite
            && previous_config.biwrite_import_all != updated.biwrite_import_all)
        || (updated.source_mode == SourceMode::Biwrite
            && previous_config.sync_paused
            && !updated.sync_paused);
    if source_changed && updated.source_mode == SourceMode::Manual {
        let dir = s.state_dir();
        match tokio::task::spawn_blocking(move || manual::public_accounts(&dir)).await {
            Ok(Ok(value)) if value["accounts"].as_array().is_some_and(Vec::is_empty) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "manual_pool_empty",
                    "请先添加手动账号，再切换为手动号池",
                );
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "source_unavailable",
                    &e.to_string(),
                );
            }
            Err(_) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "source_unavailable",
                    "无法读取手动账号列表",
                );
            }
        }
    }
    let prospective = if source_changed {
        match s.read_source(&updated).await {
            Ok(snapshot) => Some(snapshot),
            Err(e) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "source_unavailable",
                    &e.to_string(),
                );
            }
        }
    } else {
        None
    };
    if updated.source_mode == SourceMode::Manual
        && prospective
            .as_ref()
            .is_some_and(|(v, _)| v.accounts.is_empty())
    {
        return error(
            StatusCode::BAD_REQUEST,
            "manual_pool_empty",
            "请先添加手动账号，再切换为手动号池",
        );
    }
    let model = updated.model_override.as_deref();
    let model_available = |m: &str| match &prospective {
        Some((snapshot, _)) => snapshot
            .accounts
            .iter()
            .any(|a| a.models.iter().any(|v| v == m)),
        None => s
            .scheduler
            .snapshot(&updated.policy)
            .iter()
            .any(|a| a.present && a.models.iter().any(|v| v == m)),
    };
    if let Some(m) = model
        && previous_config.model_override != updated.model_override
        && !model_available(m)
    {
        return error(
            StatusCode::BAD_REQUEST,
            "model_unavailable",
            "当前候选号池中没有此模型",
        );
    }
    let path = s.config_path.clone();
    let saved = updated.clone();
    if !matches!(
        tokio::task::spawn_blocking(move || config::save(&path, &saved)).await,
        Ok(Ok(()))
    ) {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_write_failed",
            "无法保存设置",
        );
    }
    *s.config.write().unwrap_or_else(|e| e.into_inner()) = updated;
    if let Some((snapshot, upstream)) = prospective {
        s.scheduler.sync_accounts(snapshot.accounts);
        *s.resolved_upstream
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(upstream);
        *s.source.write().unwrap_or_else(|e| e.into_inner()) = SourceState {
            read_at_ms: snapshot.read_at_ms,
            last_error: None,
            warnings: snapshot.warnings,
            providers: snapshot.providers,
        };
    }
    s.scheduler.notify().notify_waiters();
    Json(json!({"ok":true,"config":s.settings().public()})).into_response()
}

async fn api_manual_list(State(s): State<Arc<AppState>>) -> Response {
    let dir = s.state_dir();
    match tokio::task::spawn_blocking(move || manual::public_accounts(&dir)).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(e)) => error(
            StatusCode::BAD_REQUEST,
            "manual_pool_unavailable",
            &e.to_string(),
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "manual_pool_unavailable",
            "读取手动号池失败",
        ),
    }
}

async fn api_manual_add(State(s): State<Arc<AppState>>, Json(input): Json<Value>) -> Response {
    let _lock = s.settings_lock.lock().await;
    let dir = s.state_dir();
    match tokio::task::spawn_blocking(move || manual::add(&dir, input)).await {
        Ok(Ok(id)) => {
            if s.settings().source_mode == SourceMode::Manual {
                let _ = s.sync_source().await;
            }
            Json(json!({"ok":true,"id":id})).into_response()
        }
        Ok(Err(e)) => error(
            StatusCode::BAD_REQUEST,
            "invalid_manual_account",
            &e.to_string(),
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "manual_save_failed",
            "保存手动账号失败",
        ),
    }
}

async fn api_manual_delete(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let _lock = s.settings_lock.lock().await;
    let dir = s.state_dir();
    match tokio::task::spawn_blocking(move || manual::remove(&dir, &id)).await {
        Ok(Ok(true)) => {
            if s.settings().source_mode == SourceMode::Manual {
                let _ = s.sync_source().await;
            }
            Json(json!({"ok":true})).into_response()
        }
        Ok(Ok(false)) => error(
            StatusCode::NOT_FOUND,
            "manual_account_not_found",
            "未找到手动账号",
        ),
        Ok(Err(e)) => error(
            StatusCode::BAD_REQUEST,
            "manual_delete_failed",
            &e.to_string(),
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "manual_delete_failed",
            "删除手动账号失败",
        ),
    }
}
async fn api_sync(State(s): State<Arc<AppState>>) -> Response {
    match s.sync_source_now().await {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "source_unavailable",
            &e.to_string(),
        ),
    }
}
async fn api_reset(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let model = body
        .as_ref()
        .and_then(|b| b.0.get("model"))
        .and_then(Value::as_str);
    if s.scheduler.clear_cooldown(&id, model) {
        Json(json!({"ok":true})).into_response()
    } else {
        error(
            StatusCode::NOT_FOUND,
            "account_not_found",
            "未找到该账号或模型",
        )
    }
}
async fn api_connect(State(s): State<Arc<AppState>>) -> Response {
    let cfg = s.settings();
    let dir = s
        .config_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    match tokio::task::spawn_blocking(move || {
        integration::connect(&cfg.base_url(), &cfg.local_token, &dir)
    })
    .await
    {
        Ok(Ok(message)) => Json(json!({"ok":true,"message":message})).into_response(),
        Ok(Err(e)) => error(StatusCode::CONFLICT, "connection_failed", &e.to_string()),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "connection_failed",
            "接入过程未完成",
        ),
    }
}

async fn api_legacy_connect(State(s): State<Arc<AppState>>) -> Response {
    let cfg = s.settings();
    let dir = s.state_dir();
    let result = tokio::task::spawn_blocking(move || {
        integration::connect_legacy_monitor(&cfg.base_url(), &dir)
    })
    .await;
    legacy_integration_response(result)
}

async fn api_legacy_restore(State(s): State<Arc<AppState>>) -> Response {
    let dir = s.state_dir();
    let result =
        tokio::task::spawn_blocking(move || integration::restore_legacy_monitor(&dir)).await;
    legacy_integration_response(result)
}

fn legacy_integration_response(
    result: std::result::Result<Result<String>, tokio::task::JoinError>,
) -> Response {
    match result {
        Ok(Ok(message)) => Json(json!({"ok":true,"message":message})).into_response(),
        Ok(Err(e)) => error(
            StatusCode::CONFLICT,
            "legacy_connection_failed",
            &e.to_string(),
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "legacy_connection_failed",
            "旧供应商监控接入操作未完成",
        ),
    }
}

async fn api_models(State(s): State<Arc<AppState>>) -> Json<Value> {
    let mut models: Vec<String> = s
        .scheduler
        .snapshot(&s.settings().policy)
        .iter()
        .filter(|a| a.present)
        .flat_map(|a| a.models.clone())
        .collect();
    models.sort();
    models.dedup();
    Json(
        json!({"object":"list","data":models.into_iter().map(|id|json!({"id":id,"object":"model","owned_by":"biwrite-pool"})).collect::<Vec<_>>()}),
    )
}
async fn ws_unavailable() -> Response {
    error(
        StatusCode::UPGRADE_REQUIRED,
        "use_http_responses",
        "此入口使用 HTTP/SSE；请将供应商 supports_websockets 设为 false",
    )
}

async fn api_probe(State(s): State<Arc<AppState>>, Json(v): Json<Value>) -> Response {
    let (Some(key), Some(model)) = (v["key_id"].as_str(), v["model"].as_str()) else {
        return error(StatusCode::BAD_REQUEST, "invalid_probe", "请指定账号和模型");
    };
    if !config::valid_key_id(key) || !config::safe_label(model) {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_probe",
            "账号或模型格式无效",
        );
    }
    let body = probe_payload(key, model);
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let local_token = s.settings().local_token;
    let probe_secrets = s.scheduler.credential_snapshot();
    let sanitize = |text: &str| {
        crate::probe::redact_sensitive(
            &s.scheduler.redact_sensitive(text),
            probe_secrets
                .iter()
                .map(|secret| secret.as_ref())
                .chain(std::iter::once(local_token.as_str())),
        )
    };
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("accept", HeaderValue::from_static("text/event-stream"));
    headers.insert(
        "user-agent",
        HeaderValue::from_static(concat!("PoolRouter/", env!("CARGO_PKG_VERSION"))),
    );
    if let Ok(value) = HeaderValue::from_str(key) {
        headers.insert("x-pool-router-key", value);
    }
    headers.insert("x-pool-router-probe", HeaderValue::from_static("1"));
    let response = tokio::time::timeout_at(
        deadline,
        run_proxy(
            s.clone(),
            Uri::from_static("/v1/responses"),
            headers,
            Bytes::from(body.to_string()),
        ),
    )
    .await;
    let response = match response {
        Ok(response) => response,
        Err(_) => {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let mut collector = crate::probe::Collector::new(true);
            collector.add_event("request_started", 0, "开始固定合成验证请求。");
            collector.add_event("probe_timeout", elapsed_ms, "验证超过 120 秒，已停止等待。");
            let details =
                collector.details(None, model, elapsed_ms, Some("probe_timeout"), sanitize);
            return Json(json!({"ok":false,"status":504,"message":"验证超时","response_model":null,"error_code":"probe_timeout","details":details})).into_response();
        }
    };
    let status = response.status();
    // This is a locally generated record ID, never an arbitrary upstream header value.
    let request_id = response
        .headers()
        .get("x-pool-router-request-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .map(|value| value.to_string());
    let is_sse = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
    let mut collector = crate::probe::Collector::new(is_sse);
    collector.add_event("request_started", 0, "开始固定合成验证请求。");
    collector.add_event(
        "response_headers",
        started.elapsed().as_millis() as u64,
        &format!("收到 HTTP {} 响应。", status.as_u16()),
    );
    let mut stream = response.into_body().into_data_stream();
    let mut received = 0usize;
    let mut failure = None;
    loop {
        let part = tokio::time::timeout_at(deadline, stream.next()).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match part {
            Ok(Some(Ok(bytes))) => {
                received = received.saturating_add(bytes.len());
                if received > crate::probe::MAX_RESPONSE_BYTES {
                    failure = Some("probe_response_too_large");
                    break;
                }
                collector.push(&bytes, elapsed_ms);
            }
            Ok(None) => break,
            Ok(Some(Err(_))) => {
                failure = Some("probe_stream_failed");
                break;
            }
            Err(_) => {
                failure = Some("probe_timeout");
                break;
            }
        }
    }
    // Dropping the body releases its request lease and cancels further upstream reads.
    drop(stream);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    collector.finish(elapsed_ms);
    if let Some(code) = failure {
        collector.add_event(code, elapsed_ms, crate::probe::error_message(Some(code)));
    }
    if let Some(record) = request_id.as_ref().and_then(|id| {
        s.records
            .records()
            .into_iter()
            .find(|record| &record.id == id)
    }) {
        collector.record_events(&record);
    }
    let observation = collector.observation().clone();
    let ok = failure.is_none()
        && status.is_success()
        && observation.completed
        && observation.error_code.is_none();
    let status_code = match status.as_u16() {
        401 => Some("authentication_error"),
        403 => Some("permission_denied"),
        429 => Some("rate_limit_exceeded"),
        500..=599 => Some("upstream_error"),
        _ => None,
    };
    let observed_code = collector.error_code();
    let error_code = failure
        .or(observed_code.filter(|code| *code != "unknown_error"))
        .or(status_code)
        .or(observed_code)
        .or((!ok).then_some("probe_incomplete"))
        .map(str::to_owned);
    let response_model = observation.model.as_deref().map(sanitize);
    let details = collector.details(
        request_id,
        model,
        elapsed_ms,
        error_code.as_deref(),
        sanitize,
    );
    Json(json!({"ok":ok,"status":status.as_u16(),"message":if ok{"验证成功"}else{"本次验证未成功，请查看详细日志"},"response_model":response_model,"error_code":error_code,"details":details})).into_response()
}

fn probe_payload(key_id: &str, model: &str) -> Value {
    // Some Responses endpoints require a cache routing key. This is
    // metadata for this short synthetic probe, never a replacement for a real
    // Codex conversation's cache key.
    let model_hash = blake3::hash(model.as_bytes()).to_hex();
    let cache_key = format!(
        "pr-probe-{}-{}",
        &key_id[..key_id.len().min(12)],
        &model_hash[..8]
    );
    json!({"model":model,"instructions":crate::probe::REQUEST_TEXT,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":crate::probe::REQUEST_TEXT}]}],
        "reasoning":{"effort":"low"},"include":["reasoning.encrypted_content"],
        "prompt_cache_key":cache_key,"stream":true,"store":false})
}

async fn proxy_handler(State(s): State<Arc<AppState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let limit = s.settings().max_request_bytes;
    let bytes = match to_bytes(body, limit).await {
        Ok(v) => v,
        Err(_) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "请求超过本地允许的大小",
            );
        }
    };
    let accepts_sse = parts
        .headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
    if accepts_sse
        && parts.uri.path() == "/v1/responses"
        && legacy_key(&parts.headers, &s.settings()).is_none()
    {
        waiting_stream(s, parts.uri, parts.headers, bytes)
    } else {
        run_proxy(s, parts.uri, parts.headers, bytes).await
    }
}

/// Send SSE comments while selecting an account and while upstream is silent.
/// A comment carries no response ID or generated output, so pre-output failover
/// remains safe. Keeping the pending operation inside Body also cancels queued
/// work as soon as the downstream disconnects.
fn waiting_stream(state: Arc<AppState>, uri: Uri, headers: HeaderMap, raw: Bytes) -> Response {
    state
        .counters
        .waiting_streams
        .fetch_add(1, Ordering::Relaxed);
    let output = async_stream::stream! {
        yield Ok::<Bytes,io::Error>(Bytes::from_static(b": pool-router waiting\n\n"));
        let pending=run_proxy(state,uri,headers,raw);
        tokio::pin!(pending);
        let mut heartbeat=tokio::time::interval(Duration::from_secs(5));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat.tick().await;
        let response=loop {
            tokio::select! {
                result=&mut pending=>break result,
                _=heartbeat.tick()=>yield Ok(Bytes::from_static(b": pool-router waiting\n\n")),
            }
        };
        let status=response.status();
        let is_sse=response.headers().get("content-type").and_then(|v|v.to_str().ok())
            .is_some_and(|v|v.to_ascii_lowercase().contains("text/event-stream"));
        if status.is_success() && is_sse {
            let mut body=response.into_body().into_data_stream();
            loop {
                tokio::select! {
                    part=body.next()=>match part {
                        Some(Ok(bytes))=>yield Ok(bytes),
                        Some(Err(_))=>{yield Err(io::Error::other("上游流连接中断"));break;},
                        None=>break,
                    },
                    _=heartbeat.tick()=>yield Ok(Bytes::from_static(b": pool-router waiting\n\n")),
                }
            }
        } else {
            let bytes=to_bytes(response.into_body(),64*1024).await.unwrap_or_default();
            let value=serde_json::from_slice::<Value>(&bytes).unwrap_or_default();
            let event=if status.is_success() && monitor::inspect_json(&value).completed {
                json!({"type":"response.completed","response":value})
            } else {
                let details=value.get("error").filter(|v|v.is_object()).cloned().unwrap_or_else(||
                    json!({"type":"pool_router_error","code":"upstream_http_error","message":"上游未返回有效的流式响应"}));
                json!({"type":"error","error":details,"status":status.as_u16()})
            };
            yield Ok(Bytes::from(format!("data: {}\n\n",event)));
        }
    };
    let mut response = Response::new(Body::from_stream(output));
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

fn decode_request(bytes: &[u8], encoding: Option<&str>, limit: usize) -> Result<Vec<u8>> {
    let reader: Box<dyn Read + '_> = match encoding
        .unwrap_or("identity")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "identity" | "" => Box::new(bytes),
        "gzip" => Box::new(flate2::read::GzDecoder::new(bytes)),
        "deflate" => Box::new(flate2::read::ZlibDecoder::new(bytes)),
        "zstd" => Box::new(zstd::stream::read::Decoder::new(bytes)?),
        _ => anyhow::bail!("unsupported_encoding"),
    };
    let mut out = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut out)?;
    if out.len() > limit {
        anyhow::bail!("body_limit");
    }
    Ok(out)
}

fn forward_headers(incoming: &HeaderMap, rewritten: bool) -> HeaderMap {
    let connection_tokens: HashSet<String> = incoming
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    let mut result = HeaderMap::new();
    for (name, value) in incoming {
        let key = name.as_str();
        if matches!(
            key,
            "authorization"
                | "proxy-authorization"
                | "host"
                | "content-length"
                | "transfer-encoding"
                | "connection"
                | "keep-alive"
                | "te"
                | "trailer"
                | "upgrade"
                | "cookie"
                | "x-api-key"
                | "openai-organization"
                | "openai-project"
                | "chatgpt-account-id"
                | "accept-encoding"
                | "origin"
                | "referer"
        ) || key.starts_with("x-pool-router-")
            || connection_tokens.contains(key)
            || (rewritten && key == "content-encoding")
        {
            continue;
        }
        result.append(name, value.clone());
    }
    result.insert("accept-encoding", HeaderValue::from_static("identity"));
    result
}

fn response_headers(incoming: &HeaderMap) -> HeaderMap {
    let connection_tokens: HashSet<String> = incoming
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    let mut out = HeaderMap::new();
    for (name, value) in incoming {
        if !matches!(
            name.as_str(),
            "transfer-encoding"
                | "connection"
                | "content-length"
                | "keep-alive"
                | "proxy-authenticate"
                | "set-cookie"
                | "authorization"
                | "trailer"
                | "upgrade"
        ) && !connection_tokens.contains(name.as_str())
        {
            out.append(name, value.clone());
        }
    }
    out.insert("x-accel-buffering", HeaderValue::from_static("no"));
    out
}

fn retry_after(headers: &HeaderMap) -> Option<u64> {
    let v = headers.get("retry-after")?.to_str().ok()?;
    if let Ok(seconds) = v.trim().parse::<u64>() {
        return Some(seconds.saturating_mul(1000).min(86_400_000));
    }
    httpdate::parse_http_date(v)
        .ok()?
        .duration_since(std::time::SystemTime::now())
        .ok()
        .map(|d| d.as_millis().min(86_400_000) as u64)
}

fn health_status(status: u16, o: &Observation) -> u16 {
    match o.error_code.as_deref() {
        Some(
            "rate_limit_exceeded" | "too_many_requests" | "quota_exceeded" | "insufficient_quota",
        ) => 429,
        Some("invalid_api_key" | "authentication_error" | "unauthorized") => 401,
        Some("permission_denied") => 403,
        _ if o.retryable || status < 400 => 502,
        _ => status,
    }
}

fn valid_json_success(path: &str, data: &Value, o: &Observation) -> bool {
    if !data.is_object() || o.error_code.is_some() || (o.terminal && !o.completed) {
        return false;
    }
    if path.ends_with("/responses/compact") {
        return matches!(data["object"].as_str(), Some("response.compaction"))
            && data["output"].is_array();
    }
    if path.ends_with("/chat/completions") {
        return data["choices"].as_array().is_some_and(|choices| {
            !choices.is_empty()
                && choices
                    .iter()
                    .all(|v| v["message"].is_object() && v["finish_reason"].is_string())
        });
    }
    o.completed
}

struct RequestGuard {
    state: Arc<AppState>,
    pub record: RequestRecord,
    started: Instant,
    recording: bool,
    finished: bool,
    upstream_session: Option<String>,
}
impl RequestGuard {
    fn new(state: Arc<AppState>, record: RequestRecord, recording: bool) -> Self {
        state.counters.requests.fetch_add(1, Ordering::Relaxed);
        state.counters.active.fetch_add(1, Ordering::Relaxed);
        Self {
            state,
            record,
            started: Instant::now(),
            recording,
            finished: false,
            upstream_session: None,
        }
    }
    fn observe(&mut self, o: &Observation) {
        if o.meaningful_output && self.record.first_output_ms.is_none() {
            self.record.first_output_ms = Some(self.started.elapsed().as_millis() as u64);
        }
        self.record.response_model = o.model.clone();
        self.record.response_effort = o.effort.clone();
        self.record.response_service_tier = o.service_tier.clone();
        self.record.input_tokens = o.input_tokens;
        self.record.output_tokens = o.output_tokens;
        self.record.cached_tokens = o.cached_tokens;
        self.record.reasoning_tokens = o.reasoning_tokens;
        if self.record.route_mode != "legacy"
            && let (Some(id), Some(key), Some(model)) = (
                &o.response_id,
                &self.record.key_id,
                &self.record.effective_model,
            )
        {
            self.state.scheduler.bind_response_with_session(
                id,
                key,
                model,
                self.upstream_session.as_deref(),
            );
        }
    }
    fn finish(&mut self, outcome: &str) {
        self.record.outcome = outcome.into();
        self.finished = true;
    }
    fn action(&mut self, action: &str, attempt: usize, count: usize) {
        if self.record.gateway_actions.len() < 32 {
            self.record.gateway_actions.push(GatewayAction {
                action: action.into(),
                attempt,
                elapsed_ms: self.started.elapsed().as_millis() as u64,
                count,
                ..GatewayAction::default()
            });
        }
    }
    fn error(&self, status: StatusCode, code: &str, message: &str) -> Response {
        let mut response = error(status, code, message);
        if let Ok(value) = HeaderValue::from_str(&self.record.id) {
            response
                .headers_mut()
                .insert("x-pool-router-request-id", value);
        }
        response
    }
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.state.counters.active.fetch_sub(1, Ordering::Relaxed);
        if !self.finished {
            self.record.outcome = "client_cancelled".into();
        }
        self.record.duration_ms = self.started.elapsed().as_millis() as u64;
        if self.record.outcome == "completed" {
            self.state
                .counters
                .successes
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.state.counters.failures.fetch_add(1, Ordering::Relaxed);
        }
        if self.recording {
            let _ = self.state.records.record(self.record.clone());
        }
    }
}

async fn wait_slot(s: &AppState, deadline: Instant) {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_millis(250));
    if !remaining.is_zero() {
        let notify = s.scheduler.notify();
        tokio::select! {_ = notify.notified()=>{},_ = tokio::time::sleep(remaining)=>{}}
    }
}

/// Observe a caller-owned credential without ever consulting the stored pool.
/// Errors and unrecognized bodies are forwarded with their upstream status and bytes.
async fn run_legacy_proxy(
    state: Arc<AppState>,
    uri: Uri,
    headers: HeaderMap,
    raw: Bytes,
    key: String,
    cfg: Config,
) -> Response {
    const OBSERVE_LIMIT: usize = 4 * 1024 * 1024;
    let key_id = blake3::hash(key.as_bytes()).to_hex().to_string();
    let copy = raw.clone();
    let request_limit = cfg.max_request_bytes;
    let encoding = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = tokio::task::spawn_blocking(move || {
        decode_request(&copy, encoding.as_deref(), request_limit)
            .ok()
            .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    let model = body["model"]
        .as_str()
        .filter(|v| config::safe_label(v))
        .map(str::to_owned);
    let effort = body["reasoning"]["effort"]
        .as_str()
        .or_else(|| body["reasoning_effort"].as_str())
        .filter(|v| config::safe_label(v))
        .map(str::to_owned);
    let record = RequestRecord {
        id: uuid::Uuid::new_v4().to_string(),
        started_at_ms: now_ms(),
        conversation_id: monitor::conversation_id(&headers, &body),
        route_mode: "legacy".into(),
        requested_service_tier: body["service_tier"]
            .as_str()
            .filter(|v| config::safe_label(v))
            .map(str::to_owned),
        transport: "http".into(),
        requested_model: model.clone(),
        effective_model: model,
        requested_effort: effort.clone(),
        effective_effort: effort,
        key_id: Some(key_id.clone()),
        request_bytes: raw.len() as u64,
        has_previous_response_id: body["previous_response_id"]
            .as_str()
            .is_some_and(|v| !v.is_empty()),
        input_items: body["input"].as_array().map(Vec::len),
        store: body["store"].as_bool(),
        ..RequestRecord::default()
    };
    let mut guard = RequestGuard::new(state.clone(), record, cfg.recording_enabled);
    let override_origin = state
        .legacy_upstream_override
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let origin = if let Some(origin) = override_origin {
        Ok(origin)
    } else {
        let state_dir = state.state_dir();
        tokio::task::spawn_blocking(move || -> Result<String> {
            if let Some(origin) = integration::legacy_monitor_upstream(&state_dir)? {
                return Ok(origin);
            }
            anyhow::bail!("请先在面板接入 CC Switch 兼容监控；此模式只使用接入备份中的原上游地址")
        })
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("无法读取兼容监控的上游地址")))
    };
    let origin = match origin {
        Ok(origin) => origin,
        Err(e) => {
            guard.record.status = Some(503);
            guard.finish("upstream_unconfigured");
            return guard.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream_unconfigured",
                &e.to_string(),
            );
        }
    };
    let suffix = uri
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or(uri.path())
        .strip_prefix("/v1")
        .unwrap_or("/responses");
    let target = format!("{}{}", origin.trim_end_matches('/'), suffix);
    let request = state
        .legacy_client
        .post(target)
        .headers(forward_headers(&headers, false))
        .bearer_auth(&key)
        .body(raw);
    let started = Instant::now();
    let upstream = match tokio::time::timeout(
        Duration::from_millis(cfg.upstream_header_timeout_ms),
        request.send(),
    )
    .await
    {
        Ok(Ok(response)) => response,
        outcome => {
            let (status, reason) = if outcome.is_err() {
                (StatusCode::GATEWAY_TIMEOUT, "header_timeout")
            } else {
                (StatusCode::BAD_GATEWAY, "transport_uncertain")
            };
            guard.record.status = Some(status.as_u16());
            guard.record.attempts.push(AttemptRecord {
                error_message: None,
                key_id,
                status: None,
                reason: reason.into(),
                duration_ms: started.elapsed().as_millis() as u64,
            });
            guard.finish(reason);
            return guard.error(
                status,
                reason,
                "旧供应商上游连接未完成；未重试或改用其他账号",
            );
        }
    };
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let encoding = upstream_headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("identity")
        .to_owned();
    let identity = encoding.is_empty() || encoding.eq_ignore_ascii_case("identity");
    let is_sse = upstream_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
    guard.record.status = Some(status.as_u16());
    guard.record.transport = if is_sse { "sse" } else { "http" }.into();
    let request_id = guard.record.id.clone();
    let mut upstream_stream = upstream.bytes_stream();
    let stream = async_stream::stream! {
        let mut guard = guard;
        let mut observer = SseObserver::new();
        let mut buffered = Vec::new();
        let mut too_large = false;
        let mut transport_failed = false;
        let add_heartbeats = is_sse && identity && status.is_success();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat.tick().await;
        if add_heartbeats { yield Ok::<Bytes,io::Error>(Bytes::from_static(b": pool-router legacy waiting\n\n")); }
        loop {
            // Local heartbeats must not reset the upstream idle deadline.
            let part = {
                let pending = tokio::time::timeout(Duration::from_millis(cfg.stream_idle_timeout_ms), upstream_stream.next());
                tokio::pin!(pending);
                loop {
                    tokio::select! {
                        result = &mut pending => break result,
                        _ = heartbeat.tick(), if add_heartbeats => yield Ok(Bytes::from_static(b": pool-router legacy waiting\n\n")),
                    }
                }
            };
            match part {
                Ok(Some(Ok(bytes))) => {
                    if guard.record.ttfb_ms.is_none() { guard.record.ttfb_ms = Some(guard.started.elapsed().as_millis() as u64); }
                    guard.record.response_bytes += bytes.len() as u64;
                    if is_sse && identity {
                        observer.push(&bytes);
                        guard.observe(observer.observation());
                    } else if !too_large {
                        if buffered.len() + bytes.len() <= OBSERVE_LIMIT { buffered.extend_from_slice(&bytes); }
                        else { too_large = true; buffered.clear(); }
                    }
                    yield Ok(bytes);
                    if is_sse && identity && observer.observation().terminal { break; }
                }
                Ok(None) => break,
                _ => {
                    transport_failed = true;
                    break;
                }
            }
        }
        let mut parsed = None;
        if !(is_sse && identity) && !too_large
            && let Ok(decoded) = decode_request(&buffered, Some(&encoding), OBSERVE_LIMIT) {
            if is_sse { observer.push(&decoded); }
            else { parsed = serde_json::from_slice::<Value>(&decoded).ok(); }
        }
        observer.finish();
        let observation = if is_sse { observer.observation().clone() }
            else { parsed.as_ref().map(monitor::inspect_json).unwrap_or_default() };
        guard.observe(&observation);
        let completed = status.is_success() && !transport_failed && if is_sse {
            observation.completed && observation.error_code.is_none()
        } else { parsed.as_ref().is_some_and(|v| valid_json_success(uri.path(), v, &observation)) };
        let outcome = if completed { "completed" } else if transport_failed { "stream_read_failed" }
            else if !status.is_success() || observation.error_code.is_some() || (observation.terminal && !observation.completed) { "upstream_failed" }
            else { "invalid_upstream_response" };
        let error_message = observation.raw_error_message.as_deref().and_then(|message|
            crate::probe::bounded_error_message(message, [cfg.local_token.as_str(), key.as_str()]));
        guard.record.attempts.push(AttemptRecord { error_message, key_id, status: Some(status.as_u16()),
            reason: observation.error_code.unwrap_or_else(|| outcome.into()), duration_ms: started.elapsed().as_millis() as u64 });
        guard.finish(outcome);
        if transport_failed { yield Err(io::Error::other("旧供应商上游响应中断")); }
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers(&upstream_headers);
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert("x-pool-router-request-id", value);
    }
    response
}

#[derive(Default)]
struct RequestRecovery {
    rate_limits: HashMap<String, usize>,
    session_reset: bool,
    compaction_repaired: bool,
}

impl RequestRecovery {
    fn reset_session(
        &mut self,
        state: &AppState,
        cfg: &Config,
        kind: Option<RecoveryKind>,
        scope: Option<&str>,
        request: (&HeaderMap, &Value),
    ) -> bool {
        let (headers, body) = request;
        let Some(scope) = scope else {
            return false;
        };
        let rate_limits = if kind == Some(RecoveryKind::RateLimited) {
            let count = self.rate_limits.entry(scope.to_owned()).or_default();
            *count += 1;
            *count
        } else {
            self.rate_limits.remove(scope);
            0
        };
        let persistent_rejection = kind == Some(RecoveryKind::SessionResourceUnavailable)
            || (kind == Some(RecoveryKind::RateLimited) && rate_limits >= 2);
        if !cfg.session_recovery_enabled || self.session_reset || !persistent_rejection {
            return false;
        }
        let mut probe_headers = headers.clone();
        let mut probe_body = body.clone();
        if !recovery::rewrite_upstream_session(
            &mut probe_headers,
            &mut probe_body,
            "00000000-0000-4000-8000-000000000001",
        ) {
            return false;
        }
        state
            .session_remaps
            .rotate(scope, cfg.policy.affinity_ttl_seconds.saturating_mul(1000));
        self.session_reset = true;
        true
    }
}

fn session_scope(
    headers: &HeaderMap,
    body: &Value,
    key: &str,
    model: &str,
    upstream: &str,
) -> Option<String> {
    let identity = monitor::conversation_id(headers, body).or_else(|| {
        body["prompt_cache_key"]
            .as_str()
            .filter(|v| !v.is_empty() && v.len() <= 1024)
            .map(str::to_owned)
    })?;
    Some(
        blake3::hash(
            json!([identity, key, model, upstream])
                .to_string()
                .as_bytes(),
        )
        .to_hex()
        .to_string(),
    )
}

fn recovery_reason(kind: Option<RecoveryKind>) -> Option<&'static str> {
    kind.map(|kind| match kind {
        RecoveryKind::RateLimited => "rate_limit_exceeded",
        RecoveryKind::SessionResourceUnavailable => "resource_not_found",
        RecoveryKind::IncompatibleEncryptedContent => "invalid_encrypted_content",
    })
}

fn observed_error_message(
    state: &AppState,
    observation: &Observation,
    secrets: &[&str],
) -> Option<String> {
    let message = observation.raw_error_message.as_deref()?;
    crate::probe::bounded_error_message(
        &state.scheduler.redact_sensitive(message),
        secrets.iter().copied(),
    )
}

pub async fn run_proxy(state: Arc<AppState>, uri: Uri, headers: HeaderMap, raw: Bytes) -> Response {
    let initial = state.settings();
    if let Some(key) = legacy_key(&headers, &initial).map(str::to_owned) {
        if !legacy_path(uri.path()) {
            return error(
                StatusCode::UNAUTHORIZED,
                "local_auth_required",
                "此操作需要本地访问令牌",
            );
        }
        return run_legacy_proxy(state, uri, headers, raw, key, initial).await;
    }
    // Internal probes intentionally omit Authorization. A supplied foreign credential
    // must never fall through to the stored pool, even for direct Rust callers.
    if bearer(&headers)
        .is_some_and(|key| !same_token(key.as_bytes(), initial.local_token.as_bytes()))
    {
        return error(
            StatusCode::UNAUTHORIZED,
            "local_auth_required",
            "访问凭据无效或旧供应商监控已停用",
        );
    }
    let encoding = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = raw.clone();
    let limit = initial.max_request_bytes;
    let decoded = match tokio::task::spawn_blocking(move || {
        decode_request(&bytes, encoding.as_deref(), limit)
    })
    .await
    {
        Ok(Ok(v)) => v,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "request_decode_failed",
                "无法解析请求压缩格式或请求过大",
            );
        }
    };
    let mut body: Value = match serde_json::from_slice(&decoded) {
        Ok(v) => v,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                "模型请求必须是 JSON",
            );
        }
    };
    let Some(requested_model) = body["model"]
        .as_str()
        .filter(|s| config::safe_label(s))
        .map(str::to_owned)
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "model_required",
            "请指定有效的模型名称",
        );
    };
    let requested_effort = body["reasoning"]["effort"]
        .as_str()
        .or_else(|| body["reasoning_effort"].as_str())
        .filter(|v| config::safe_label(v))
        .map(str::to_owned);
    let probe = headers.get("x-pool-router-probe").is_some();
    let effective_model = if probe {
        requested_model.clone()
    } else {
        initial
            .model_override
            .clone()
            .unwrap_or_else(|| requested_model.clone())
    };
    let effective_effort = if probe {
        requested_effort.clone()
    } else {
        initial
            .reasoning_override
            .clone()
            .or_else(|| requested_effort.clone())
    };
    let previous = body["previous_response_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if previous.is_some() && effective_model != requested_model {
        return error(
            StatusCode::CONFLICT,
            "stateful_model_change",
            "请求引用了前一次响应，请保持原模型完成此对话链",
        );
    }
    let mut rewritten = effective_model != requested_model || effective_effort != requested_effort;
    if rewritten {
        body["model"] = Value::String(effective_model.clone());
        if let Some(effort) = &effective_effort {
            if uri.path().ends_with("chat/completions") {
                body["reasoning_effort"] = Value::String(effort.clone());
            } else {
                if !body["reasoning"].is_object() {
                    body["reasoning"] = json!({});
                }
                body["reasoning"]["effort"] = Value::String(effort.clone());
            }
        }
    }
    let image_summary = if initial.compress_images {
        match tokio::task::spawn_blocking(move || {
            let summary = crate::image_compression::compress_inline_images(&mut body);
            (body, summary)
        })
        .await
        {
            Ok((updated, summary)) => {
                body = updated;
                rewritten |= summary.count > 0;
                Some(summary)
            }
            Err(_) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "image_processing_failed",
                    "图片处理未完成，请关闭图片压缩后重试",
                );
            }
        }
    } else {
        None
    };
    let mut payload = if rewritten {
        Bytes::from(body.to_string())
    } else {
        raw.clone()
    };
    let conversation = monitor::conversation_id(&headers, &body);
    let explicit = headers
        .get("x-pool-router-key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| config::valid_key_id(v))
        .map(str::to_owned);
    let record = RequestRecord {
        route_mode: "pool".into(),
        requested_service_tier: body["service_tier"]
            .as_str()
            .filter(|v| config::safe_label(v))
            .map(str::to_owned),
        id: uuid::Uuid::new_v4().to_string(),
        started_at_ms: now_ms(),
        conversation_id: conversation.clone(),
        transport: if body["stream"].as_bool() == Some(true) {
            "sse"
        } else {
            "http"
        }
        .into(),
        requested_model: Some(requested_model),
        effective_model: Some(effective_model.clone()),
        requested_effort,
        effective_effort,
        request_bytes: raw.len() as u64,
        has_previous_response_id: previous.is_some(),
        input_items: body["input"].as_array().map(Vec::len),
        store: body["store"].as_bool(),
        ..RequestRecord::default()
    };
    let mut guard = RequestGuard::new(state.clone(), record, initial.recording_enabled);
    if let Some(summary) = image_summary
        && summary.count > 0
    {
        guard.record.gateway_actions.push(GatewayAction {
            action: "image_compression".into(),
            count: summary.count,
            before_bytes: Some(summary.before_bytes as u64),
            after_bytes: Some(summary.after_bytes as u64),
            ..GatewayAction::default()
        });
    }
    let mut selection = Selection {
        model: effective_model.clone(),
        conversation,
        previous_response_id: previous,
        exclude: HashSet::new(),
        explicit_key: explicit,
    };
    let deadline = Instant::now() + Duration::from_millis(initial.queue_timeout_ms);
    let mut attempt_count = 0usize;
    let mut recovery_state = RequestRecovery::default();
    let mut retry_same_key: Option<String> = None;
    'attempts: loop {
        let cfg = state.settings();
        if Instant::now() >= deadline || attempt_count >= initial.max_attempts {
            guard.record.status = Some(503);
            guard.finish("pool_exhausted");
            let mut r = guard.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "pool_exhausted",
                "当前模型候选均忙碌、冷却或重试未成功，稍后重试",
            );
            r.headers_mut()
                .insert("retry-after", HeaderValue::from_static("2"));
            return r;
        }
        let mut attempt_selection = selection.clone();
        if let Some(key) = &retry_same_key {
            attempt_selection.explicit_key = Some(key.clone());
            attempt_selection.exclude.remove(key);
        }
        let lease = match state.scheduler.try_acquire(&attempt_selection, &cfg.policy) {
            Ok(v) => {
                retry_same_key = None;
                v
            }
            Err(SelectError::NoEligible) if retry_same_key.is_some() => {
                retry_same_key = None;
                continue;
            }
            Err(SelectError::UnknownResponse) => {
                guard.finish("unknown_response");
                return guard.error(
                    StatusCode::CONFLICT,
                    "unknown_response",
                    "无法确认前次响应所属账号，不能跨账号续接；请由客户端重发所需上下文",
                );
            }
            Err(SelectError::AffinityUnavailable) if selection.previous_response_id.is_some() => {
                guard.finish("affinity_unavailable");
                return guard.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "affinity_unavailable",
                    "前次响应所属账号或模型当前不可用，已保留上下文关联",
                );
            }
            Err(SelectError::NoEligible | SelectError::AffinityUnavailable)
                if selection.exclude.is_empty() =>
            {
                guard.finish("no_candidate");
                return guard.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no_candidate",
                    "当前号池没有匹配模型的可调度账号，或账号已停用",
                );
            }
            Err(_) => {
                if !selection.exclude.is_empty() {
                    selection.exclude.clear();
                }
                wait_slot(&state, deadline).await;
                continue;
            }
        };
        attempt_count += 1;
        let attempt_started = Instant::now();
        let first_output_deadline =
            attempt_started + Duration::from_millis(cfg.first_output_timeout_ms);
        let key = lease.account.id.clone();
        guard.record.key_id = Some(key.clone());
        let scope = session_scope(
            &headers,
            &body,
            &key,
            &effective_model,
            &lease.account.upstream,
        );
        let mut attempt_headers = headers.clone();
        let mut attempt_payload = payload.clone();
        let mut attempt_rewritten = rewritten;
        let mapped = if let Some(previous) = selection.previous_response_id.as_deref() {
            match state.scheduler.response_session(
                previous,
                &key,
                &effective_model,
                &lease.account.upstream,
                cfg.policy.affinity_ttl_seconds,
            ) {
                Some(mapped) => mapped,
                None => {
                    guard.finish("unknown_response");
                    return guard.error(
                        StatusCode::CONFLICT,
                        "unknown_response",
                        "前次响应的会话绑定已失效，请由客户端重发所需上下文",
                    );
                }
            }
        } else {
            scope.as_deref().and_then(|scope| {
                state
                    .session_remaps
                    .lookup(scope, cfg.policy.affinity_ttl_seconds.saturating_mul(1000))
            })
        };
        guard.upstream_session = None;
        if let Some(mapped) = mapped {
            let mut mapped_body = body.clone();
            if recovery::apply_known_upstream_session(
                &mut attempt_headers,
                &mut mapped_body,
                &mapped,
            ) {
                attempt_payload = Bytes::from(mapped_body.to_string());
                attempt_rewritten = true;
                guard.upstream_session = Some(mapped);
                guard.action("session_reused", attempt_count, 1);
            } else if selection.previous_response_id.is_some() {
                guard.finish("stateful_session_unavailable");
                return guard.error(
                    StatusCode::CONFLICT,
                    "stateful_session_unavailable",
                    "此响应关联的上游会话缺少可恢复的请求标识，请保持原会话配置或重发所需上下文",
                );
            }
        }
        guard.record.upstream_request_bytes = Some(attempt_payload.len() as u64);
        let suffix = uri.path().strip_prefix("/v1").unwrap_or("/responses");
        let target = format!("{}{}", lease.account.upstream.trim_end_matches('/'), suffix);
        let request = state
            .client
            .post(&target)
            .headers(forward_headers(&attempt_headers, attempt_rewritten))
            .bearer_auth(lease.account.secret.as_ref())
            .body(attempt_payload);
        let sent = tokio::time::timeout(
            Duration::from_millis(cfg.upstream_header_timeout_ms).min(
                deadline
                    .min(first_output_deadline)
                    .saturating_duration_since(Instant::now()),
            ),
            request.send(),
        )
        .await;
        let upstream = match sent {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                if let Some(scope) = scope.as_deref() {
                    recovery_state.rate_limits.remove(scope);
                }
                let safe_retry = e.is_connect();
                let reason = if safe_retry {
                    "connect_failed"
                } else {
                    "transport_uncertain"
                };
                state.scheduler.mark_failure(
                    &key,
                    &effective_model,
                    None,
                    None,
                    reason,
                    &cfg.policy,
                );
                guard.record.attempts.push(AttemptRecord {
                    error_message: None,
                    key_id: key.clone(),
                    status: None,
                    reason: reason.into(),
                    duration_ms: attempt_started.elapsed().as_millis() as u64,
                });
                drop(lease);
                if safe_retry {
                    selection.exclude.insert(key);
                    continue;
                }
                guard.finish(reason);
                return guard.error(
                    StatusCode::BAD_GATEWAY,
                    reason,
                    "上游连接异常，无法确定请求是否已被处理，请由客户端决定重试",
                );
            }
            Err(_) => {
                if let Some(scope) = scope.as_deref() {
                    recovery_state.rate_limits.remove(scope);
                }
                let reason = if Instant::now() >= first_output_deadline {
                    "first_output_timeout"
                } else {
                    "header_timeout"
                };
                state.scheduler.mark_failure(
                    &key,
                    &effective_model,
                    None,
                    None,
                    reason,
                    &cfg.policy,
                );
                guard.record.attempts.push(AttemptRecord {
                    error_message: None,
                    key_id: key.clone(),
                    status: None,
                    reason: reason.into(),
                    duration_ms: attempt_started.elapsed().as_millis() as u64,
                });
                if selection.previous_response_id.is_none() {
                    guard.action(
                        if reason == "first_output_timeout" {
                            reason
                        } else {
                            "header_timeout_retry"
                        },
                        attempt_count,
                        1,
                    );
                    drop(lease);
                    selection.exclude.insert(key);
                    continue;
                }
                guard.finish(reason);
                return guard.error(
                    StatusCode::GATEWAY_TIMEOUT,
                    reason,
                    "等待上游响应超时；带有前次响应绑定的请求未自动重放",
                );
            }
        };
        let status = upstream.status();
        let upstream_headers = upstream.headers().clone();
        let retry_delay = retry_after(&upstream_headers);
        guard.record.status = Some(status.as_u16());
        let is_sse = upstream_headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
        let mut upstream_stream = upstream.bytes_stream();
        if !status.is_success() || !is_sse {
            let mut response_body = Vec::new();
            let mut body_error = false;
            let mut first_output_timed_out = false;
            loop {
                match tokio::time::timeout(
                    Duration::from_millis(cfg.stream_idle_timeout_ms).min(
                        deadline
                            .min(first_output_deadline)
                            .saturating_duration_since(Instant::now()),
                    ),
                    upstream_stream.next(),
                )
                .await
                {
                    Ok(Some(Ok(chunk))) => {
                        if response_body.len() + chunk.len() > initial.max_request_bytes {
                            body_error = true;
                            break;
                        }
                        response_body.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    _ => {
                        body_error = true;
                        first_output_timed_out = Instant::now() >= first_output_deadline;
                        break;
                    }
                }
            }
            if body_error {
                if let Some(scope) = scope.as_deref() {
                    recovery_state.rate_limits.remove(scope);
                }
                if first_output_timed_out && selection.previous_response_id.is_none() {
                    state.scheduler.mark_failure(
                        &key,
                        &effective_model,
                        None,
                        None,
                        "first_output_timeout",
                        &cfg.policy,
                    );
                    guard.record.attempts.push(AttemptRecord {
                        error_message: None,
                        key_id: key.clone(),
                        status: Some(status.as_u16()),
                        reason: "first_output_timeout".into(),
                        duration_ms: attempt_started.elapsed().as_millis() as u64,
                    });
                    guard.action("first_output_timeout", attempt_count, 1);
                    drop(upstream_stream);
                    drop(lease);
                    selection.exclude.insert(key);
                    continue;
                }
                guard.finish("response_read_failed");
                guard.record.attempts.push(AttemptRecord {
                    error_message: None,
                    key_id: key.clone(),
                    status: Some(status.as_u16()),
                    reason: "response_read_failed".into(),
                    duration_ms: attempt_started.elapsed().as_millis() as u64,
                });
                state.scheduler.mark_failure(
                    &key,
                    &effective_model,
                    None,
                    None,
                    "response_read_failed",
                    &cfg.policy,
                );
                return guard.error(
                    StatusCode::BAD_GATEWAY,
                    "response_read_failed",
                    "上游响应未完整收到",
                );
            }
            guard.record.response_bytes = response_body.len() as u64;
            let parsed = serde_json::from_slice::<Value>(&response_body).ok();
            let observation = parsed
                .as_ref()
                .map(monitor::inspect_json)
                .unwrap_or_default();
            let recovery_kind = recovery::classify_error(status.as_u16(), &response_body);
            guard.observe(&observation);
            let retryable = matches!(
                status.as_u16(),
                401 | 403 | 408 | 429 | 500 | 502 | 503 | 504
            ) || observation.retryable;
            let invalid_success = status.is_success()
                && observation.error_code.is_none()
                && !(observation.terminal && !observation.completed)
                && !parsed
                    .as_ref()
                    .is_some_and(|v| valid_json_success(uri.path(), v, &observation));
            let failed = !status.is_success()
                || observation.error_code.is_some()
                || (observation.terminal && !observation.completed)
                || invalid_success;
            let reason = if failed {
                recovery_reason(recovery_kind)
                    .or(observation.error_code.as_deref())
                    .unwrap_or("http_error")
            } else {
                "completed"
            };
            guard.record.attempts.push(AttemptRecord {
                error_message: observed_error_message(
                    &state,
                    &observation,
                    &[cfg.local_token.as_str(), lease.account.secret.as_ref()],
                ),
                key_id: key.clone(),
                status: Some(status.as_u16()),
                reason: reason.into(),
                duration_ms: attempt_started.elapsed().as_millis() as u64,
            });
            if failed {
                state.scheduler.mark_failure(
                    &key,
                    &effective_model,
                    Some(health_status(status.as_u16(), &observation)),
                    retry_delay,
                    reason,
                    &cfg.policy,
                );
            } else {
                state.scheduler.mark_success(&key, &effective_model);
                guard
                    .record
                    .first_output_ms
                    .get_or_insert(guard.started.elapsed().as_millis() as u64);
            }
            let mut repaired = false;
            if failed
                && cfg.compaction_repair_enabled
                && !recovery_state.compaction_repaired
                && uri.path() == "/v1/responses/compact"
                && status == StatusCode::BAD_REQUEST
                && recovery_kind == Some(RecoveryKind::IncompatibleEncryptedContent)
            {
                let mut repaired_body = body.clone();
                let count = recovery::strip_incompatible_encrypted_items(&mut repaired_body);
                if count > 0
                    && repaired_body["input"]
                        .as_array()
                        .is_some_and(|items| !items.is_empty())
                {
                    body = repaired_body;
                    payload = Bytes::from(body.to_string());
                    rewritten = true;
                    recovery_state.compaction_repaired = true;
                    guard.action("compaction_repair", attempt_count, count);
                    repaired = true;
                }
            }
            let session_reset = failed
                && recovery_state.reset_session(
                    &state,
                    &cfg,
                    recovery_kind,
                    scope.as_deref(),
                    (&headers, &body),
                );
            if session_reset {
                guard.action("session_reset", attempt_count, 1);
            }
            drop(lease);
            if repaired || session_reset {
                retry_same_key = Some(key);
                continue;
            }
            if failed && retryable {
                selection.exclude.insert(key);
                continue;
            }
            if invalid_success {
                guard.record.status = Some(502);
                guard.finish("invalid_upstream_response");
                return guard.error(
                    StatusCode::BAD_GATEWAY,
                    "invalid_upstream_response",
                    "上游未返回有效的完整模型响应",
                );
            }
            guard.finish(if failed {
                "upstream_failed"
            } else {
                "completed"
            });
            let mut response = Response::new(Body::from(response_body));
            *response.status_mut() = status;
            *response.headers_mut() = response_headers(&upstream_headers);
            if let Ok(value) = HeaderValue::from_str(&guard.record.id) {
                response
                    .headers_mut()
                    .insert("x-pool-router-request-id", value);
            }
            return response;
        }
        // Delay only the prelude. An explicit rejection before meaningful output can use another account.
        let mut observer = SseObserver::new();
        let mut prelude = Vec::<Bytes>::new();
        let mut prelude_size = 0usize;
        let mut ended = false;
        loop {
            let chunk = tokio::time::timeout(
                Duration::from_millis(cfg.stream_idle_timeout_ms).min(
                    deadline
                        .min(first_output_deadline)
                        .saturating_duration_since(Instant::now()),
                ),
                upstream_stream.next(),
            )
            .await;
            match chunk {
                Ok(Some(Ok(bytes))) => {
                    if guard.record.ttfb_ms.is_none() {
                        guard.record.ttfb_ms = Some(guard.started.elapsed().as_millis() as u64);
                    }
                    prelude_size += bytes.len();
                    observer.push(&bytes);
                    prelude.push(bytes);
                    let o = observer.observation();
                    if o.meaningful_output {
                        guard
                            .record
                            .first_output_ms
                            .get_or_insert(guard.started.elapsed().as_millis() as u64);
                    }
                    if o.meaningful_output || o.terminal || prelude_size >= 256 * 1024 {
                        break;
                    }
                }
                Ok(None) => {
                    observer.finish();
                    ended = true;
                    break;
                }
                _ => {
                    if let Some(scope) = scope.as_deref() {
                        recovery_state.rate_limits.remove(scope);
                    }
                    if Instant::now() >= first_output_deadline
                        && selection.previous_response_id.is_none()
                    {
                        state.scheduler.mark_failure(
                            &key,
                            &effective_model,
                            None,
                            None,
                            "first_output_timeout",
                            &cfg.policy,
                        );
                        guard.record.attempts.push(AttemptRecord {
                            error_message: None,
                            key_id: key.clone(),
                            status: Some(status.as_u16()),
                            reason: "first_output_timeout".into(),
                            duration_ms: attempt_started.elapsed().as_millis() as u64,
                        });
                        guard.action("first_output_timeout", attempt_count, 1);
                        drop(upstream_stream);
                        drop(lease);
                        selection.exclude.insert(key);
                        continue 'attempts;
                    }
                    state.scheduler.mark_failure(
                        &key,
                        &effective_model,
                        None,
                        None,
                        "stream_read_failed",
                        &cfg.policy,
                    );
                    guard.finish("stream_read_failed");
                    guard.record.attempts.push(AttemptRecord {
                        error_message: None,
                        key_id: key.clone(),
                        status: Some(status.as_u16()),
                        reason: "stream_read_failed".into(),
                        duration_ms: attempt_started.elapsed().as_millis() as u64,
                    });
                    return guard.error(
                        StatusCode::BAD_GATEWAY,
                        "stream_read_failed",
                        "上游流在输出前中断，未自动重复提交",
                    );
                }
            }
        }
        let observation = observer.observation().clone();
        if observation.terminal {
            ended = true;
        }
        let recovery_kind = if observation.terminal && !observation.meaningful_output {
            let bounded_error: Vec<u8> = prelude
                .iter()
                .flat_map(|part| part.iter().copied())
                .take(256 * 1024)
                .collect();
            recovery::classify_error(status.as_u16(), &bounded_error)
        } else {
            None
        };
        if observation.terminal
            && !observation.meaningful_output
            && (observation.retryable
                || recovery_kind == Some(RecoveryKind::SessionResourceUnavailable))
        {
            let reason = recovery_reason(recovery_kind)
                .or(observation.error_code.as_deref())
                .unwrap_or("upstream_rejected");
            state.scheduler.mark_failure(
                &key,
                &effective_model,
                Some(health_status(status.as_u16(), &observation)),
                retry_delay,
                reason,
                &cfg.policy,
            );
            guard.record.attempts.push(AttemptRecord {
                error_message: observed_error_message(
                    &state,
                    &observation,
                    &[cfg.local_token.as_str(), lease.account.secret.as_ref()],
                ),
                key_id: key.clone(),
                status: Some(status.as_u16()),
                reason: reason.into(),
                duration_ms: attempt_started.elapsed().as_millis() as u64,
            });
            let session_reset = recovery_state.reset_session(
                &state,
                &cfg,
                recovery_kind,
                scope.as_deref(),
                (&headers, &body),
            );
            if session_reset {
                guard.action("session_reset", attempt_count, 1);
            }
            drop(lease);
            drop(upstream_stream);
            if session_reset {
                retry_same_key = Some(key);
            } else {
                selection.exclude.insert(key);
            }
            continue;
        }
        guard.record.transport = "sse".into();
        let policy: RoutingPolicy = cfg.policy.clone();
        let idle = cfg.stream_idle_timeout_ms;
        let request_id = guard.record.id.clone();
        let response_key = key.clone();
        let stream = async_stream::stream! {
            let _lease=lease;
            let mut record_guard=guard;
            record_guard.observe(observer.observation());
            for bytes in prelude {record_guard.record.response_bytes+=bytes.len() as u64;yield Ok::<Bytes,io::Error>(bytes);}
            let mut transport_failed=false;
            let mut output_timed_out=false;
            if !ended {
                loop {
                    let timeout = if observer.observation().meaningful_output { Duration::from_millis(idle) }
                        else { Duration::from_millis(idle).min(first_output_deadline.saturating_duration_since(Instant::now())) };
                    match tokio::time::timeout(timeout,upstream_stream.next()).await {
                        Ok(Some(Ok(bytes)))=>{observer.push(&bytes);record_guard.observe(observer.observation());record_guard.record.response_bytes+=bytes.len() as u64;yield Ok(bytes);if observer.observation().terminal{break;}},
                        Ok(None)=>break,
                        _=>{transport_failed=true;output_timed_out=!observer.observation().meaningful_output && Instant::now()>=first_output_deadline;yield Err(io::Error::other("上游流连接中断"));break;}
                    }
                }
            }
            observer.finish();let final_observation=observer.observation();record_guard.observe(final_observation);
            let completed=final_observation.completed && final_observation.error_code.is_none() && !transport_failed;
            let reason=if completed{"completed"}else if output_timed_out{"first_output_timeout"}else{final_observation.error_code.as_deref().unwrap_or("incomplete_stream")};
            if completed {state.scheduler.mark_success(&key,&effective_model);}else{state.scheduler.mark_failure(&key,&effective_model,Some(health_status(status.as_u16(),final_observation)),retry_delay,reason,&policy);}
            let error_message=observed_error_message(&state,final_observation,&[cfg.local_token.as_str(),_lease.account.secret.as_ref()]);
            record_guard.record.attempts.push(AttemptRecord{error_message,key_id:key,status:Some(status.as_u16()),reason:reason.into(),duration_ms:attempt_started.elapsed().as_millis() as u64});
            record_guard.finish(if completed{"completed"}else{"upstream_failed"});
        };
        let mut response = Response::new(Body::from_stream(stream));
        *response.status_mut() = status;
        *response.headers_mut() = response_headers(&upstream_headers);
        if let Ok(value) = HeaderValue::from_str(&request_id) {
            response
                .headers_mut()
                .insert("x-pool-router-request-id", value);
        }
        if let Ok(value) = HeaderValue::from_str(&response_key[..12.min(response_key.len())]) {
            response
                .headers_mut()
                .insert("x-pool-router-account", value);
        }
        return response;
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    #[test]
    fn probe_has_required_codex_fields_and_short_nonsecret_cache_key() {
        let p = probe_payload(&"a".repeat(64), "gpt-6-astra");
        assert_eq!(p["store"], false);
        assert_eq!(p["stream"], true);
        assert_eq!(p["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(p["input"][0]["content"][0]["type"], "input_text");
        assert!(!p["prompt_cache_key"].as_str().unwrap().is_empty());
        assert!(p["prompt_cache_key"].as_str().unwrap().len() <= 64);
        assert!(p.get("max_output_tokens").is_none());
    }

    #[tokio::test]
    async fn probe_details_accept_json_and_sse_without_exposing_loaded_credentials() {
        use crate::types::Account;
        use std::collections::HashMap;
        use std::sync::atomic::AtomicUsize;
        for sse in [false, true] {
            let key = "opaque-primary-fixture-credential";
            let other_key = "opaque-other-fixture-credential";
            let local_token = "local-fixture-token-at-least-32-characters";
            let visible = format!(
                "OK {key} {other_key} {local_token} sk-unloaded-fixture Bearer unknown-fixture https://user:password@example.com/private"
            );
            let result = json!({"id":"resp_probe_fixture","object":"response","model":"test-model","status":"completed",
                "output":[{"type":"reasoning","encrypted_content":"private-encrypted-fixture"},
                    {"type":"message","content":[{"type":"output_text","text":visible}]}],
                "usage":{"input_tokens":7,"output_tokens":3}});
            let count = Arc::new(AtomicUsize::new(0));
            let seen = count.clone();
            let app = Router::new().route(
                "/v1/responses",
                post(move |headers: HeaderMap, Json(request): Json<Value>| {
                    let result = result.clone();
                    let seen = seen.clone();
                    async move {
                        seen.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(request["instructions"], crate::probe::REQUEST_TEXT);
                        assert_eq!(
                            request["input"][0]["content"][0]["text"],
                            crate::probe::REQUEST_TEXT
                        );
                        assert_eq!(headers["authorization"], format!("Bearer {key}"));
                        let text = if sse {
                            format!(
                                "data: {}\n\n",
                                json!({"type":"response.completed","response":result})
                            )
                        } else {
                            result.to_string()
                        };
                        Response::builder()
                            .header(
                                "content-type",
                                if sse {
                                    "text/event-stream"
                                } else {
                                    "application/json"
                                },
                            )
                            .header("x-private-fixture", "header-never-displayed")
                            .body(Body::from(text))
                            .unwrap()
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let upstream = format!("http://{}/v1", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let dir = tempfile::tempdir().unwrap();
            let config = Config {
                local_token: local_token.into(),
                max_attempts: 1,
                ..Config::default()
            };
            let state = AppState::new(config, dir.path().join("config.json"), false).unwrap();
            state.scheduler.sync_accounts(
                [("a".repeat(64), key), ("b".repeat(64), other_key)]
                    .into_iter()
                    .map(|(id, secret)| Account {
                        id,
                        secret: Arc::from(secret),
                        upstream: upstream.clone(),
                        label: "fixture".into(),
                        models: vec!["test-model".into()],
                        provider_ids: vec!["fixture".into()],
                        source_max_concurrency: 1,
                        model_concurrency: HashMap::from([("test-model".into(), 1)]),
                        last_source_success_ms: None,
                        model_source_success_ms: HashMap::new(),
                    })
                    .collect(),
            );
            let response = api_probe(
                State(state.clone()),
                Json(json!({"key_id":"a".repeat(64),"model":"test-model"})),
            )
            .await;
            let bytes = to_bytes(response.into_body(), 512 * 1024).await.unwrap();
            let result: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(result["ok"], true);
            assert_eq!(result["status"], 200);
            assert_eq!(result["details"]["usage"]["input_tokens"], 7);
            assert!(
                result["details"]["response_text"]
                    .as_str()
                    .unwrap()
                    .starts_with("OK ")
            );
            let serialized = String::from_utf8(bytes.to_vec()).unwrap();
            for forbidden in [
                key,
                other_key,
                local_token,
                "sk-unloaded-fixture",
                "unknown-fixture",
                "user:password",
                "private-encrypted-fixture",
                "header-never-displayed",
            ] {
                assert!(!serialized.contains(forbidden), "probe leaked fixture data");
            }
            assert_eq!(count.load(Ordering::SeqCst), 1);
            assert!(
                state
                    .scheduler
                    .snapshot(&state.settings().policy)
                    .iter()
                    .all(|account| account.in_flight == 0)
            );
            server.abort();
        }
    }
}
