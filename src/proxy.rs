use crate::{
    biwrite,
    config::{self, Config, SourceMode},
    conversations::ConversationIndex,
    integration, manual,
    monitor::{
        self, AttemptRecord, GatewayAction, Observation, RecordStore, RequestRecord, SseObserver,
    },
    recovery::{self, RecoveryKind, SessionRemaps},
    scheduler::{Lease, Scheduler, SelectError, Selection},
    types::{ProviderSummary, RoutingPolicy, SourceSnapshot, now_ms},
    updates::UpdateService,
    usage,
};
use anyhow::Result;
use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
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

#[derive(Clone, Debug, Default, Serialize)]
pub struct SyncState {
    pub running: bool,
    pub completed: usize,
    pub total: usize,
    pub healthy: usize,
    pub removed: usize,
    pub temporary: usize,
    pub last_completed_ms: Option<u64>,
}

/// A verification request waits as long as a loaded upstream needs for its
/// first output, so a slow model is not recorded as a temporary failure.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(60);
/// Concurrent verification checks. Per-account and per-model limits still
/// apply, so this only bounds how many candidates are checked at once.
const VERIFY_CONCURRENCY: usize = 6;

/// A cancelled manual sync must not leave the console stuck on "verifying".
struct SyncRunGuard(Arc<AppState>);

impl Drop for SyncRunGuard {
    fn drop(&mut self) {
        self.0
            .sync
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .running = false;
    }
}

pub enum ManualSyncError {
    AlreadyRunning,
    Source(anyhow::Error),
}

#[derive(Default)]
pub struct Counters {
    requests: AtomicU64,
    successes: AtomicU64,
    failures: AtomicU64,
    /// Requests naming a model the pool holds no account for at all. The
    /// upstream often does not offer that model either, so counting these as
    /// routing failures would misrepresent the pool.
    unassigned: AtomicU64,
    active: AtomicU64,
    waiting_streams: AtomicU64,
}

#[derive(Default)]
pub struct ContinuationState {
    pub streak: HashMap<String, u32>,
    /// Resumes already sent per conversation, bounded so a broken upstream
    /// cannot keep spending quota while nobody is watching.
    pub resumes: HashMap<String, u32>,
    /// The last conversation resumed and how many times, for the dashboard.
    pub last: Option<(String, u32)>,
}

pub struct AppState {
    pub config: RwLock<Config>,
    pub config_path: PathBuf,
    pub scheduler: Scheduler,
    probe_scheduler: Scheduler,
    pub records: RecordStore,
    /// Per-day token accounting. Independent of `records`, which only keeps the
    /// most recent requests and cannot answer "how much did today cost".
    pub usage: usage::Ledger,
    /// Local display metadata only. Titles never enter the persisted request log.
    pub conversations: Arc<ConversationIndex>,
    /// Consecutive failures per conversation, and the prompt offered once a
    /// conversation has failed enough times for the client to have given up.
    pub continuation: RwLock<ContinuationState>,
    pub updates: UpdateService,
    pub source: RwLock<SourceState>,
    pub sync: RwLock<SyncState>,
    pub client: reqwest::Client,
    legacy_client: reqwest::Client,
    pub started: Instant,
    pub counters: Counters,
    /// Optional connection metadata supplied by embedders or isolated tests.
    pub connection_override: RwLock<Option<integration::ConnectionInfo>>,
    /// Rust-only injection for isolated transport tests. Never exposed through config or APIs.
    pub legacy_upstream_override: RwLock<Option<String>>,
    /// Rust-only source transport injection for isolated synchronization tests.
    pub source_upstream_override: RwLock<Option<String>>,
    resolved_upstream: RwLock<Option<String>>,
    permanent_exclusions: RwLock<HashSet<(String, String, String)>>,
    session_remaps: SessionRemaps,
    settings_lock: tokio::sync::Mutex<()>,
    source_lock: tokio::sync::Mutex<()>,
    manual_sync_lock: tokio::sync::Mutex<()>,
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
        let state_dir = config_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        let usage = usage::Ledger::load(&state_dir);
        let updates = UpdateService::new(
            config_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_path_buf(),
        );
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
            probe_scheduler: Scheduler::new(),
            records,
            usage,
            conversations: Arc::new(ConversationIndex::from_environment()),
            continuation: RwLock::new(ContinuationState::default()),
            updates,
            source: RwLock::new(SourceState::default()),
            sync: RwLock::new(SyncState::default()),
            client,
            legacy_client,
            started: Instant::now(),
            counters: Counters::default(),
            connection_override: RwLock::new(None),
            legacy_upstream_override: RwLock::new(None),
            source_upstream_override: RwLock::new(None),
            resolved_upstream: RwLock::new(None),
            permanent_exclusions: RwLock::new(HashSet::new()),
            session_remaps: SessionRemaps::default(),
            settings_lock: tokio::sync::Mutex::new(()),
            source_lock: tokio::sync::Mutex::new(()),
            manual_sync_lock: tokio::sync::Mutex::new(()),
        }))
    }
    pub fn settings(&self) -> Config {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub async fn sync_source(self: &Arc<Self>) -> Result<()> {
        self.sync_source_with_options(false, false).await
    }
    pub async fn sync_source_startup(self: &Arc<Self>) -> Result<()> {
        self.sync_source_with_options(true, false).await
    }
    pub async fn sync_source_now(self: &Arc<Self>) -> Result<(), ManualSyncError> {
        let _lock = self
            .manual_sync_lock
            .try_lock()
            .map_err(|_| ManualSyncError::AlreadyRunning)?;
        self.sync_source_with_options(true, true)
            .await
            .map_err(ManualSyncError::Source)
    }
    async fn sync_source_with_options(
        self: &Arc<Self>,
        ignore_pause: bool,
        verify: bool,
    ) -> Result<()> {
        let _lock = self.source_lock.lock().await;
        let cfg = self.settings();
        if cfg.source_mode == SourceMode::Biwrite && cfg.sync_paused && !ignore_pause {
            return Ok(());
        }
        let result = self.read_source(&cfg).await;
        let current = self.settings();
        if !same_source_config(&cfg, &current) {
            return Ok(());
        }
        match result {
            Ok((snapshot, upstream)) => {
                let verified = verify && verifies_candidates(&cfg);
                let (snapshot, probe_only_pairs) = if verified {
                    self.verify_snapshot(snapshot).await
                } else {
                    (self.filtered_snapshot(&cfg, snapshot), Vec::new())
                };
                if !same_source_config(&cfg, &self.settings()) {
                    return Ok(());
                }
                *self
                    .resolved_upstream
                    .write()
                    .unwrap_or_else(|e| e.into_inner()) = Some(upstream);
                self.scheduler.sync_accounts(snapshot.accounts);
                self.scheduler
                    .adopt_probe_health(&self.probe_scheduler, &probe_only_pairs);
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
    async fn verify_snapshot(
        self: &Arc<Self>,
        snapshot: SourceSnapshot,
    ) -> (SourceSnapshot, Vec<(String, String, String)>) {
        self.probe_scheduler
            .sync_accounts(snapshot.accounts.clone());
        let pairs: Vec<(String, String, String)> = snapshot
            .accounts
            .iter()
            .flat_map(|account| {
                account
                    .models
                    .iter()
                    .map(|model| (account.upstream.clone(), account.id.clone(), model.clone()))
            })
            .collect();
        let upstreams: HashSet<String> = pairs
            .iter()
            .map(|(upstream, _, _)| upstream.clone())
            .collect();
        *self.sync.write().unwrap_or_else(|e| e.into_inner()) = SyncState {
            running: true,
            total: pairs.len(),
            ..SyncState::default()
        };
        let _running = SyncRunGuard(self.clone());
        let mut checks = stream::iter(pairs.into_iter().map(|(upstream, id, model)| {
            let state = self.clone();
            async move {
                let probe_only = !state.scheduler.contains_model(&id, &upstream, &model);
                if probe_only {
                    state.probe_scheduler.clear_cooldown(&id, Some(&model));
                }
                let result = probe_account(state, &id, &model, VERIFY_TIMEOUT, probe_only).await;
                (upstream, id, model, probe_only, result)
            }
        }))
        .buffer_unordered(VERIFY_CONCURRENCY);
        let mut exclusions = HashSet::new();
        let mut probe_only_pairs = Vec::new();
        while let Some((upstream, id, model, probe_only, result)) = checks.next().await {
            if probe_only {
                probe_only_pairs.push((upstream.clone(), id.clone(), model.clone()));
            }
            let status = result["upstream_status"]
                .as_u64()
                .and_then(|status| u16::try_from(status).ok());
            let code = result["upstream_error_code"]
                .as_str()
                .or_else(|| result["error_code"].as_str());
            let mut sync = self.sync.write().unwrap_or_else(|e| e.into_inner());
            sync.completed = sync.completed.saturating_add(1);
            if result["ok"].as_bool() == Some(true) {
                sync.healthy = sync.healthy.saturating_add(1);
            } else if permanent_probe_failure(status, code) {
                sync.removed = sync.removed.saturating_add(1);
                exclusions.insert((upstream, id, model));
            } else {
                sync.temporary = sync.temporary.saturating_add(1);
            }
        }
        let mut all_exclusions = self
            .permanent_exclusions
            .write()
            .unwrap_or_else(|e| e.into_inner());
        all_exclusions.retain(|(upstream, _, _)| !upstreams.contains(upstream));
        all_exclusions.extend(exclusions);
        let snapshot = filter_snapshot(snapshot, &all_exclusions);
        drop(all_exclusions);
        // The summary lives in the sync state, which the panel keeps showing.
        // A source warning would be replaced by the next periodic read.
        let mut sync = self.sync.write().unwrap_or_else(|e| e.into_inner());
        sync.running = false;
        sync.last_completed_ms = Some(now_ms());
        (snapshot, probe_only_pairs)
    }
    fn filtered_snapshot(&self, cfg: &Config, snapshot: SourceSnapshot) -> SourceSnapshot {
        if verifies_candidates(cfg) {
            let exclusions = self
                .permanent_exclusions
                .read()
                .unwrap_or_else(|e| e.into_inner());
            filter_snapshot(snapshot, &exclusions)
        } else {
            snapshot
        }
    }
    pub fn state_dir(&self) -> PathBuf {
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
        let transport_override = (mode == SourceMode::Biwrite)
            .then(|| {
                self.source_upstream_override
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            })
            .flatten();
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || {
            let upstream = cfg.selected_upstream()?;
            let mut snapshot = match mode {
                SourceMode::Biwrite => {
                    biwrite::load_snapshot_with_scope(&dir, &upstream, cfg.biwrite_import_all)
                }
                SourceMode::Manual => manual::load_snapshot(&dir, &upstream),
            }?;
            if let Some(transport) = transport_override {
                for account in &mut snapshot.accounts {
                    account.upstream = transport.clone();
                }
            }
            Ok((snapshot, upstream))
        })
        .await?
    }
}

fn same_source_config(left: &Config, right: &Config) -> bool {
    left.source_mode == right.source_mode
        && left.upstream_base_url == right.upstream_base_url
        && left.biwrite_import_all == right.biwrite_import_all
}

fn verifies_candidates(cfg: &Config) -> bool {
    cfg.source_mode == SourceMode::Biwrite && !cfg.biwrite_import_all
}

fn permanent_probe_failure(status: Option<u16>, code: Option<&str>) -> bool {
    matches!(status, Some(401 | 403))
        || matches!(
            code,
            Some(
                "authentication_error"
                    | "invalid_api_key"
                    | "unauthorized"
                    | "permission_denied"
                    | "insufficient_quota"
                    | "quota_exceeded"
                    | "model_not_found"
                    | "unsupported_model"
            )
        )
}

fn filter_snapshot(
    mut snapshot: SourceSnapshot,
    exclusions: &HashSet<(String, String, String)>,
) -> SourceSnapshot {
    snapshot.accounts.retain_mut(|account| {
        account.models.retain(|model| {
            !exclusions.contains(&(account.upstream.clone(), account.id.clone(), model.clone()))
        });
        account
            .model_concurrency
            .retain(|model, _| account.models.contains(model));
        account
            .model_source_success_ms
            .retain(|model, _| account.models.contains(model));
        account.provider_ids.retain(|provider| {
            snapshot
                .providers
                .iter()
                .any(|summary| &summary.id == provider && account.models.contains(&summary.model))
        });
        !account.models.is_empty()
    });
    for provider in &mut snapshot.providers {
        provider.imported_keys = snapshot
            .accounts
            .iter()
            .filter(|account| {
                account.provider_ids.contains(&provider.id)
                    && account.models.contains(&provider.model)
            })
            .count();
    }
    snapshot
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
        .route("/api/usage", get(api_usage))
        .route("/api/config/files", get(api_config_files))
        .route("/api/usage/pricing", post(api_usage_pricing))
        .route("/api/records/clear", post(api_clear))
        .route("/api/update/check", post(api_update_check))
        .route("/api/update/download", post(api_update_download))
        .route("/api/config", post(api_config))
        .route("/api/sync", post(api_sync))
        .route("/api/accounts/{id}/reset", post(api_reset))
        .route("/api/probe", post(api_probe))
        .route("/api/connect", post(api_connect))
        .route("/api/restore", post(api_restore))
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
        "source":state.source.read().unwrap_or_else(|e|e.into_inner()).clone(),"sync":state.sync.read().unwrap_or_else(|e|e.into_inner()).clone(),"accounts":accounts,"models":models,"config":cfg.public(),
        "connection":connection,"continuation":state.continuation.read().unwrap_or_else(|e|e.into_inner()).last.as_ref().map(|(id,n)|json!({"conversation_id":id,"resumes":n})),"stats":{"requests":state.counters.requests.load(Ordering::Relaxed),"successes":state.counters.successes.load(Ordering::Relaxed),"failures":state.counters.failures.load(Ordering::Relaxed),"unassigned":state.counters.unassigned.load(Ordering::Relaxed),"active":state.counters.active.load(Ordering::Relaxed),"waiting_streams":state.counters.waiting_streams.load(Ordering::Relaxed)}}),
    )
}
/// Read-only view of the two files that decide behaviour, with every
/// credential masked. Editing stays in the forms, which validate before saving.
async fn api_config_files(State(s): State<Arc<AppState>>) -> Response {
    let own_path = s.config_path.clone();
    let files = tokio::task::spawn_blocking(move || {
        let own = std::fs::read_to_string(&own_path).unwrap_or_default();
        let (codex_path, codex) = integration::codex_config_preview()
            .unwrap_or_else(|_| ("未找到 Codex 配置".into(), String::new()));
        json!([
            {
                "name": "Pool Router 配置",
                "path": own_path.to_string_lossy(),
                "content": integration::mask_json_secrets(&own),
                "note": "路由设置页的改动会写回这个文件。手工编辑后需要重启本地服务。",
            },
            {
                "name": "Codex 配置",
                "path": codex_path,
                "content": codex,
                "note": "「直接接入 Codex」会写入其中的 model_providers.pool_router 段，包含与号池一致的流重试次数。",
            }
        ])
    })
    .await
    .unwrap_or_else(|_| json!([]));
    Json(json!({"files": files})).into_response()
}

/// Token totals per day, with the price table that produced the estimates.
async fn api_usage(State(s): State<Arc<AppState>>, Query(q): Query<UsageQuery>) -> Response {
    let days = q.days.unwrap_or(30).clamp(1, 400);
    let today = usage::local_date(now_ms(), s.usage.pricing().day_offset_minutes);
    let mut value = s.usage.snapshot(days, &today);
    // Conversation names come from the local Codex index, like the monitor does,
    // so the usage page reads the same labels without loading the monitor.
    let ids: Vec<String> = value["by_conversation"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if !ids.is_empty() {
        let index = s.conversations.clone();
        let titles = tokio::task::spawn_blocking(move || index.lookup(&ids))
            .await
            .unwrap_or_default();
        if let Some(rows) = value["by_conversation"].as_array_mut() {
            for row in rows {
                if let Some(title) = row["name"].as_str().and_then(|id| titles.get(id)) {
                    row["title"] = json!(title);
                }
            }
        }
    }
    Json(value).into_response()
}

#[derive(Deserialize)]
struct UsageQuery {
    days: Option<usize>,
}

async fn api_usage_pricing(State(s): State<Arc<AppState>>, Json(v): Json<Value>) -> Response {
    let Ok(pricing) = serde_json::from_value::<usage::Pricing>(v) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_pricing",
            "价目表字段无效，未修改原文件",
        );
    };
    let dir = s.state_dir();
    match s.usage.set_pricing(&dir, pricing) {
        Ok(()) => Json(json!({"ok": true, "pricing": s.usage.pricing()})).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, "invalid_pricing", &e.to_string()),
    }
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
async fn api_update_check(State(s): State<Arc<AppState>>) -> Response {
    match s.updates.check().await {
        Ok(info) => Json(info).into_response(),
        Err(error_info) => error(
            StatusCode::BAD_GATEWAY,
            "update_check_failed",
            &error_info.to_string(),
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateDownloadRequest {
    version: String,
}

async fn api_update_download(
    State(s): State<Arc<AppState>>,
    Json(request): Json<UpdateDownloadRequest>,
) -> Response {
    let download = match s.updates.download(&request.version).await {
        Ok(download) => download,
        Err(error_info) => {
            return error(
                StatusCode::BAD_REQUEST,
                "update_download_failed",
                &error_info.to_string(),
            );
        }
    };
    #[cfg(target_os = "macos")]
    let mut opener = {
        let mut command = std::process::Command::new("/usr/bin/open");
        command.arg(&download.path);
        command
    };
    #[cfg(target_os = "windows")]
    let mut opener = std::process::Command::new(&download.path);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut opener = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(&download.path);
        command
    };
    match opener
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => {
            drop(child);
            Json(json!({
                "ok": true,
                "opened": true,
                "version": download.version,
                "filename": download.filename,
                "message": "安装包已通过 SHA-256 校验，已请求系统打开。完成安装后刷新控制台，现有配置会保留。"
            }))
            .into_response()
        }
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "update_open_failed",
            "安装包已通过校验，但系统未能打开；请重试或从 GitHub Release 手动下载安装。",
        ),
    }
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
            Ok((snapshot, upstream)) => Some((s.filtered_snapshot(&updated, snapshot), upstream)),
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
        Ok(()) => {
            let sync = s.sync.read().unwrap_or_else(|e| e.into_inner()).clone();
            Json(json!({"ok": true, "sync": sync})).into_response()
        }
        Err(ManualSyncError::AlreadyRunning) => error(
            StatusCode::CONFLICT,
            "sync_in_progress",
            "同步并验证正在进行，请稍后查看进度",
        ),
        Err(ManualSyncError::Source(e)) => error(
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
        integration::connect(
            &cfg.base_url(),
            &cfg.local_token,
            &dir,
            u32::try_from(cfg.max_attempts.saturating_sub(1).max(1)).unwrap_or(15),
            u32::try_from(cfg.queue_timeout_ms).unwrap_or(600_000),
        )
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

/// Undo this software's own changes to the Codex configuration. The panel needs
/// this so disconnecting does not require the command line.
async fn api_restore(State(s): State<Arc<AppState>>) -> Response {
    let dir = s.state_dir();
    match tokio::task::spawn_blocking(move || integration::restore(&dir)).await {
        Ok(Ok(message)) => Json(json!({"ok":true,"message":message})).into_response(),
        Ok(Err(e)) => error(StatusCode::CONFLICT, "restore_failed", &e.to_string()),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "restore_failed",
            "断开接入未完成",
        ),
    }
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
    Json(probe_account(s, key, model, Duration::from_secs(120), false).await).into_response()
}

async fn probe_account(
    s: Arc<AppState>,
    key: &str,
    model: &str,
    timeout: Duration,
    sync_probe: bool,
) -> Value {
    let body = probe_payload(key, model);
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + timeout;
    let local_token = s.settings().local_token;
    let probe_secrets = if sync_probe {
        s.probe_scheduler.credential_snapshot()
    } else {
        s.scheduler.credential_snapshot()
    };
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
        run_proxy_inner(
            s.clone(),
            Uri::from_static("/v1/responses"),
            headers,
            Bytes::from(body.to_string()),
            sync_probe,
        ),
    )
    .await;
    let response = match response {
        Ok(response) => response,
        Err(_) => {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let mut collector = crate::probe::Collector::new(true);
            collector.add_event("request_started", 0, "开始固定合成验证请求。");
            collector.add_event(
                "probe_timeout",
                elapsed_ms,
                &format!("验证超过 {} 秒，已停止等待。", timeout.as_secs()),
            );
            let details =
                collector.details(None, model, elapsed_ms, Some("probe_timeout"), sanitize);
            return json!({"ok":false,"status":504,"message":"验证超时","response_model":null,"error_code":"probe_timeout","details":details});
        }
    };
    let probe_upstream = response
        .extensions()
        .get::<Arc<RwLock<ProbeUpstream>>>()
        .cloned()
        .unwrap_or_default();
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
    let upstream = probe_upstream
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let upstream_status = upstream.status;
    let upstream_error_code = upstream.error_code;
    let record = request_id.as_ref().and_then(|id| {
        s.records
            .records()
            .into_iter()
            .find(|record| &record.id == id)
    });
    if let Some(record) = &record {
        collector.record_events(record);
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
    json!({"ok":ok,"status":status.as_u16(),"message":if ok{"验证成功"}else{"本次验证未成功，请查看详细日志"},"response_model":response_model,"error_code":error_code,"details":details,"upstream_status":upstream_status,"upstream_error_code":upstream_error_code})
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
            // An upstream chunk can end mid line. A comment inserted there would
            // split an event and the client would fail to decode the body, so
            // heartbeats only go out while the stream sits on a line boundary.
            let mut at_line_boundary=true;
            loop {
                tokio::select! {
                    part=body.next()=>match part {
                        Some(Ok(bytes))=>{
                            if let Some(last)=bytes.last() { at_line_boundary=*last==b'\n'; }
                            yield Ok(bytes);
                        },
                        Some(Err(_))=>{yield Err(io::Error::other("上游流连接中断"));break;},
                        None=>break,
                    },
                    _=heartbeat.tick(),if at_line_boundary=>yield Ok(Bytes::from_static(b": pool-router waiting\n\n")),
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

#[derive(Clone, Debug, Default)]
struct ProbeUpstream {
    status: Option<u16>,
    error_code: Option<String>,
}

struct RequestGuard {
    state: Arc<AppState>,
    scheduler: Scheduler,
    probe_upstream: Arc<RwLock<ProbeUpstream>>,
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
            scheduler: state.scheduler.clone(),
            state,
            probe_upstream: Arc::new(RwLock::new(ProbeUpstream::default())),
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
        self.record.cache_write_tokens = o.cache_write_tokens;
        self.record.cache_write_30m_tokens = o.cache_write_30m_tokens;
        self.record.cache_write_12h_tokens = o.cache_write_12h_tokens;
        self.record.total_tokens = o.total_tokens;
        self.record.reasoning_tokens = o.reasoning_tokens;
        if self.record.route_mode != "legacy"
            && let (Some(id), Some(key), Some(model)) = (
                &o.response_id,
                &self.record.key_id,
                &self.record.effective_model,
            )
        {
            self.scheduler.bind_response_with_session(
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
    fn set_probe_upstream(&self, status: Option<u16>, error_code: Option<String>) {
        *self
            .probe_upstream
            .write()
            .unwrap_or_else(|e| e.into_inner()) = ProbeUpstream { status, error_code };
    }
    fn with_probe_upstream(&self, mut response: Response) -> Response {
        response
            .extensions_mut()
            .insert(self.probe_upstream.clone());
        response
    }
    fn error(&self, status: StatusCode, code: &str, message: &str) -> Response {
        let mut response = error(status, code, message);
        if let Ok(value) = HeaderValue::from_str(&self.record.id) {
            response
                .headers_mut()
                .insert("x-pool-router-request-id", value);
        }
        self.with_probe_upstream(response)
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
        } else if self.record.outcome == "no_candidate" {
            self.state
                .counters
                .unassigned
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.state.counters.failures.fetch_add(1, Ordering::Relaxed);
        }
        // A conversation that keeps failing has usually exhausted the client's
        // own retries too, leaving the user with a dead turn and no thread id
        // at hand. Count the streak here so every way a request can end feeds
        // it, and offer the prompt that resumes the work.
        let settings = self.state.settings();
        if settings.continuation_after_failures > 0
            && let Some(conversation) = self.record.conversation_id.clone()
        {
            let completed = self.record.outcome == "completed";
            let mut state = self
                .state
                .continuation
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if completed {
                state.streak.remove(&conversation);
                state.resumes.remove(&conversation);
            } else {
                let streak = state
                    .streak
                    .entry(conversation.clone())
                    .and_modify(|n| *n = n.saturating_add(1))
                    .or_insert(1);
                let reached = *streak >= settings.continuation_after_failures;
                let resumed = state.resumes.get(&conversation).copied().unwrap_or(0);
                if reached && resumed < settings.auto_resume_max {
                    let attempt = resumed + 1;
                    state.resumes.insert(conversation.clone(), attempt);
                    // Resetting the streak stops one stalled conversation from
                    // queueing a message on every later failure.
                    state.streak.insert(conversation.clone(), 0);
                    state.last = Some((conversation.clone(), attempt));
                    let id = conversation.clone();
                    drop(state);
                    // Codex talks to its own daemon, which can take a moment.
                    std::thread::spawn(move || {
                        let sent = resume_conversation(&id);
                        let _ = (attempt, sent);
                    });
                    return;
                }
                // Bound the map so a long session cannot grow it without end.
                if state.streak.len() > 512 {
                    state.streak.clear();
                }
            }
        }
        // Accounting is independent of the monitor: turning request logging off
        // hides the details, not the daily totals.
        self.state.usage.record(&self.record);
        if self.recording {
            let _ = self.state.records.record(self.record.clone());
        }
    }
}

/// Higher reasoning effort takes measurably longer to produce its first
/// meaningful token, so one fixed deadline either kills healthy slow requests
/// or lets dead ones linger. Measured on a live pool over 583 completed
/// requests: the 99th percentile of time to first output was 90s at low, 195s
/// at high and 270s at xhigh. The base covers low, and the higher tiers scale
/// from it. A stream that sends nothing at all is still cut by the idle
/// timeout, which is independent of this.
fn first_output_budget(base_ms: u64, effort: Option<&str>) -> u64 {
    let scaled = match effort.unwrap_or("").to_ascii_lowercase().as_str() {
        "minimal" | "low" => base_ms,
        "high" => base_ms.saturating_mul(2),
        "xhigh" | "max" | "ultra" => base_ms.saturating_mul(5) / 2,
        // Unspecified or medium sits between low and high.
        _ => base_ms.saturating_mul(3) / 2,
    };
    scaled.min(600_000)
}

/// A failure that every account in the pool will hit the same way, because it
/// is the model or the relay that is unavailable rather than one key. Walking
/// the pool for these only spends the retry budget, so the whole model waits.
/// Matching is on the upstream's own wording; anything unrecognized keeps the
/// existing per-account behaviour.
fn pool_wide_failure(status: Option<u16>, message: Option<&str>) -> bool {
    let Some(status) = status else { return false };
    if !(500..=599).contains(&status) {
        return false;
    }
    // A bare 5xx is ambiguous: it can be one account's own transient failure,
    // which the next account may well survive. Only the upstream's own wording
    // for "the model or my own upstream is unavailable" is treated as shared,
    // so an unrecognized error keeps the ordinary per-account retry path.
    let message = message.unwrap_or("").to_ascii_lowercase();
    const SHARED_OUTAGE: [&str; 7] = [
        "负载已经达到上限",
        "负载已达上限",
        "当前模型负载",
        "请求上游地址失败",
        "overloaded",
        "at capacity",
        "bad response status code 50",
    ];
    SHARED_OUTAGE.iter().any(|needle| message.contains(needle))
}

/// Reasoning tiers in increasing order of depth, so a response can be compared
/// with what was asked for. Unknown names rank as unknown and never trigger a
/// downgrade verdict.
fn effort_rank(effort: &str) -> Option<u8> {
    match effort.to_ascii_lowercase().as_str() {
        "minimal" | "none" => Some(0),
        "low" => Some(1),
        "medium" => Some(2),
        "high" => Some(3),
        "xhigh" | "max" | "ultra" => Some(4),
        _ => None,
    }
}

/// Whether the upstream answered with a shallower model or reasoning tier than
/// the request asked for. The first streamed event already carries the model
/// and effort the upstream committed to, so this is decided before any content
/// reaches the client and the attempt can still move to another account.
fn served_below_request(
    requested_model: &str,
    requested_effort: Option<&str>,
    served_model: Option<&str>,
    served_effort: Option<&str>,
) -> bool {
    if let Some(served) = served_model
        && !served.is_empty()
        && !served.eq_ignore_ascii_case(requested_model)
    {
        return true;
    }
    let (Some(asked), Some(served)) = (
        requested_effort.and_then(effort_rank),
        served_effort.and_then(effort_rank),
    ) else {
        return false;
    };
    served < asked
}

/// The Codex command line that the desktop application actually runs. It is
/// installed under the user's home rather than on PATH, and the copy on PATH
/// may be a broken wrapper, so the bundled one is preferred.
fn codex_command() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let bundled =
        home.join(".codex/plugins/.plugin-appserver/codex-cli/CodexCLI.app/Contents/MacOS/codex");
    bundled.is_file().then_some(bundled)
}

/// Ask Codex to carry on with a conversation that has stopped making progress.
/// The message is queued onto the existing thread, so the agent keeps the
/// context it already has instead of starting over. Returns whether Codex
/// accepted it.
fn resume_conversation(conversation_id: &str) -> bool {
    let Some(command) = codex_command() else {
        return false;
    };
    let output = std::process::Command::new(command)
        .arg("queue")
        .arg("--thread")
        .arg(conversation_id)
        .arg("--message")
        .arg("继续")
        .stdin(std::process::Stdio::null())
        .output();
    // The command exits zero even when the thread is unknown, so the message
    // on standard error is what says whether anything was queued.
    match output {
        Ok(out) => out.status.success() && !String::from_utf8_lossy(&out.stderr).contains("Error:"),
        Err(_) => false,
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
        // A chunk may end mid line; a comment inserted there would split an event.
        let mut at_line_boundary = true;
        loop {
            // Local heartbeats must not reset the upstream idle deadline.
            let part = {
                let pending = tokio::time::timeout(Duration::from_millis(cfg.stream_idle_timeout_ms), upstream_stream.next());
                tokio::pin!(pending);
                loop {
                    tokio::select! {
                        result = &mut pending => break result,
                        _ = heartbeat.tick(), if add_heartbeats && at_line_boundary => yield Ok(Bytes::from_static(b": pool-router legacy waiting\n\n")),
                    }
                }
            };
            match part {
                Ok(Some(Ok(bytes))) => {
                    if let Some(last) = bytes.last() { at_line_boundary = *last == b'\n'; }
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
    run_proxy_inner(state, uri, headers, raw, false).await
}

async fn run_proxy_inner(
    state: Arc<AppState>,
    uri: Uri,
    headers: HeaderMap,
    raw: Bytes,
    sync_probe: bool,
) -> Response {
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
    let scheduler = if sync_probe {
        &state.probe_scheduler
    } else {
        &state.scheduler
    };
    let mut effective_model = if probe {
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
    // A client may ask for a model this upstream does not carry at all. Rather
    // than fail the request, fall through the configured substitutes and take
    // the first one the pool can serve right now, so a dead primary does not
    // decide the outcome. Probes keep the model they were asked to check.
    let aliased_from = if probe || !initial.model_aliases_enabled {
        None
    } else {
        initial
            .model_aliases
            .get(&effective_model)
            .filter(|_| {
                !state
                    .scheduler
                    .can_serve_model(&effective_model, &initial.policy)
            })
            .and_then(|targets| {
                targets
                    .iter()
                    .find(|target| state.scheduler.can_serve_model(target, &initial.policy))
                    .cloned()
            })
    };
    if let Some(target) = &aliased_from {
        effective_model = target.clone();
    }
    // Captured before the value is moved; the retry loop sizes each attempt's
    // first-output budget from it.
    let effort_for_budget = effective_effort.clone();
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
    guard.scheduler = scheduler.clone();
    if aliased_from.is_some() {
        guard.action("model_alias", 0, 1);
    }
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
    let max_attempts = if probe { 1 } else { initial.max_attempts };
    let mut attempt_count = 0usize;
    let mut recovery_state = RequestRecovery::default();
    let mut downgrade_retries_left = initial.downgrade_retries;
    let mut judged_attempts = 0usize;
    let mut pool_wide_failures = 0usize;
    let mut retry_same_key: Option<String> = None;
    'attempts: loop {
        let cfg = state.settings();
        if Instant::now() >= deadline || attempt_count >= max_attempts {
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
        // The previous attempt's verdict decides whether another account is even
        // worth trying. This sits ahead of acquisition so every failure branch
        // feeds it without each having to remember to.
        // Only ever judge a given attempt once. Paths that loop without adding
        // an attempt record would otherwise re-judge the same failure and back
        // off again on every pass, spending the whole deadline on waiting.
        if guard.record.attempts.len() > judged_attempts
            && let Some(last) = guard.record.attempts.last()
            && pool_wide_failure(last.status, last.error_message.as_deref())
        {
            // Retrying past a few rounds cannot help while the model itself is
            // unavailable, and the caller gives up long before this request's
            // own deadline. Say so quickly instead of holding the connection.
            const GIVE_UP_AFTER: usize = 3;
            pool_wide_failures += 1;
            if pool_wide_failures >= GIVE_UP_AFTER {
                let detail = guard
                    .record
                    .attempts
                    .last()
                    .and_then(|attempt| attempt.error_message.clone())
                    .unwrap_or_else(|| "上游未给出原因".to_owned());
                guard.record.status = Some(503);
                guard.finish("model_unavailable");
                let mut response = guard.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "model_unavailable",
                    &format!(
                        "{effective_model} 在上游侧整体不可用，号池里每个账号都得到同样结果，已停止重试。上游原因：{detail}"
                    ),
                );
                response
                    .headers_mut()
                    .insert("retry-after", HeaderValue::from_static("30"));
                return response;
            }
            let delay = scheduler.cool_model(&effective_model);
            guard.action("model_backoff", attempt_count, 1);
            let wait = Duration::from_millis(delay)
                .min(deadline.saturating_duration_since(Instant::now()));
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
        judged_attempts = guard.record.attempts.len();
        let mut attempt_selection = selection.clone();
        if let Some(key) = &retry_same_key {
            attempt_selection.explicit_key = Some(key.clone());
            attempt_selection.exclude.remove(key);
        }
        let lease = match scheduler.try_acquire(&attempt_selection, &cfg.policy) {
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
                let mut names: Vec<String> = state
                    .scheduler
                    .snapshot(&initial.policy)
                    .into_iter()
                    .filter(|account| account.present)
                    .flat_map(|account| account.models)
                    .collect();
                names.sort();
                names.dedup();
                let available = names.join("、");
                let asked = guard.record.requested_model.clone().unwrap_or_default();
                let detail = if available.is_empty() {
                    format!("号池里没有 {asked} 的账号，当前号池为空")
                } else {
                    format!(
                        "号池里没有 {asked} 的账号；现在可用的模型是 {available}。上游也可能不提供该模型"
                    )
                };
                return guard.error(StatusCode::SERVICE_UNAVAILABLE, "no_candidate", &detail);
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
        let mut downgraded_on_this_attempt = false;
        let attempt_started = Instant::now();
        let first_output_deadline = attempt_started
            + Duration::from_millis(first_output_budget(
                cfg.first_output_timeout_ms,
                effort_for_budget.as_deref(),
            ));
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
            match scheduler.response_session(
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
        let build = |secret: &str, endpoint: &str| {
            state
                .client
                .post(endpoint)
                .headers(forward_headers(&attempt_headers, attempt_rewritten))
                .bearer_auth(secret)
                .body(attempt_payload.clone())
        };
        let request = build(lease.account.secret.as_ref(), &target);
        let budget = Duration::from_millis(cfg.upstream_header_timeout_ms).min(
            deadline
                .min(first_output_deadline)
                .saturating_duration_since(Instant::now()),
        );
        // A second account is raced only while no response headers exist, so no
        // output can ever be duplicated. The loser is dropped, which cancels it.
        // A continuation is pinned to the account that holds its session, so it
        // has nowhere else to go and is never raced.
        let hedgeable = attempt_selection.previous_response_id.is_none();
        let mut hedge: Option<(Lease, String)> = None;
        let sent = if !hedgeable
            || cfg.hedge_after_ms == 0
            || cfg.hedge_after_ms as u128 >= budget.as_millis()
        {
            tokio::time::timeout(budget, request.send()).await
        } else {
            let started = Instant::now();
            let primary = request.send();
            tokio::pin!(primary);
            let first = tokio::select! {
                result = &mut primary => Some(result),
                _ = tokio::time::sleep(Duration::from_millis(cfg.hedge_after_ms)) => None,
            };
            match first {
                Some(result) => Ok(result),
                None => {
                    let mut spare = attempt_selection.clone();
                    spare.explicit_key = None;
                    spare.exclude.insert(key.clone());
                    match scheduler.try_acquire(&spare, &cfg.policy) {
                        Ok(second) => {
                            let endpoint = format!(
                                "{}{}",
                                second.account.upstream.trim_end_matches('/'),
                                suffix
                            );
                            let secondary = build(second.account.secret.as_ref(), &endpoint);
                            let second_key = second.account.id.clone();
                            guard.action("hedged_attempt", attempt_count, 1);
                            let remaining = budget.saturating_sub(started.elapsed());
                            tokio::time::timeout(remaining, async {
                                tokio::select! {
                                    result = &mut primary => result,
                                    result = secondary.send() => {
                                        hedge = Some((second, second_key));
                                        result
                                    },
                                }
                            })
                            .await
                        }
                        Err(_) => {
                            tokio::time::timeout(budget.saturating_sub(started.elapsed()), primary)
                                .await
                        }
                    }
                }
            }
        };
        // The hedge won: adopt its account for the rest of this attempt.
        let (lease, key): (Lease, String) = match hedge {
            Some((winner, winner_key)) => {
                drop(lease);
                guard.action("hedged_win", attempt_count, 1);
                guard.record.key_id = Some(winner_key.clone());
                (winner, winner_key)
            }
            None => (lease, key),
        };
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
                scheduler.mark_failure(&key, &effective_model, None, None, reason, &cfg.policy);
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
                scheduler.mark_failure(&key, &effective_model, None, None, reason, &cfg.policy);
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
                    scheduler.mark_failure(
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
                scheduler.mark_failure(
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
            guard.set_probe_upstream(
                Some(status.as_u16()),
                observation
                    .error_code
                    .clone()
                    .or_else(|| failed.then(|| reason.to_owned())),
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
            if failed {
                scheduler.mark_failure(
                    &key,
                    &effective_model,
                    Some(health_status(status.as_u16(), &observation)),
                    retry_delay,
                    reason,
                    &cfg.policy,
                );
            } else {
                scheduler.mark_success(&key, &effective_model);
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
            return guard.with_probe_upstream(response);
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
                    // The upstream has named the model and tier it will use and
                    // nothing has reached the client yet, so a downgrade can
                    // still be answered by moving to another account.
                    // Still inside the prelude, which is forwarded only after
                    // this loop ends, so nothing has reached the client yet even
                    // once content has arrived from the upstream.
                    if downgrade_retries_left > 0
                        && served_below_request(
                            &effective_model,
                            effort_for_budget.as_deref(),
                            o.model.as_deref(),
                            o.effort.as_deref(),
                        )
                    {
                        downgraded_on_this_attempt = true;
                        break;
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
                        scheduler.mark_failure(
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
                    scheduler.mark_failure(
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
        // Nothing has been forwarded yet, so the shallow answer can be dropped
        // and another account asked. The account is excluded for this request
        // only; its record is what steers later selection.
        if downgraded_on_this_attempt {
            downgrade_retries_left = downgrade_retries_left.saturating_sub(1);
            scheduler.note_depth(&key, &effective_model, true);
            guard.record.attempts.push(AttemptRecord {
                error_message: None,
                key_id: key.clone(),
                status: Some(status.as_u16()),
                reason: "served_below_request".into(),
                duration_ms: attempt_started.elapsed().as_millis() as u64,
            });
            guard.action("downgrade_retry", attempt_count, 1);
            guard.record.effort_mismatch = observation.effort.clone();
            selection.exclude.insert(key.clone());
            continue 'attempts;
        }
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
            guard.set_probe_upstream(
                Some(status.as_u16()),
                Some(
                    observation
                        .error_code
                        .clone()
                        .unwrap_or_else(|| reason.to_owned()),
                ),
            );
            scheduler.mark_failure(
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
        let response_scheduler = scheduler.clone();
        let probe_upstream = guard.probe_upstream.clone();
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
            record_guard.set_probe_upstream(Some(status.as_u16()),if completed{None}else{Some(final_observation.error_code.clone().unwrap_or_else(||reason.to_owned()))});
            if completed {
                // A completed answer that kept the requested tier is the
                // evidence that makes this account preferred later.
                response_scheduler.note_depth(&key,&effective_model,false);
                response_scheduler.mark_success(&key,&effective_model);
            }else{response_scheduler.mark_failure(&key,&effective_model,Some(health_status(status.as_u16(),final_observation)),retry_delay,reason,&policy);}
            let error_message=observed_error_message(&state,final_observation,&[cfg.local_token.as_str(),_lease.account.secret.as_ref()]);
            record_guard.record.attempts.push(AttemptRecord{error_message,key_id:key,status:Some(status.as_u16()),reason:reason.into(),duration_ms:attempt_started.elapsed().as_millis() as u64});
            record_guard.finish(if completed{"completed"}else{"upstream_failed"});
        };
        let mut response = Response::new(Body::from_stream(stream));
        response.extensions_mut().insert(probe_upstream);
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

    #[test]
    fn permanent_probe_failure_is_narrow_and_temporary_failures_are_retained() {
        for code in [
            "authentication_error",
            "invalid_api_key",
            "unauthorized",
            "permission_denied",
            "insufficient_quota",
            "quota_exceeded",
            "model_not_found",
            "unsupported_model",
        ] {
            assert!(permanent_probe_failure(None, Some(code)));
        }
        assert!(permanent_probe_failure(Some(401), None));
        assert!(permanent_probe_failure(Some(403), Some("unknown_error")));
        for (status, code) in [
            (Some(429), Some("rate_limit_exceeded")),
            (Some(500), Some("upstream_error")),
            (None, Some("connect_failed")),
            (None, Some("probe_timeout")),
            (None, Some("probe_incomplete")),
            (Some(400), Some("bad_request")),
        ] {
            assert!(!permanent_probe_failure(status, code));
        }
    }

    #[test]
    fn snapshot_filter_is_model_scoped_and_recounts_providers() {
        use crate::types::Account;
        let account = |id: &str| Account {
            id: id.into(),
            secret: Arc::from("unit-test-fixture-credential"),
            upstream: "http://127.0.0.1:1/v1".into(),
            label: "fixture".into(),
            models: vec!["astra".into(), "sol".into()],
            provider_ids: vec!["p-astra".into(), "p-sol".into()],
            source_max_concurrency: 2,
            model_concurrency: HashMap::from([("astra".into(), 1), ("sol".into(), 2)]),
            last_source_success_ms: None,
            model_source_success_ms: HashMap::from([("astra".into(), 1), ("sol".into(), 2)]),
        };
        let provider = |id: &str, model: &str| ProviderSummary {
            id: id.into(),
            name: model.into(),
            model: model.into(),
            total_keys: 3,
            kept_keys: 2,
            imported_keys: 2,
        };
        let snapshot = SourceSnapshot {
            accounts: vec![account("a"), account("b")],
            providers: vec![provider("p-astra", "astra"), provider("p-sol", "sol")],
            warnings: Vec::new(),
            read_at_ms: 1,
        };
        let unrelated_upstream =
            HashSet::from([("http://127.0.0.1:2/v1".into(), "a".into(), "astra".into())]);
        assert_eq!(
            filter_snapshot(snapshot.clone(), &unrelated_upstream)
                .accounts
                .len(),
            2
        );
        let exclusions = HashSet::from([
            ("http://127.0.0.1:1/v1".into(), "a".into(), "astra".into()),
            ("http://127.0.0.1:1/v1".into(), "b".into(), "astra".into()),
            ("http://127.0.0.1:1/v1".into(), "b".into(), "sol".into()),
        ]);
        let filtered = filter_snapshot(snapshot, &exclusions);
        assert_eq!(filtered.accounts.len(), 1);
        assert_eq!(filtered.accounts[0].models, ["sol"]);
        assert_eq!(filtered.accounts[0].provider_ids, ["p-sol"]);
        assert_eq!(filtered.accounts[0].model_concurrency.len(), 1);
        assert_eq!(filtered.accounts[0].model_source_success_ms.len(), 1);
        assert_eq!(filtered.providers[0].imported_keys, 0);
        assert_eq!(filtered.providers[1].imported_keys, 1);
        assert!(filtered.providers.iter().all(|p| p.kept_keys == 2));
    }
}

#[cfg(test)]
mod adaptive_tests {
    use super::{
        codex_command, effort_rank, first_output_budget, pool_wide_failure, resume_conversation,
        served_below_request,
    };

    #[test]
    fn the_first_output_budget_follows_the_measured_cost_of_each_effort() {
        // Measured 99th percentiles on a live pool: 90s at low, 195s at high,
        // 270s at xhigh. Each tier's budget has to clear its own percentile.
        let base = 120_000;
        assert_eq!(first_output_budget(base, Some("low")), 120_000);
        assert_eq!(first_output_budget(base, Some("minimal")), 120_000);
        assert_eq!(first_output_budget(base, None), 180_000);
        assert_eq!(first_output_budget(base, Some("medium")), 180_000);
        assert!(first_output_budget(base, Some("high")) >= 195_000);
        assert!(first_output_budget(base, Some("xhigh")) >= 270_000);
        // Codex sends xhigh; the upstream reports it back as max, and a user
        // may write ultra in the client configuration.
        assert_eq!(
            first_output_budget(base, Some("XHigh")),
            first_output_budget(base, Some("max"))
        );
        assert_eq!(
            first_output_budget(base, Some("ultra")),
            first_output_budget(base, Some("xhigh"))
        );
        // An operator who shortens the base still gets the same ordering.
        assert!(
            first_output_budget(20_000, Some("low")) < first_output_budget(20_000, Some("high"))
        );
        assert!(first_output_budget(10_000_000, Some("xhigh")) <= 600_000);
    }

    /// Only runs when a thread id is supplied, because it queues a real
    /// message into that conversation and makes the agent work.
    #[test]
    fn resuming_reaches_a_real_conversation() {
        let Ok(thread) = std::env::var("POOL_ROUTER_TEST_THREAD") else {
            return;
        };
        assert!(
            resume_conversation(&thread),
            "Codex did not accept the resume for {thread}"
        );
    }

    #[test]
    fn resuming_reports_failure_for_a_thread_codex_does_not_have() {
        // The real check is that a resume is only reported as sent when Codex
        // actually took it. `codex queue` exits zero even for an unknown
        // thread, so a naive success check would report every resume as sent
        // and the cap would be spent on conversations that were never resumed.
        if codex_command().is_none() {
            return; // Codex is not installed here; nothing to talk to.
        }
        assert!(!resume_conversation("00000000-0000-4000-8000-000000000000"));
    }

    #[test]
    fn a_shallower_model_or_tier_than_was_asked_for_counts_as_a_downgrade() {
        // Codex asks for xhigh; the upstream reports the same tier as max.
        assert!(!served_below_request(
            "gpt-6.1-sol",
            Some("xhigh"),
            Some("gpt-6.1-sol"),
            Some("max")
        ));
        assert_eq!(effort_rank("xhigh"), effort_rank("max"));
        // The reported tier is below what was asked for.
        assert!(served_below_request(
            "gpt-6.1-sol",
            Some("xhigh"),
            Some("gpt-6.1-sol"),
            Some("low")
        ));
        assert!(served_below_request(
            "gpt-6.1-sol",
            Some("high"),
            Some("gpt-6.1-sol"),
            Some("medium")
        ));
        // A different model entirely, which is the case the upstream is
        // reported to serve quietly.
        assert!(served_below_request(
            "gpt-6.1-sol",
            Some("xhigh"),
            Some("gpt-5.5"),
            Some("max")
        ));
        // Above what was asked for is not a downgrade.
        assert!(!served_below_request(
            "gpt-6.1-sol",
            Some("low"),
            Some("gpt-6.1-sol"),
            Some("high")
        ));
        // Nothing reported yet, or a name this build does not know, must never
        // throw away a working answer.
        assert!(!served_below_request(
            "gpt-6.1-sol",
            Some("xhigh"),
            None,
            None
        ));
        assert!(!served_below_request(
            "gpt-6.1-sol",
            Some("xhigh"),
            Some("gpt-6.1-sol"),
            Some("experimental-tier")
        ));
        assert!(!served_below_request(
            "gpt-6.1-sol",
            None,
            Some("gpt-6.1-sol"),
            Some("low")
        ));
        // Case differences in the model name are not a downgrade.
        assert!(!served_below_request(
            "GPT-6.1-Sol",
            Some("high"),
            Some("gpt-6.1-sol"),
            Some("high")
        ));
    }

    #[test]
    fn only_failures_every_account_would_share_hold_back_the_whole_model() {
        // The relay could not reach its own upstream, so no key of ours helps.
        assert!(pool_wide_failure(
            Some(503),
            Some("bad response status code 503")
        ));
        assert!(pool_wide_failure(
            Some(502),
            Some("bad response status code 502")
        ));
        // A bare 5xx stays ambiguous: it can be one account's own transient
        // failure, so it keeps the per-account retry path and the documented
        // retry budget still applies to it.
        assert!(!pool_wide_failure(Some(502), None));
        assert!(!pool_wide_failure(Some(503), None));
        assert!(!pool_wide_failure(Some(504), None));
        // The model itself is at capacity, reported as a 500 with its own text.
        assert!(pool_wide_failure(
            Some(500),
            Some("当前模型 gpt-6.1-sol 负载已经达到上限，请稍后重试")
        ));
        assert!(pool_wide_failure(Some(500), Some("请求上游地址失败")));
        assert!(pool_wide_failure(
            Some(500),
            Some("The model is Overloaded")
        ));

        // One account's own problem must still move to the next account.
        assert!(!pool_wide_failure(Some(401), Some("invalid api key")));
        assert!(!pool_wide_failure(Some(403), Some("无权限")));
        assert!(!pool_wide_failure(Some(429), Some("rate limit")));
        assert!(!pool_wide_failure(Some(500), Some("internal error")));
        assert!(!pool_wide_failure(Some(500), None));
        assert!(!pool_wide_failure(None, Some("负载已经达到上限")));
    }
}
