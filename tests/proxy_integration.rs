//! Real localhost transport tests. Every credential is a fixed test fixture;
//! these tests never load BiWrite or send traffic to an external service.

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::post,
};
use futures_util::StreamExt;
use pool_router::{
    config::{Config, SourceMode},
    proxy::{self, AppState},
    scheduler::Selection,
    types::{Account, Strategy, now_ms},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle};

const ASTRA: &str = "gpt-6-astra";
const SOL: &str = "gpt-6.1-sol";
const TEST_KEY_A: &str = "sk-integration-fixture-alpha";
const TEST_KEY_B: &str = "sk-integration-fixture-beta";
const TEST_MANUAL_KEY: &str = "sk-integration-manual-fixture-gamma";

struct Seen {
    authorization: String,
    headers: HeaderMap,
    body: Bytes,
}

enum Reply {
    ForModel(&'static str, Box<Reply>),
    /// Holds back the response headers, which is what a hedge races against.
    SlowHeaders(Duration, Box<Reply>),
    Sse(Vec<(Option<Arc<Notify>>, String)>),
    Json(StatusCode, Value, Option<&'static str>),
    Raw(StatusCode, &'static str, &'static str),
    Heartbeats(Vec<String>),
    CancellableSse(Arc<Notify>, Arc<AtomicBool>),
}

struct DropSignal(Arc<AtomicBool>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
struct MockState {
    script: Arc<Mutex<VecDeque<Reply>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

async fn mock_response(State(state): State<MockState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 1024 * 1024).await.unwrap();
    state.seen.lock().unwrap().push(Seen {
        authorization: parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .into(),
        headers: parts.headers,
        body: body.clone(),
    });
    let requested_model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|body| body["model"].as_str().map(str::to_owned));
    let reply = {
        let mut script = state.script.lock().unwrap();
        let index = script
            .iter()
            .position(|reply| match reply {
                Reply::ForModel(model, _) => requested_model.as_deref() == Some(*model),
                _ => true,
            })
            .unwrap_or(0);
        script.remove(index).map(|reply| match reply {
            Reply::ForModel(_, reply) => *reply,
            reply => reply,
        })
    };
    let reply = match reply {
        Some(Reply::SlowHeaders(delay, inner)) => {
            tokio::time::sleep(delay).await;
            Some(*inner)
        }
        other => other,
    };
    match reply {
        Some(Reply::ForModel(_, _)) => unreachable!("model replies are unwrapped above"),
        Some(Reply::SlowHeaders(_, _)) => unreachable!("delays are applied above"),
        Some(Reply::Sse(chunks)) => {
            let stream = async_stream::stream! {
                for (gate, chunk) in chunks {
                    if let Some(gate) = gate { gate.notified().await; }
                    yield Ok::<_, std::io::Error>(Bytes::from(chunk));
                    tokio::task::yield_now().await;
                }
            };
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Some(Reply::Json(status, value, delay)) => {
            let mut response = Response::builder()
                .status(status)
                .header("content-type", "application/json");
            if let Some(delay) = delay {
                response = response.header("retry-after", delay);
            }
            response.body(Body::from(value.to_string())).unwrap()
        }
        Some(Reply::Raw(status, content_type, bytes)) => Response::builder()
            .status(status)
            .header("content-type", content_type)
            .body(Body::from(bytes))
            .unwrap(),
        Some(Reply::Heartbeats(prefix)) => {
            let stream = async_stream::stream! {
                for chunk in prefix {
                    yield Ok::<_, std::io::Error>(Bytes::from(chunk));
                    tokio::task::yield_now().await;
                }
                loop {
                    yield Ok(Bytes::from_static(b": fixture heartbeat\n\n"));
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            };
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Some(Reply::CancellableSse(gate, dropped)) => {
            let stream = async_stream::stream! {
                let _drop_signal = DropSignal(dropped);
                yield Ok::<_, std::io::Error>(Bytes::from_static(b": upstream awaiting completion\n\n"));
                gate.notified().await;
                yield Ok(Bytes::from(completed("resp_cancel_before_content", ASTRA)));
            };
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("test script exhausted"))
            .unwrap(),
    }
}

fn frame(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}

fn created(id: &str, model: &str) -> String {
    frame(
        json!({"type":"response.created","response":{"id":id,"model":model,"status":"in_progress"}}),
    )
}

fn delta(text: &str) -> String {
    frame(
        json!({"type":"response.output_text.delta","delta":text,"output_index":0,"content_index":0}),
    )
}

fn completed(id: &str, model: &str) -> String {
    frame(json!({"type":"response.completed","response":{
        "id":id,"model":model,"status":"completed","reasoning":{"effort":"high"},
        "usage":{"input_tokens":100,"output_tokens":12,"input_tokens_details":{"cached_tokens":80},"output_tokens_details":{"reasoning_tokens":4}}
    }}))
}

fn failed(id: &str, model: &str) -> String {
    frame(json!({"type":"response.failed","response":{
        "id":id,"model":model,"status":"failed",
        "error":{"code":"rate_limit_exceeded","message":"fixture upstream is busy"}
    }}))
}

fn success(id: &str, model: &str) -> Reply {
    Reply::Sse(vec![
        (None, created(id, model)),
        (None, delta("winner-only")),
        (None, completed(id, model)),
    ])
}

fn account(secret: &str, upstream: &str, models: &[&str]) -> Account {
    Account {
        id: blake3::hash(secret.as_bytes()).to_hex().to_string(),
        secret: Arc::from(secret),
        upstream: upstream.into(),
        label: "测试账号".into(),
        models: models.iter().map(|m| (*m).into()).collect(),
        provider_ids: vec!["fixture-provider".into()],
        source_max_concurrency: 1,
        model_concurrency: models.iter().map(|m| ((*m).into(), 1)).collect(),
        last_source_success_ms: None,
        model_source_success_ms: HashMap::new(),
    }
}

struct Harness {
    state: Arc<AppState>,
    base: String,
    upstream: String,
    client: reqwest::Client,
    mock: MockState,
    jobs: Vec<JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Drop for Harness {
    fn drop(&mut self) {
        for job in &self.jobs {
            job.abort();
        }
    }
}

impl Harness {
    async fn new(script: Vec<Reply>, accounts: usize, models: &[&str]) -> Self {
        let mock = MockState {
            script: Arc::new(Mutex::new(script.into())),
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}/v1", upstream_listener.local_addr().unwrap());
        let mock_router = Router::new()
            .route("/v1/responses", post(mock_response))
            .route("/v1/responses/compact", post(mock_response))
            .with_state(mock.clone());
        let upstream_job = tokio::spawn(async move {
            axum::serve(upstream_listener, mock_router).await.unwrap();
        });
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen = proxy_listener.local_addr().unwrap().to_string();
        let mut config = Config {
            listen: listen.clone(),
            queue_timeout_ms: 5000,
            upstream_header_timeout_ms: 5000,
            stream_idle_timeout_ms: 5000,
            max_attempts: 4,
            ..Config::default()
        };
        config.policy.strategy = Strategy::RoundRobin;
        let dir = tempfile::tempdir().unwrap();
        config.biwrite_dir = dir.path().join("biwrite-fixture");
        let state = AppState::new(config, dir.path().join("config.json"), false).unwrap();
        state.scheduler.sync_accounts(
            (0..accounts)
                .map(|index| match index {
                    0 => TEST_KEY_A.to_owned(),
                    1 => TEST_KEY_B.to_owned(),
                    _ => format!("sk-integration-fixture-{index}"),
                })
                .map(|secret| account(&secret, &upstream, models))
                .collect(),
        );
        let proxy_router = proxy::router(state.clone());
        let proxy_job = tokio::spawn(async move {
            axum::serve(proxy_listener, proxy_router).await.unwrap();
        });
        Self {
            state,
            base: format!("http://{listen}"),
            upstream,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            mock,
            jobs: vec![upstream_job, proxy_job],
            _dir: dir,
        }
    }

    fn post(&self, body: Value) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}/v1/responses", self.base))
            .bearer_auth(&self.state.settings().local_token)
            .json(&body)
    }

    fn request(model: &str) -> Value {
        json!({"model":model,"stream":true,"store":false,"input":[{"role":"user","content":"fixture input"}],"reasoning":{"effort":"high"}})
    }

    fn calls(&self) -> usize {
        self.mock.seen.lock().unwrap().len()
    }

    fn api_post(&self, path: &str, value: Value) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.state.settings().local_token)
            .json(&value)
    }

    fn api_get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.state.settings().local_token)
    }

    fn write_biwrite_fixture(&self, models: &[&str], prefix: &str) {
        let dir = self.state.settings().biwrite_dir;
        fs::create_dir_all(&dir).unwrap();
        let fingerprint = blake3::hash(TEST_KEY_A.as_bytes()).to_hex().to_string()[..12].to_owned();
        let mut providers = Vec::new();
        let mut keys = serde_json::Map::new();
        let mut kept = serde_json::Map::new();
        for (index, model) in models.iter().enumerate() {
            let id = if models.len() == 1 {
                prefix.to_owned()
            } else {
                format!("{prefix}-{index}")
            };
            providers.push(json!({"id":id,"name":"自动测试","kind":"openai_compatible",
                "wireApi":"responses","baseUrl":"https://api.example.com/v1","model":model,
                "hasKey":true,"keyCount":1,"keyConcurrency":1}));
            keys.insert(id.clone(), json!(TEST_KEY_A));
            kept.insert(id, json!([fingerprint]));
        }
        fs::write(
            dir.join("settings.json"),
            json!({"keysInFile":true,"providers":providers,"keptKeys":kept}).to_string(),
        )
        .unwrap();
        fs::write(dir.join("keys.json"), json!({"keys":keys}).to_string()).unwrap();
        *self.state.source_upstream_override.write().unwrap() = Some(self.upstream.clone());
    }

    async fn install_biwrite_fixture(&self) {
        self.write_biwrite_fixture(&[ASTRA], "p-auto-fixture");
        self.state.sync_source().await.unwrap();
    }

    async fn install_biwrite_models_fixture(&self, models: &[&str]) {
        self.write_biwrite_fixture(models, "p-validation");
        self.state.sync_source().await.unwrap();
    }

    fn in_flight(&self) -> u32 {
        self.state
            .scheduler
            .snapshot(&self.state.settings().policy)
            .iter()
            .map(|a| a.in_flight)
            .sum()
    }

    async fn direct(&self, model: &str) -> Response {
        proxy::run_proxy(
            self.state.clone(),
            Uri::from_static("/v1/responses"),
            HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            )]),
            Bytes::from(Self::request(model).to_string()),
        )
        .await
    }
}

#[tokio::test]
async fn prelude_rate_limit_retries_another_key_and_only_winning_sse_reaches_client() {
    let h = Harness::new(
        vec![
            Reply::Sse(vec![
                (None, created("resp_rejected", ASTRA)),
                (None, failed("resp_rejected", ASTRA)),
            ]),
            success("resp_winner", ASTRA),
        ],
        2,
        &[ASTRA],
    )
    .await;
    let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    assert!(text.contains("resp_winner"));
    assert!(!text.contains("resp_rejected"));
    assert!(!text.contains("rate_limit_exceeded"));
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].authorization, seen[1].authorization);
    drop(seen);
    let records = h.state.records.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].attempts.len(), 2);
    assert_eq!(records[0].outcome, "completed");
    assert_eq!(records[0].input_tokens, Some(100));
    assert_eq!(records[0].cached_tokens, Some(80));
}

#[tokio::test]
async fn failure_after_text_delta_never_replays_request_on_another_key() {
    let h = Harness::new(
        vec![Reply::Sse(vec![
            (None, created("resp_partial", ASTRA)),
            (None, delta("already-visible")),
            (None, failed("resp_partial", ASTRA)),
        ])],
        2,
        &[ASTRA],
    )
    .await;
    let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
    let text = response.text().await.unwrap();
    assert!(text.contains("already-visible"));
    assert!(text.contains("response.failed"));
    assert_eq!(h.calls(), 1);
    assert_eq!(h.state.records.records()[0].outcome, "upstream_failed");
}

#[tokio::test]
async fn rate_limited_astra_does_not_cool_down_sol_for_the_same_key() {
    let h = Harness::new(
        vec![
            Reply::Sse(vec![(None, failed("resp_busy", ASTRA))]),
            success("resp_sol", SOL),
        ],
        1,
        &[ASTRA, SOL],
    )
    .await;
    h.state.config.write().unwrap().max_attempts = 1;
    let busy = h.post(Harness::request(ASTRA)).send().await.unwrap();
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    let sol = h.post(Harness::request(SOL)).send().await.unwrap();
    assert_eq!(sol.status(), StatusCode::OK);
    assert!(sol.text().await.unwrap().contains("resp_sol"));
    let snapshot = h.state.scheduler.snapshot(&h.state.settings().policy);
    let astra = snapshot[0]
        .model_health
        .iter()
        .find(|m| m.model == ASTRA)
        .unwrap();
    let sol = snapshot[0]
        .model_health
        .iter()
        .find(|m| m.model == SOL)
        .unwrap();
    assert!(astra.cooldown_until_ms.is_some());
    assert_eq!(sol.cooldown_until_ms, None);
    assert_eq!(sol.successes, 1);
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen[0].authorization, seen[1].authorization);
}

#[tokio::test]
async fn sse_lease_remains_held_until_response_body_finishes() {
    let gate = Arc::new(Notify::new());
    let h = Harness::new(
        vec![Reply::Sse(vec![
            (None, delta("first chunk")),
            (Some(gate.clone()), completed("resp_held", ASTRA)),
        ])],
        1,
        &[ASTRA],
    )
    .await;
    let response = h.direct(ASTRA).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(h.in_flight(), 1);
    let selection = Selection {
        model: ASTRA.into(),
        ..Selection::default()
    };
    assert!(
        h.state
            .scheduler
            .try_acquire(&selection, &h.state.settings().policy)
            .is_err()
    );
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().unwrap().starts_with(b"event:"));
    assert_eq!(h.in_flight(), 1);
    gate.notify_one();
    while let Some(chunk) = stream.next().await {
        chunk.unwrap();
    }
    assert_eq!(h.in_flight(), 0);
    assert_eq!(h.state.records.records()[0].outcome, "completed");
}

#[tokio::test]
async fn cancelling_downstream_body_releases_sse_lease() {
    let gate = Arc::new(Notify::new());
    let h = Harness::new(
        vec![Reply::Sse(vec![
            (None, delta("first chunk")),
            (Some(gate), completed("resp_cancelled", ASTRA)),
        ])],
        1,
        &[ASTRA],
    )
    .await;
    let response = h.direct(ASTRA).await;
    let mut stream = response.into_body().into_data_stream();
    stream.next().await.unwrap().unwrap();
    assert_eq!(h.in_flight(), 1);
    drop(stream);
    assert_eq!(h.in_flight(), 0);
    assert_eq!(h.state.records.records()[0].outcome, "client_cancelled");
    let selection = Selection {
        model: ASTRA.into(),
        ..Selection::default()
    };
    assert!(
        h.state
            .scheduler
            .try_acquire(&selection, &h.state.settings().policy)
            .is_ok()
    );
}

#[tokio::test]
async fn http_429_retries_another_key_and_preserves_retry_after_cooldown() {
    let h = Harness::new(
        vec![
            Reply::Json(
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error":{"code":"rate_limit_exceeded"}}),
                Some("30"),
            ),
            success("resp_after_429", ASTRA),
        ],
        2,
        &[ASTRA],
    )
    .await;
    let before = now_ms();
    let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
    assert!(response.text().await.unwrap().contains("resp_after_429"));
    assert_eq!(h.calls(), 2);
    let snapshots = h.state.scheduler.snapshot(&h.state.settings().policy);
    let cooling = snapshots
        .iter()
        .flat_map(|a| &a.model_health)
        .find(|m| m.failures == 1)
        .unwrap();
    assert!(cooling.cooldown_until_ms.unwrap() >= before + 29_000);
    assert_eq!(h.state.records.records()[0].attempts[0].status, Some(429));
}

#[tokio::test]
async fn unknown_previous_response_id_is_rejected_before_any_upstream_request() {
    let h = Harness::new(vec![], 1, &[ASTRA]).await;
    let mut body = Harness::request(ASTRA);
    body["previous_response_id"] = json!("resp_unknown");
    let response = h.post(body).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "unknown_response"
    );
    assert_eq!(h.calls(), 0);
}

#[tokio::test]
async fn known_previous_response_id_stays_on_original_key_and_model() {
    let h = Harness::new(
        vec![success("resp_parent", ASTRA), success("resp_child", ASTRA)],
        2,
        &[ASTRA, SOL],
    )
    .await;
    h.post(Harness::request(ASTRA))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let mut followup = Harness::request(ASTRA);
    followup["previous_response_id"] = json!("resp_parent");
    let second = h.post(followup).send().await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    assert!(second.text().await.unwrap().contains("resp_child"));
    let mut wrong_model = Harness::request(SOL);
    wrong_model["previous_response_id"] = json!("resp_parent");
    let denied = h.post(wrong_model).send().await.unwrap();
    assert!(!denied.status().is_success());
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].authorization, seen[1].authorization);
    assert_eq!(
        serde_json::from_slice::<Value>(&seen[1].body).unwrap()["model"],
        ASTRA
    );
}

#[tokio::test]
async fn compressed_request_preserves_exact_wire_bytes_and_observation_metadata() {
    let h = Harness::new(vec![success("resp_compressed", ASTRA)], 1, &[ASTRA]).await;
    let original = Harness::request(ASTRA).to_string();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(original.as_bytes()).unwrap();
    let compressed = encoder.finish().unwrap();
    let response = h
        .client
        .post(format!("{}/v1/responses", h.base))
        .bearer_auth(&h.state.settings().local_token)
        .header("content-type", "application/json")
        .header("content-encoding", "gzip")
        .header("x-fixture-header", "preserved")
        .body(compressed.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen[0].body.as_ref(), compressed);
    assert_eq!(seen[0].headers["content-encoding"], "gzip");
    assert_eq!(seen[0].headers["x-fixture-header"], "preserved");
    assert_ne!(
        seen[0].authorization,
        format!("Bearer {}", h.state.settings().local_token)
    );
    drop(seen);
    let records = h.state.records.records();
    assert_eq!(records[0].request_bytes, compressed.len() as u64);
    assert_eq!(records[0].requested_model.as_deref(), Some(ASTRA));
    assert_eq!(records[0].requested_effort.as_deref(), Some("high"));
    assert_eq!(records[0].output_tokens, Some(12));
}

#[tokio::test]
async fn host_origin_and_authorization_guards_reject_before_upstream() {
    let h = Harness::new(vec![], 1, &[ASTRA]).await;
    let invalid_host = h
        .post(Harness::request(ASTRA))
        .header("host", "evil.example:10813")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_host.status(), StatusCode::FORBIDDEN);
    let invalid_origin = h
        .post(Harness::request(ASTRA))
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_origin.status(), StatusCode::FORBIDDEN);
    let unauthenticated = h
        .client
        .post(format!("{}/v1/responses", h.base))
        .json(&Harness::request(ASTRA))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    let wrong_token = h
        .client
        .get(format!("{}/api/records", h.base))
        .bearer_auth("incorrect-fixture-token")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_token.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(h.calls(), 0);
}

#[tokio::test]
async fn static_panel_is_accessible_without_embedding_local_or_upstream_secrets() {
    let h = Harness::new(vec![], 1, &[ASTRA]).await;
    for path in ["/", "/app.js", "/style.css", "/health"] {
        let response = h
            .client
            .get(format!("{}{path}", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        let text = response.text().await.unwrap();
        assert!(!text.contains(&h.state.settings().local_token));
        assert!(!text.contains(TEST_KEY_A));
        assert!(!text.contains(TEST_KEY_B));
    }
    assert_eq!(h.calls(), 0);
}

#[tokio::test]
async fn json_rate_limit_with_http_200_still_applies_429_cooldown() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let h = Harness::new(
            vec![Reply::Json(
                StatusCode::OK,
                json!({"error":{"code":"rate_limit_exceeded"}}),
                None,
            )],
            1,
            &[ASTRA],
        )
        .await;
        h.state.config.write().unwrap().max_attempts = 1;
        let before = now_ms();
        let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
        assert!(!response.status().is_success());
        response.bytes().await.unwrap();
        let snapshot = h.state.scheduler.snapshot(&h.state.settings().policy);
        let health = &snapshot[0].model_health[0];
        assert_eq!(health.failures, 1);
        assert_eq!(health.successes, 0);
        assert_eq!(health.last_error.as_deref(), Some("rate_limit_exceeded"));
        assert!(
            health
                .cooldown_until_ms
                .is_some_and(|until| until >= before + 1900)
        );
        assert_eq!(h.calls(), 1);
    })
    .await
    .expect("HTTP 200 JSON 限流处理不得挂起");
}

#[tokio::test]
async fn http_200_invalid_or_unfinished_responses_return_502_without_success() {
    tokio::time::timeout(Duration::from_secs(3), async {
        for reply in [
            Reply::Raw(StatusCode::OK, "text/html", "<html>upstream maintenance</html>"),
            Reply::Json(StatusCode::OK, json!({}), None),
            Reply::Json(
                StatusCode::OK,
                json!({"id":"resp_unfinished","object":"response","model":ASTRA,"status":"in_progress","output":[]}),
                None,
            ),
        ] {
            let h = Harness::new(vec![reply], 1, &[ASTRA]).await;
            let mut body = Harness::request(ASTRA);
            body["stream"] = json!(false);
            let response = h.post(body).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            response.bytes().await.unwrap();
            let snapshot = h.state.scheduler.snapshot(&h.state.settings().policy);
            assert_eq!(snapshot[0].model_health[0].successes, 0);
            assert_eq!(h.calls(), 1);
            let records = h.state.records.records();
            assert_eq!(records.len(), 1);
            assert_ne!(records[0].outcome, "completed");
        }
    })
    .await
    .expect("无效 HTTP 200 响应不得被当作成功或持续重试");
}

#[tokio::test]
async fn completed_sse_ends_and_releases_lease_even_when_upstream_heartbeats_continue() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let h = Harness::new(
            vec![Reply::Heartbeats(vec![
                created("resp_terminal", ASTRA),
                delta("terminal result"),
                completed("resp_terminal", ASTRA),
            ])],
            1,
            &[ASTRA],
        )
        .await;
        let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.bytes().await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("response.completed"));
        assert_eq!(h.in_flight(), 0);
        let records = h.state.records.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, "completed");
        assert_eq!(records[0].output_tokens, Some(12));
    })
    .await
    .expect("SSE 完成帧后必须结束，不应等待上游 EOF");
}

#[tokio::test]
async fn heartbeat_only_prelude_is_bounded_by_queue_deadline_not_idle_timeout() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let h = Harness::new(vec![Reply::Heartbeats(Vec::new())], 1, &[ASTRA]).await;
        {
            let mut config = h.state.config.write().unwrap();
            config.queue_timeout_ms = 1000;
            config.stream_idle_timeout_ms = 10_000;
        }
        let started = std::time::Instant::now();
        let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
        assert!(!response.status().is_success());
        response.bytes().await.unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(900));
        assert!(elapsed < Duration::from_secs(2));
        assert_eq!(h.calls(), 1);
        assert_eq!(h.in_flight(), 0);
        assert_ne!(h.state.records.records()[0].outcome, "completed");
    })
    .await
    .expect("只有心跳的前导流必须在请求总等待期限内退出");
}

#[tokio::test]
async fn paused_recording_keeps_forwarding_but_does_not_append_records() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let h = Harness::new(
            vec![
                success("resp_recorded", ASTRA),
                success("resp_unrecorded", ASTRA),
            ],
            1,
            &[ASTRA],
        )
        .await;
        h.post(Harness::request(ASTRA))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let before = h.state.records.records();
        assert_eq!(before.len(), 1);
        h.state.config.write().unwrap().recording_enabled = false;
        let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.text().await.unwrap().contains("resp_unrecorded"));
        assert_eq!(h.calls(), 2);
        let after = h.state.records.records();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, before[0].id);
        assert_eq!(h.in_flight(), 0);
        let snapshot = h.state.scheduler.snapshot(&h.state.settings().policy);
        assert_eq!(snapshot[0].model_health[0].successes, 2);
    })
    .await
    .expect("暂停记录不得暂停请求转发");
}

#[tokio::test]
async fn accept_sse_sends_immediate_comment_before_delayed_upstream_and_preserves_result() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let gate = Arc::new(Notify::new());
        let h = Harness::new(
            vec![Reply::Sse(vec![
                (Some(gate.clone()), created("resp_delayed", ASTRA)),
                (None, delta("delayed result")),
                (None, completed("resp_delayed", ASTRA)),
            ])],
            1,
            &[ASTRA],
        )
        .await;
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            gate.notify_one();
        });
        let started = std::time::Instant::now();
        let response = h
            .post(Harness::request(ASTRA))
            .header("accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .contains("text/event-stream")
        );
        let mut stream = response.bytes_stream();
        let first = stream.next().await.unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_millis(750));
        assert!(first.starts_with(b": pool-router waiting\n\n"));
        assert!(!String::from_utf8_lossy(&first).contains("resp_delayed"));
        let mut result = first.to_vec();
        while let Some(chunk) = stream.next().await {
            result.extend_from_slice(&chunk.unwrap());
        }
        release.await.unwrap();
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("delayed result"));
        assert!(text.contains("resp_delayed"));
        assert!(text.contains("response.completed"));
        assert_eq!(h.calls(), 1);
        assert_eq!(h.in_flight(), 0);
        assert_eq!(h.state.records.records()[0].outcome, "completed");
    })
    .await
    .expect("Accept SSE 请求应先立即发送等待注释，再完成上游响应");
}

#[tokio::test]
async fn accept_sse_waiting_comment_does_not_expose_rejected_prelude_during_key_failover() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let h = Harness::new(
            vec![
                Reply::Sse(vec![
                    (None, created("resp_hidden_rejection", ASTRA)),
                    (None, failed("resp_hidden_rejection", ASTRA)),
                ]),
                success("resp_visible_winner", ASTRA),
            ],
            2,
            &[ASTRA],
        )
        .await;
        let response = h
            .post(Harness::request(ASTRA))
            .header("accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let text = response.text().await.unwrap();
        assert!(text.starts_with(": pool-router waiting\n\n"));
        assert!(text.contains("resp_visible_winner"));
        assert!(text.contains("response.completed"));
        assert!(!text.contains("resp_hidden_rejection"));
        assert!(!text.contains("response.failed"));
        assert!(!text.contains("rate_limit_exceeded"));
        let seen = h.mock.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_ne!(seen[0].authorization, seen[1].authorization);
        drop(seen);
        let records = h.state.records.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].attempts.len(), 2);
        assert_eq!(records[0].outcome, "completed");
    })
    .await
    .expect("外层等待注释不得阻止前导失败自动换号");
}

#[tokio::test]
async fn cancelling_after_waiting_comment_cancels_pending_upstream_and_releases_lease() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let dropped = Arc::new(AtomicBool::new(false));
        let h = Harness::new(
            vec![Reply::CancellableSse(
                Arc::new(Notify::new()),
                dropped.clone(),
            )],
            1,
            &[ASTRA],
        )
        .await;
        {
            // Releasing within one second must result from cancellation,
            // not the ordinary queue or upstream idle deadline.
            let mut config = h.state.config.write().unwrap();
            config.queue_timeout_ms = 10_000;
            config.upstream_header_timeout_ms = 10_000;
            config.stream_idle_timeout_ms = 10_000;
        }
        let response = h
            .post(Harness::request(ASTRA))
            .header("accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        let mut stream = response.bytes_stream();
        let first = stream.next().await.unwrap().unwrap();
        assert!(first.starts_with(b": pool-router waiting\n\n"));
        tokio::time::timeout(Duration::from_secs(1), async {
            while h.calls() != 1 || h.in_flight() != 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("测试必须先确认上游已收到请求并持有并发槽");
        assert!(!dropped.load(Ordering::Acquire));
        drop(stream);
        tokio::time::timeout(Duration::from_secs(1), async {
            while h.in_flight() != 0 || !dropped.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("客户端取消后应及时取消上游并释放并发槽");
        assert_eq!(h.calls(), 1);
        let records = h.state.records.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, "client_cancelled");
    })
    .await
    .expect("等待注释之后的客户端断开不得留下后台上游请求");
}

#[tokio::test]
async fn source_defaults_to_biwrite_and_empty_manual_selection_preserves_auto_pool() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let h = Harness::new(vec![], 1, &[ASTRA]).await;
        assert!(Config::default().source_mode == SourceMode::Biwrite);
        let old_config: Config = serde_json::from_value(json!({})).unwrap();
        assert!(old_config.source_mode == SourceMode::Biwrite);
        let before = h.state.scheduler.snapshot(&h.state.settings().policy)[0]
            .id
            .clone();
        let response = h
            .api_post("/api/config", json!({"source_mode":"manual"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = response.json::<Value>().await.unwrap();
        assert_eq!(response["error"]["code"], "manual_pool_empty");
        assert!(h.state.settings().source_mode == SourceMode::Biwrite);
        let accounts = h.state.scheduler.snapshot(&h.state.settings().policy);
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, before);
        assert!(!h.state.config_path.exists());
        assert_eq!(h.calls(), 0);
    })
    .await
    .expect("空手动号池切换必须被拒绝且保留自动来源");
}

#[tokio::test]
async fn manual_keys_without_a_selected_upstream_cannot_replace_existing_config_or_pool() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let h = Harness::new(vec![], 1, &[ASTRA]).await;
        let before = h.state.settings();
        assert!(before.upstream_base_url.is_none());
        assert!(!before.biwrite_dir.exists());
        pool_router::config::save(&h.state.config_path, &before).unwrap();
        let saved_before = fs::read(&h.state.config_path).unwrap();
        let account_before = h.state.scheduler.snapshot(&before.policy)[0].id.clone();
        let add = h
            .api_post(
                "/api/manual",
                json!({"key":TEST_MANUAL_KEY,"models":[SOL],"label":"手动测试"}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(add.status(), StatusCode::OK);

        let response = h
            .api_post(
                "/api/config",
                json!({"source_mode":"manual","policy":{"strategy":"random"}}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = response.json::<Value>().await.unwrap();
        assert_eq!(response["error"]["code"], "source_unavailable");
        assert!(!response.to_string().contains(TEST_MANUAL_KEY));
        assert_eq!(
            serde_json::to_value(h.state.settings()).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        assert_eq!(fs::read(&h.state.config_path).unwrap(), saved_before);
        let accounts = h.state.scheduler.snapshot(&h.state.settings().policy);
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, account_before);
        assert_eq!(accounts[0].models, [ASTRA]);
        assert!(!h.state.settings().biwrite_dir.exists());
        assert_eq!(h.calls(), 0);
    })
    .await
    .expect("已有手动密钥但缺少上游时必须拒绝切换并保留配置与当前号池");
}

#[tokio::test]
async fn adding_manual_key_keeps_auto_pool_and_pinned_auto_routing_unchanged() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let h = Harness::new(vec![success("resp_pinned_auto", ASTRA)], 2, &[ASTRA]).await;
        *h.state.connection_override.write().unwrap() =
            Some(pool_router::integration::ConnectionInfo {
                base_url: format!("{}/v1", h.base),
                configured: true,
                detail: "isolated test".into(),
            });
        let auto_id = blake3::hash(TEST_KEY_A.as_bytes()).to_hex().to_string();
        let add = h
            .api_post(
                "/api/manual",
                json!({"key":TEST_MANUAL_KEY,"models":[SOL],"label":"手动测试"}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(add.status(), StatusCode::OK);
        let response = add.json::<Value>().await.unwrap();
        assert_eq!(
            response["id"],
            blake3::hash(TEST_MANUAL_KEY.as_bytes())
                .to_hex()
                .to_string()
        );
        assert!(!response.to_string().contains(TEST_MANUAL_KEY));
        assert!(h.state.settings().source_mode == SourceMode::Biwrite);
        let accounts = h.state.scheduler.snapshot(&h.state.settings().policy);
        assert_eq!(accounts.len(), 2);
        assert!(accounts.iter().all(|account| account.models == [ASTRA]));
        let pin = h
            .api_post(
                "/api/config",
                json!({"policy":{"strategy":"pinned","pinned_key":auto_id}}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(pin.status(), StatusCode::OK);
        let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
        assert!(response.text().await.unwrap().contains("resp_pinned_auto"));
        assert_eq!(
            h.mock.seen.lock().unwrap()[0].authorization,
            format!("Bearer {TEST_KEY_A}")
        );
        let manual = h
            .api_get("/api/manual")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(manual["accounts"].as_array().unwrap().len(), 1);
        assert_eq!(manual["accounts"][0]["models"], json!([SOL]));
        assert!(!manual.to_string().contains(TEST_MANUAL_KEY));
        let public_state = h
            .api_get("/api/state")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(public_state["config"]["source_mode"], "biwrite");
        assert_eq!(public_state["config"]["policy"]["strategy"], "pinned");
        assert_eq!(public_state["accounts"].as_array().unwrap().len(), 2);
        assert_eq!(public_state["connection"]["detail"], "isolated test");
        let public_text = public_state.to_string();
        for secret in [TEST_KEY_A, TEST_KEY_B, TEST_MANUAL_KEY] {
            assert!(!public_text.contains(secret));
        }
        assert!(!public_text.contains(&h.state.settings().local_token));
        assert_eq!(h.calls(), 1);
    })
    .await
    .expect("维护手动账号不得影响当前自动池和指定账号调度");
}

#[tokio::test]
async fn manual_source_is_independent_of_biwrite_and_deletion_removes_candidate_immediately() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let h = Harness::new(vec![], 0, &[ASTRA]).await;
        *h.state.connection_override.write().unwrap() = Some(pool_router::integration::ConnectionInfo {
            base_url: format!("{}/v1", h.base), configured: true, detail: "isolated test".into(),
        });
        h.install_biwrite_fixture().await;
        let biwrite_dir = h.state.settings().biwrite_dir;
        let parked = h._dir.path().join("biwrite-parked");
        let original_settings = fs::read(biwrite_dir.join("settings.json")).unwrap();
        let original_keys = fs::read(biwrite_dir.join("keys.json")).unwrap();
        let add = h.api_post("/api/manual", json!({"key":TEST_MANUAL_KEY,"models":[SOL],"label":"手动测试","max_concurrency":2})).send().await.unwrap();
        assert_eq!(add.status(), StatusCode::OK);
        let id = add.json::<Value>().await.unwrap()["id"].as_str().unwrap().to_owned();
        fs::rename(&biwrite_dir, &parked).unwrap();
        let switched = h.api_post("/api/config", json!({"source_mode":"manual","policy":{"strategy":"random"}})).send().await.unwrap();
        assert_eq!(switched.status(), StatusCode::OK);
        assert!(h.state.settings().source_mode == SourceMode::Manual);
        assert_eq!(h.state.settings().upstream_base_url.as_deref(), Some("https://api.example.com/v1"));
        assert_eq!(h.state.settings().policy.strategy, Strategy::Random);
        h.state.sync_source().await.unwrap();
        let accounts = h.state.scheduler.snapshot(&h.state.settings().policy);
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, id);
        assert_eq!(accounts[0].models, [SOL]);
        let public_state = h.api_get("/api/state").send().await.unwrap().json::<Value>().await.unwrap();
        assert_eq!(public_state["config"]["source_mode"], "manual");
        assert_eq!(public_state["config"]["upstream_base_url"], "https://api.example.com/v1");
        assert_eq!(public_state["config"]["policy"]["strategy"], "random");
        assert_eq!(public_state["accounts"].as_array().unwrap().len(), 1);
        assert_eq!(public_state["accounts"][0]["id"], id);
        assert_eq!(public_state["accounts"][0]["models"], json!([SOL]));
        assert_eq!(public_state["connection"]["detail"], "isolated test");
        let public_text = public_state.to_string();
        assert!(!public_text.contains(TEST_MANUAL_KEY));
        assert!(!public_text.contains(TEST_KEY_A));
        assert!(!public_text.contains(&h.state.settings().local_token));
        // Selection proves manual source supports automatic scheduling.
        let selection = Selection { model: SOL.into(), ..Selection::default() };
        let lease = h.state.scheduler.try_acquire(&selection, &h.state.settings().policy).unwrap();
        assert_eq!(lease.account.id, id);
        assert_eq!(lease.account.upstream, "https://api.example.com/v1");
        drop(lease);
        // A fresh AppState has no cached automatic-source selection. Its manual
        // sync must use the persisted endpoint while BiWrite remains absent.
        let saved = pool_router::config::load(&h.state.config_path).unwrap();
        assert!(saved.source_mode == SourceMode::Manual);
        assert_eq!(saved.upstream_base_url.as_deref(), Some("https://api.example.com/v1"));
        let restarted = AppState::new(saved, h.state.config_path.clone(), false).unwrap();
        restarted.sync_source().await.unwrap();
        let restarted_accounts = restarted.scheduler.snapshot(&restarted.settings().policy);
        assert_eq!(restarted_accounts.len(), 1);
        assert_eq!(restarted_accounts[0].id, id);
        assert_eq!(restarted_accounts[0].models, [SOL]);
        assert!(restarted.source.read().unwrap().last_error.is_none());
        let lease = restarted.scheduler.try_acquire(&selection, &restarted.settings().policy).unwrap();
        assert_eq!(lease.account.upstream, "https://api.example.com/v1");
        drop(lease);
        drop(restarted);
        assert!(!biwrite_dir.exists());
        assert_eq!(fs::read(parked.join("settings.json")).unwrap(), original_settings);
        assert_eq!(fs::read(parked.join("keys.json")).unwrap(), original_keys);
        let removed = h.client.delete(format!("{}/api/manual/{id}", h.base))
            .bearer_auth(&h.state.settings().local_token).send().await.unwrap();
        assert_eq!(removed.status(), StatusCode::OK);
        assert!(h.state.scheduler.snapshot(&h.state.settings().policy).is_empty());
        assert!(h.state.scheduler.try_acquire(&selection, &h.state.settings().policy).is_err());
        assert_eq!(h.api_get("/api/manual").send().await.unwrap().json::<Value>().await.unwrap(), json!({"accounts":[]}));
        let public_state = h.api_get("/api/state").send().await.unwrap().json::<Value>().await.unwrap();
        assert_eq!(public_state["accounts"], json!([]));
        fs::rename(&parked, &biwrite_dir).unwrap();
        let back = h.api_post("/api/config", json!({"source_mode":"biwrite"})).send().await.unwrap();
        assert_eq!(back.status(), StatusCode::OK);
        assert!(h.state.settings().source_mode == SourceMode::Biwrite);
        assert_eq!(h.state.settings().policy.strategy, Strategy::Random);
        let accounts = h.state.scheduler.snapshot(&h.state.settings().policy);
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, blake3::hash(TEST_KEY_A.as_bytes()).to_hex().to_string());
        assert_eq!(accounts[0].models, [ASTRA]);
        assert_eq!(fs::read(biwrite_dir.join("settings.json")).unwrap(), original_settings);
        assert_eq!(fs::read(biwrite_dir.join("keys.json")).unwrap(), original_keys);
        assert_eq!(h.calls(), 0);
    }).await.expect("手动来源应独立于BiWrite并支持删除及切回自动来源");
}

fn recovery_error(status: StatusCode, code: &str) -> Reply {
    Reply::Json(
        status,
        json!({"error":{"code":code,"message":"fixture rejection"}}),
        None,
    )
}

fn compaction_request() -> Value {
    json!({"model":ASTRA,"input":[
        {"role":"user","content":"retain this user message"},
        {"type":"reasoning","id":"rs_opaque","encrypted_content":"fixture-opaque"},
        {"type":"reasoning","id":"rs_plain","encrypted_content":"fixture-reasoning",
            "summary":[{"type":"summary_text","text":"retain plaintext reasoning"}]},
        {"type":"function_call","call_id":"call_fixture","name":"lookup","arguments":"{}"},
        {"type":"function_call_output","call_id":"call_fixture","output":"retain tool result"},
        {"type":"compaction","encrypted_content":"fixture-compaction"}
    ]})
}

fn compact_success() -> Reply {
    Reply::Json(
        StatusCode::OK,
        json!({"id":"cmp_fixture","object":"response.compaction",
        "output":[{"type":"compaction","encrypted_content":"fixture-new-compaction"}]}),
        None,
    )
}

fn session_request() -> Value {
    let mut body = Harness::request(ASTRA);
    body["prompt_cache_key"] = json!("fixture-session");
    body["client_metadata"] = json!({"session_id":"fixture-session","thread_id":"fixture-session"});
    body
}

fn use_short_cooldown(h: &Harness) {
    let mut config = h.state.config.write().unwrap();
    config.policy.cooldown_base_ms = 100;
    config.policy.cooldown_max_ms = 100;
}

#[tokio::test]
async fn default_retry_budget_allows_sixteen_attempts_and_never_a_seventeenth() {
    assert_eq!(Config::default().max_attempts, 16);
    assert_eq!(
        serde_json::from_value::<Config>(json!({}))
            .unwrap()
            .max_attempts,
        16
    );
    for rejected in [15, 16] {
        let mut script: Vec<_> = (0..rejected)
            .map(|_| recovery_error(StatusCode::SERVICE_UNAVAILABLE, "server_error"))
            .collect();
        script.push(success("resp_final_allowed_attempt", ASTRA));
        let h = Harness::new(script, 17, &[ASTRA]).await;
        h.state.config.write().unwrap().max_attempts = Config::default().max_attempts;
        let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert_eq!(h.calls(), 16);
        assert_eq!(h.state.records.records()[0].attempts.len(), 16);
        assert_eq!(h.in_flight(), 0);
        if rejected == 15 {
            assert_eq!(status, StatusCode::OK);
            assert!(text.contains("resp_final_allowed_attempt"));
        } else {
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(!text.contains("resp_final_allowed_attempt"));
            assert_eq!(h.mock.script.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn first_output_deadline_retries_heartbeat_only_attempt_on_another_key() {
    assert_eq!(Config::default().first_output_timeout_ms, 120_000);
    let h = Harness::new(
        vec![
            Reply::Heartbeats(vec![created("resp_stalled_prelude", ASTRA)]),
            success("resp_after_first_output_timeout", ASTRA),
        ],
        2,
        &[ASTRA],
    )
    .await;
    h.state.config.write().unwrap().first_output_timeout_ms = 1000;
    let started = std::time::Instant::now();
    let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    assert!(text.contains("resp_after_first_output_timeout"));
    assert!(!text.contains("resp_stalled_prelude"));
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert!(started.elapsed() < Duration::from_secs(4));
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].authorization, seen[1].authorization);
    drop(seen);
    let records = h.state.records.records();
    assert_eq!(records[0].attempts[0].reason, "first_output_timeout");
    assert_eq!(records[0].outcome, "completed");
    assert_eq!(h.in_flight(), 0);
}

#[tokio::test]
async fn first_output_deadline_does_not_replay_after_visible_output() {
    let gate = Arc::new(Notify::new());
    let h = Harness::new(
        vec![
            Reply::Sse(vec![
                (None, created("resp_started", ASTRA)),
                (None, delta("already-visible")),
                (Some(gate.clone()), completed("resp_started", ASTRA)),
            ]),
            success("resp_forbidden_replay", ASTRA),
        ],
        2,
        &[ASTRA],
    )
    .await;
    h.state.config.write().unwrap().first_output_timeout_ms = 1000;
    let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.bytes_stream();
    let mut text = String::new();
    while !text.contains("already-visible") {
        text.push_str(&String::from_utf8_lossy(
            &stream.next().await.unwrap().unwrap(),
        ));
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(h.calls(), 1);
    gate.notify_one();
    while let Some(part) = stream.next().await {
        text.push_str(&String::from_utf8_lossy(&part.unwrap()));
    }
    assert!(text.contains("response.completed"));
    assert!(!text.contains("resp_forbidden_replay"));
    assert_eq!(h.calls(), 1);
    assert_eq!(h.state.records.records()[0].outcome, "completed");
}

#[tokio::test]
async fn compaction_encrypted_content_400_repairs_once_and_preserves_plaintext_and_tools() {
    let h = Harness::new(
        vec![
            recovery_error(StatusCode::BAD_REQUEST, "invalid_encrypted_content"),
            compact_success(),
        ],
        2,
        &[ASTRA],
    )
    .await;
    let original = compaction_request();
    let response = h
        .api_post("/v1/responses/compact", original.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["object"],
        "response.compaction"
    );
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].authorization, seen[1].authorization);
    assert_eq!(
        serde_json::from_slice::<Value>(&seen[0].body).unwrap(),
        original
    );
    let repaired: Value = serde_json::from_slice(&seen[1].body).unwrap();
    let mut expected_reasoning = original["input"][2].clone();
    expected_reasoning
        .as_object_mut()
        .unwrap()
        .remove("encrypted_content");
    assert_eq!(
        repaired["input"],
        json!([
            original["input"][0],
            expected_reasoning,
            original["input"][3],
            original["input"][4]
        ])
    );
    drop(seen);
    let records = h.state.records.records();
    assert_eq!(
        records[0]
            .gateway_actions
            .iter()
            .filter(|a| a.action == "compaction_repair")
            .count(),
        1
    );
}

#[tokio::test]
async fn compaction_repair_cannot_repeat_or_exceed_total_attempt_budget() {
    for budget in [1, 4] {
        let h = Harness::new(
            vec![
                recovery_error(StatusCode::BAD_REQUEST, "invalid_encrypted_content"),
                recovery_error(StatusCode::BAD_REQUEST, "invalid_encrypted_content"),
                compact_success(),
            ],
            2,
            &[ASTRA],
        )
        .await;
        h.state.config.write().unwrap().max_attempts = budget;
        let response = h
            .api_post("/v1/responses/compact", compaction_request())
            .send()
            .await
            .unwrap();
        assert!(!response.status().is_success());
        response.bytes().await.unwrap();
        assert_eq!(h.calls(), budget.min(2));
        assert_eq!(h.state.records.records()[0].attempts.len(), budget.min(2));
        assert!(
            h.state.records.records()[0]
                .gateway_actions
                .iter()
                .filter(|a| a.action == "compaction_repair")
                .count()
                <= 1
        );
    }
}

#[tokio::test]
async fn encrypted_content_is_unchanged_outside_explicit_compaction_400_recovery() {
    for (path, status, code, repair_enabled) in [
        (
            "/v1/responses",
            StatusCode::BAD_REQUEST,
            "invalid_encrypted_content",
            true,
        ),
        (
            "/v1/responses/compact",
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            true,
        ),
        (
            "/v1/responses/compact",
            StatusCode::BAD_GATEWAY,
            "invalid_encrypted_content",
            true,
        ),
        (
            "/v1/responses/compact",
            StatusCode::BAD_REQUEST,
            "invalid_encrypted_content",
            false,
        ),
    ] {
        let h = Harness::new(
            vec![recovery_error(status, code), compact_success()],
            2,
            &[ASTRA],
        )
        .await;
        h.state.config.write().unwrap().compaction_repair_enabled = repair_enabled;
        let original = compaction_request();
        let response = h.api_post(path, original.clone()).send().await.unwrap();
        response.bytes().await.unwrap();
        let expected_calls = if status == StatusCode::BAD_GATEWAY {
            2
        } else {
            1
        };
        let seen = h.mock.seen.lock().unwrap();
        assert_eq!(seen.len(), expected_calls, "{path} {status} {code}");
        for request in seen.iter() {
            assert_eq!(
                serde_json::from_slice::<Value>(&request.body).unwrap(),
                original
            );
        }
        drop(seen);
        assert!(
            !h.state.records.records()[0]
                .gateway_actions
                .iter()
                .any(|a| a.action == "compaction_repair")
        );
    }
}

#[tokio::test]
async fn consecutive_429_rebuilds_session_once_on_same_key_and_reuses_mapping() {
    let h = Harness::new(
        vec![
            recovery_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_exceeded"),
            recovery_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_exceeded"),
            success("resp_session_recovered", ASTRA),
            success("resp_session_followup", ASTRA),
        ],
        1,
        &[ASTRA],
    )
    .await;
    use_short_cooldown(&h);
    let body = session_request();
    let response = h
        .post(body.clone())
        .header("session-id", "fixture-session")
        .send()
        .await
        .unwrap();
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("resp_session_recovered")
    );
    let mapped = {
        let seen = h.mock.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].headers["session-id"], "fixture-session");
        assert_eq!(seen[1].headers["session-id"], "fixture-session");
        assert_eq!(seen[0].authorization, seen[1].authorization);
        assert_eq!(seen[1].authorization, seen[2].authorization);
        let mapped = seen[2].headers["session-id"].to_str().unwrap().to_owned();
        assert_ne!(mapped, "fixture-session");
        assert!(uuid::Uuid::parse_str(&mapped).is_ok());
        let sent: Value = serde_json::from_slice(&seen[2].body).unwrap();
        assert_eq!(sent["prompt_cache_key"], mapped);
        assert_eq!(sent["client_metadata"]["session_id"], mapped);
        assert_eq!(sent["client_metadata"]["thread_id"], mapped);
        assert_eq!(sent["input"], body["input"]);
        mapped
    };
    assert_eq!(
        h.state.records.records()[0]
            .gateway_actions
            .iter()
            .filter(|a| a.action == "session_reset")
            .count(),
        1
    );
    let mut continuation = body;
    continuation["previous_response_id"] = json!("resp_session_recovered");
    let response = h
        .post(continuation)
        .header("session-id", "fixture-session")
        .send()
        .await
        .unwrap();
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("resp_session_followup")
    );
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[2].authorization, seen[3].authorization);
    assert_eq!(seen[3].headers["session-id"], mapped);
    assert_eq!(
        serde_json::from_slice::<Value>(&seen[3].body).unwrap()["previous_response_id"],
        "resp_session_recovered"
    );
}

#[tokio::test]
async fn repeated_sse_rate_limits_rotate_session_at_most_once_per_request() {
    let h = Harness::new(
        vec![
            Reply::Sse(vec![(None, failed("resp_hidden_first", ASTRA))]),
            Reply::Sse(vec![(None, failed("resp_hidden_second", ASTRA))]),
            Reply::Sse(vec![(None, failed("resp_hidden_third", ASTRA))]),
            success("resp_single_rotation", ASTRA),
        ],
        1,
        &[ASTRA],
    )
    .await;
    use_short_cooldown(&h);
    let response = h
        .post(session_request())
        .header("session-id", "fixture-session")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    assert!(text.contains("resp_single_rotation"));
    assert!(!text.contains("resp_hidden"));
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[0].headers["session-id"], "fixture-session");
    assert_eq!(seen[1].headers["session-id"], "fixture-session");
    assert_ne!(seen[2].headers["session-id"], "fixture-session");
    assert_eq!(seen[2].headers["session-id"], seen[3].headers["session-id"]);
    assert!(
        seen.iter()
            .all(|r| r.authorization == seen[0].authorization)
    );
    drop(seen);
    assert_eq!(
        h.state.records.records()[0]
            .gateway_actions
            .iter()
            .filter(|a| a.action == "session_reset")
            .count(),
        1
    );
}

#[tokio::test]
async fn bound_resource_502_rebuilds_session_but_generic_502_and_single_429_do_not() {
    for (status, code, enabled, expected_reset) in [
        (StatusCode::BAD_GATEWAY, "resource_not_found", true, true),
        (StatusCode::BAD_GATEWAY, "server_error", true, false),
        (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_exceeded",
            true,
            false,
        ),
        (StatusCode::BAD_GATEWAY, "resource_not_found", false, false),
    ] {
        let h = Harness::new(
            vec![
                recovery_error(status, code),
                success("resp_session_result", ASTRA),
            ],
            2,
            &[ASTRA],
        )
        .await;
        use_short_cooldown(&h);
        h.state.config.write().unwrap().session_recovery_enabled = enabled;
        let original = session_request();
        let response = h
            .post(original.clone())
            .header("session-id", "fixture-session")
            .send()
            .await
            .unwrap();
        assert!(
            response
                .text()
                .await
                .unwrap()
                .contains("resp_session_result")
        );
        let seen = h.mock.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[1].headers["session-id"] != "fixture-session",
            expected_reset
        );
        if expected_reset {
            assert_eq!(seen[0].authorization, seen[1].authorization);
        } else {
            assert_eq!(
                serde_json::from_slice::<Value>(&seen[1].body).unwrap(),
                original
            );
        }
        drop(seen);
        assert_eq!(
            h.state.records.records()[0]
                .gateway_actions
                .iter()
                .any(|a| a.action == "session_reset"),
            expected_reset
        );
    }
}

#[tokio::test]
async fn separated_429_errors_do_not_count_as_consecutive_session_rejections() {
    let h = Harness::new(
        vec![
            recovery_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_exceeded"),
            recovery_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error"),
            recovery_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_exceeded"),
            success("resp_not_consecutive", ASTRA),
        ],
        1,
        &[ASTRA],
    )
    .await;
    use_short_cooldown(&h);
    let original = session_request();
    let response = h
        .post(original.clone())
        .header("session-id", "fixture-session")
        .send()
        .await
        .unwrap();
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("resp_not_consecutive")
    );
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    for request in seen.iter() {
        assert_eq!(request.headers["session-id"], "fixture-session");
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            original
        );
    }
    drop(seen);
    assert!(
        !h.state.records.records()[0]
            .gateway_actions
            .iter()
            .any(|a| a.action == "session_reset")
    );
}

#[tokio::test]
async fn session_rebuild_never_exceeds_total_attempt_budget_or_runs_without_identity() {
    for with_identity in [true, false] {
        let h = Harness::new(
            vec![
                recovery_error(StatusCode::BAD_GATEWAY, "resource_not_found"),
                success("resp_forbidden_extra_attempt", ASTRA),
            ],
            2,
            &[ASTRA],
        )
        .await;
        h.state.config.write().unwrap().max_attempts = if with_identity { 1 } else { 2 };
        let original = if with_identity {
            session_request()
        } else {
            Harness::request(ASTRA)
        };
        let response = h.post(original.clone()).send().await.unwrap();
        let status = response.status();
        response.bytes().await.unwrap();
        if with_identity {
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(h.calls(), 1);
        } else {
            assert_eq!(status, StatusCode::OK);
            assert_eq!(h.calls(), 2);
            let seen = h.mock.seen.lock().unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&seen[1].body).unwrap(),
                original
            );
            assert!(!seen[1].headers.contains_key("session-id"));
            assert!(
                !h.state.records.records()[0]
                    .gateway_actions
                    .iter()
                    .any(|a| a.action == "session_reset")
            );
        }
    }
}

#[tokio::test]
async fn previous_response_id_prevents_session_rebuild_for_502_and_repeated_429() {
    for status in [StatusCode::BAD_GATEWAY, StatusCode::TOO_MANY_REQUESTS] {
        let code = if status == StatusCode::BAD_GATEWAY {
            "resource_not_found"
        } else {
            "rate_limit_exceeded"
        };
        let h = Harness::new(
            vec![
                success("resp_bound_parent", ASTRA),
                recovery_error(status, code),
                recovery_error(status, code),
                recovery_error(status, code),
            ],
            2,
            &[ASTRA],
        )
        .await;
        use_short_cooldown(&h);
        h.state.config.write().unwrap().max_attempts = 3;
        let mut continuation = session_request();
        h.post(continuation.clone())
            .header("session-id", "fixture-session")
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        continuation["previous_response_id"] = json!("resp_bound_parent");
        let response = h
            .post(continuation.clone())
            .header("session-id", "fixture-session")
            .send()
            .await
            .unwrap();
        assert!(!response.status().is_success());
        response.bytes().await.unwrap();
        let seen = h.mock.seen.lock().unwrap();
        assert!(seen.len() >= 2 && seen.len() <= 4);
        for request in seen.iter().skip(1) {
            assert_eq!(request.authorization, seen[0].authorization);
            assert_eq!(request.headers["session-id"], "fixture-session");
            assert_eq!(
                serde_json::from_slice::<Value>(&request.body).unwrap(),
                continuation
            );
        }
        drop(seen);
        assert!(
            !h.state
                .records
                .records()
                .iter()
                .flat_map(|r| &r.gateway_actions)
                .any(|a| a.action == "session_reset")
        );
    }
}

#[tokio::test]
async fn rate_limit_counts_are_isolated_between_accounts_in_the_same_session() {
    let h = Harness::new(
        vec![
            recovery_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_exceeded"),
            recovery_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_exceeded"),
            success("resp_two_independent_rejections", ASTRA),
        ],
        2,
        &[ASTRA],
    )
    .await;
    use_short_cooldown(&h);
    let original = session_request();
    let response = h
        .post(original.clone())
        .header("session-id", "fixture-session")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("resp_two_independent_rejections")
    );
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_ne!(seen[0].authorization, seen[1].authorization);
    for request in seen.iter() {
        assert_eq!(request.headers["session-id"], "fixture-session");
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            original
        );
    }
    drop(seen);
    assert!(
        !h.state.records.records()[0]
            .gateway_actions
            .iter()
            .any(|a| a.action == "session_reset")
    );
}

#[tokio::test]
async fn json_and_sse_errors_record_bounded_messages_with_all_fixture_credentials_redacted() {
    for transport in ["json", "sse_prelude", "sse_after_output"] {
        let h = Harness::new(vec![], 2, &[ASTRA]).await;
        h.state.config.write().unwrap().max_attempts = 1;
        let local_token = h.state.settings().local_token;
        let message = format!(
            "fixture upstream resource rejected; loaded={TEST_KEY_A} other={TEST_KEY_B} local={local_token}; \
            Bearer opaque-fixture-value; api_key=unloaded-fixture-value; \
            https://fixture-user:fixture-password@api.example.com/v1?credential=fixture-query; \
            line\nwith\tcontrols\u{202e} {}",
            "测".repeat(1500)
        );
        let error = json!({"code":"rate_limit_exceeded","message":message});
        let reply = if transport == "json" {
            Reply::Json(StatusCode::TOO_MANY_REQUESTS, json!({"error":error}), None)
        } else {
            let mut chunks = vec![(None, created("resp_error_details", ASTRA))];
            if transport == "sse_after_output" {
                chunks.push((None, delta("already-visible")));
            }
            chunks.push((
                None,
                frame(json!({"type":"response.failed","response":{
                    "id":"resp_error_details","model":ASTRA,"status":"failed","error":error
                }})),
            ));
            Reply::Sse(chunks)
        };
        h.mock.script.lock().unwrap().push_back(reply);
        let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
        response.bytes().await.unwrap();
        assert_eq!(h.calls(), 1);
        let public = h
            .api_get("/api/records")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        let attempts = public["records"][0]["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 1, "{transport}");
        let detail = attempts[0]["error_message"]
            .as_str()
            .expect("ordinary request error detail must reach the records API");
        assert!(
            detail.contains("fixture upstream resource rejected"),
            "{transport}: {detail}"
        );
        assert!(detail.contains("[REDACTED]"));
        assert!(detail.len() <= 2048);
        assert!(!detail.chars().any(char::is_control));
        assert!(!detail.contains('\u{202e}'));
        let serialized = public.to_string();
        for forbidden in [
            TEST_KEY_A,
            TEST_KEY_B,
            local_token.as_str(),
            "opaque-fixture-value",
            "unloaded-fixture-value",
            "fixture-user",
            "fixture-password",
            "fixture-query",
        ] {
            assert!(
                !serialized.contains(forbidden),
                "{transport} exposed a fixture credential"
            );
        }
        assert_eq!(
            h.state.records.records()[0].attempts[0]
                .error_message
                .as_deref(),
            Some(detail)
        );
    }
}

#[tokio::test]
async fn manual_sync_excludes_permanent_failure_until_successful_revalidation() {
    let h = Harness::new(
        vec![
            Reply::Json(
                StatusCode::UNAUTHORIZED,
                json!({"error":{"code":"invalid_api_key","message":"fixture rejected"}}),
                None,
            ),
            success("resp_revalidated", ASTRA),
            Reply::Json(
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error":{"code":"rate_limit_exceeded","message":"fixture busy"}}),
                None,
            ),
            success("resp_normal_retry", ASTRA),
        ],
        0,
        &[ASTRA],
    )
    .await;
    h.install_biwrite_models_fixture(&[ASTRA]).await;
    h.state.config.write().unwrap().recording_enabled = false;
    assert_eq!(h.calls(), 0, "automatic source sync must not probe");

    let response = h.api_post("/api/sync", json!({})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(h.calls(), 1, "a probe must not retry");
    assert!(
        h.state
            .scheduler
            .snapshot(&h.state.settings().policy)
            .is_empty()
    );
    assert!(
        h.state.records.records().is_empty(),
        "permanent failure classification must not depend on monitoring records"
    );
    let public = h
        .api_get("/api/state")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(public["sync"]["running"], false);
    assert_eq!(public["sync"]["completed"], 1);
    assert_eq!(public["sync"]["total"], 1);
    assert_eq!(public["sync"]["healthy"], 0);
    assert_eq!(public["sync"]["removed"], 1);
    assert_eq!(public["sync"]["temporary"], 0);
    assert!(public["sync"]["last_completed_ms"].is_number());
    assert_eq!(public["source"]["providers"][0]["imported_keys"], 0);
    assert_eq!(public["source"]["providers"][0]["kept_keys"], 1);

    let all = h
        .api_post("/api/config", json!({"biwrite_import_all":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(all.status(), StatusCode::OK);
    assert_eq!(
        h.state.scheduler.snapshot(&h.state.settings().policy).len(),
        1
    );
    assert_eq!(
        h.calls(),
        1,
        "all-key mode must ignore exclusions without probing"
    );
    let kept = h
        .api_post("/api/config", json!({"biwrite_import_all":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(kept.status(), StatusCode::OK);
    assert!(
        h.state
            .scheduler
            .snapshot(&h.state.settings().policy)
            .is_empty()
    );

    h.state.sync_source().await.unwrap();
    assert_eq!(
        h.calls(),
        1,
        "automatic sync must neither probe nor restore exclusions"
    );
    assert!(
        h.state
            .scheduler
            .snapshot(&h.state.settings().policy)
            .is_empty()
    );
    let response = h.api_post("/api/sync", json!({})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        h.calls(),
        2,
        "manual sync must revalidate the full source snapshot"
    );
    assert_eq!(
        h.state.scheduler.snapshot(&h.state.settings().policy).len(),
        1
    );
    let sync = h.state.sync.read().unwrap().clone();
    assert_eq!(
        (sync.completed, sync.healthy, sync.removed, sync.temporary),
        (1, 1, 0, 0)
    );

    h.state.config.write().unwrap().recording_enabled = true;
    let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        h.calls(),
        4,
        "ordinary requests retain their retry behavior"
    );
    assert_eq!(h.state.records.records()[0].attempts.len(), 2);
}

#[tokio::test]
async fn manual_sync_retains_rate_limited_candidate_with_cooldown() {
    let h = Harness::new(
        vec![Reply::Json(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error":{"code":"rate_limit_exceeded","message":"fixture busy"}}),
            None,
        )],
        0,
        &[ASTRA],
    )
    .await;
    h.install_biwrite_models_fixture(&[ASTRA]).await;
    let response = h.api_post("/api/sync", json!({})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(h.calls(), 1);
    let accounts = h.state.scheduler.snapshot(&h.state.settings().policy);
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].models, [ASTRA]);
    assert!(accounts[0].model_health[0].cooldown_until_ms.is_some());
    let sync = h.state.sync.read().unwrap().clone();
    assert_eq!((sync.removed, sync.temporary), (0, 1));
    assert_eq!(h.state.source.read().unwrap().providers[0].imported_keys, 1);
}

#[tokio::test]
async fn manual_sync_removes_models_independently() {
    let h = Harness::new(
        vec![
            Reply::ForModel(
                ASTRA,
                Box::new(Reply::Json(
                    StatusCode::FORBIDDEN,
                    json!({"error":{"code":"permission_denied","message":"fixture denied"}}),
                    None,
                )),
            ),
            Reply::ForModel(SOL, Box::new(success("resp_remaining_model", SOL))),
        ],
        0,
        &[ASTRA, SOL],
    )
    .await;
    h.install_biwrite_models_fixture(&[ASTRA, SOL]).await;
    let response = h.api_post("/api/sync", json!({})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(h.calls(), 2);
    let accounts = h.state.scheduler.snapshot(&h.state.settings().policy);
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].models, [SOL]);
    let source = h.state.source.read().unwrap().clone();
    for provider in &source.providers {
        assert_eq!(provider.imported_keys, usize::from(provider.model == SOL));
        assert_eq!(provider.kept_keys, 1);
    }
    let sync = h.state.sync.read().unwrap().clone();
    assert_eq!(
        (sync.completed, sync.total, sync.healthy, sync.removed),
        (2, 2, 1, 1)
    );
}

#[tokio::test]
async fn manual_sync_treats_429_quota_as_permanent_without_changing_retry_classification() {
    let h = Harness::new(
        vec![Reply::Json(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error":{"code":"insufficient_quota","message":"fixture quota exhausted"}}),
            None,
        )],
        0,
        &[ASTRA],
    )
    .await;
    h.install_biwrite_models_fixture(&[ASTRA]).await;
    let response = h.api_post("/api/sync", json!({})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(h.calls(), 1);
    assert!(
        h.state
            .scheduler
            .snapshot(&h.state.settings().policy)
            .is_empty()
    );
    let sync = h.state.sync.read().unwrap().clone();
    assert_eq!((sync.removed, sync.temporary), (1, 0));
    let record = &h.state.records.records()[0];
    assert_eq!(record.attempts[0].reason, "rate_limit_exceeded");
}

#[tokio::test]
async fn exclusions_are_scoped_and_preserved_across_upstream_validations() {
    let h = Harness::new(
        vec![
            Reply::Json(
                StatusCode::UNAUTHORIZED,
                json!({"error":{"code":"invalid_api_key","message":"fixture rejected"}}),
                None,
            ),
            success("resp_second_upstream", ASTRA),
        ],
        0,
        &[ASTRA],
    )
    .await;
    h.install_biwrite_models_fixture(&[ASTRA]).await;
    let response = h.api_post("/api/sync", json!({})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        h.state
            .scheduler
            .snapshot(&h.state.settings().policy)
            .is_empty()
    );

    let dir = h.state.settings().biwrite_dir;
    let settings_path = dir.join("settings.json");
    let original_settings = fs::read(&settings_path).unwrap();
    let mut settings: Value = serde_json::from_slice(&original_settings).unwrap();
    settings["providers"][0]["baseUrl"] = json!("https://api2.example.com/v1");
    fs::write(&settings_path, settings.to_string()).unwrap();
    *h.state.source_upstream_override.write().unwrap() =
        Some(h.upstream.replacen("127.0.0.1", "localhost", 1));
    let changed = h
        .api_post(
            "/api/config",
            json!({"upstream_base_url":"https://api2.example.com/v1"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::OK);
    assert_eq!(
        h.state.scheduler.snapshot(&h.state.settings().policy).len(),
        1
    );
    assert_eq!(h.calls(), 1, "changing upstream must not trigger a probe");
    let validated = h.api_post("/api/sync", json!({})).send().await.unwrap();
    assert_eq!(validated.status(), StatusCode::OK);
    assert_eq!(h.calls(), 2);

    fs::write(&settings_path, original_settings).unwrap();
    *h.state.source_upstream_override.write().unwrap() = Some(h.upstream.clone());
    let restored = h
        .api_post(
            "/api/config",
            json!({"upstream_base_url":"https://api.example.com/v1"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(restored.status(), StatusCode::OK);
    assert!(
        h.state
            .scheduler
            .snapshot(&h.state.settings().policy)
            .is_empty(),
        "validating B must not clear A's permanent exclusion"
    );
    assert_eq!(h.calls(), 2);
}

#[tokio::test]
async fn model_override_availability_uses_the_filtered_prospective_snapshot() {
    let h = Harness::new(
        vec![Reply::Json(
            StatusCode::UNAUTHORIZED,
            json!({"error":{"code":"invalid_api_key","message":"fixture rejected"}}),
            None,
        )],
        0,
        &[ASTRA],
    )
    .await;
    h.install_biwrite_models_fixture(&[ASTRA]).await;
    let response = h.api_post("/api/sync", json!({})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let changed = h
        .api_post(
            "/api/config",
            json!({"upstream_base_url":"https://api.example.com/v1","model_override":ASTRA}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        changed.json::<Value>().await.unwrap()["error"]["code"],
        "model_unavailable"
    );
    assert_eq!(h.state.settings().model_override, None);
    assert_eq!(h.calls(), 1);
}

#[tokio::test]
async fn validation_preserves_formal_exclusions_and_duplicate_sync_returns_conflict() {
    let gate = Arc::new(Notify::new());
    let h = Harness::new(
        vec![
            Reply::Json(
                StatusCode::UNAUTHORIZED,
                json!({"error":{"code":"invalid_api_key","message":"fixture rejected"}}),
                None,
            ),
            Reply::Sse(vec![
                (None, created("resp_revalidation_pending", ASTRA)),
                (
                    Some(gate.clone()),
                    completed("resp_revalidation_pending", ASTRA),
                ),
            ]),
        ],
        0,
        &[ASTRA],
    )
    .await;
    h.install_biwrite_models_fixture(&[ASTRA]).await;
    let response = h.api_post("/api/sync", json!({})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        h.state
            .scheduler
            .snapshot(&h.state.settings().policy)
            .is_empty()
    );

    let first = h.api_post("/api/sync", json!({}));
    let first = tokio::spawn(async move { first.send().await.unwrap() });
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.calls() != 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("revalidation should reach the gated local upstream");
    assert!(h.state.sync.read().unwrap().running);
    assert!(
        h.state
            .scheduler
            .snapshot(&h.state.settings().policy)
            .is_empty()
    );

    let duplicate = h.api_post("/api/sync", json!({})).send().await.unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    assert_eq!(
        duplicate.json::<Value>().await.unwrap()["error"]["code"],
        "sync_in_progress"
    );
    let ordinary = h.post(Harness::request(ASTRA)).send().await.unwrap();
    assert_eq!(ordinary.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        h.calls(),
        2,
        "ordinary routing and duplicate sync must not probe"
    );
    assert!(
        h.state
            .scheduler
            .snapshot(&h.state.settings().policy)
            .is_empty()
    );

    gate.notify_one();
    let first = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("gated revalidation should finish")
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(h.calls(), 2);
    assert_eq!(
        h.state.scheduler.snapshot(&h.state.settings().policy).len(),
        1
    );
}

#[tokio::test]
async fn validation_shares_formal_capacity_while_new_pair_still_probes() {
    let gate = Arc::new(Notify::new());
    let h = Harness::new(
        vec![
            Reply::Sse(vec![
                (None, created("resp_formal_lease", ASTRA)),
                (None, delta("formal output")),
                (Some(gate.clone()), completed("resp_formal_lease", ASTRA)),
            ]),
            success("resp_new_model_validation", SOL),
            success("resp_formal_model_validation", ASTRA),
        ],
        0,
        &[ASTRA],
    )
    .await;
    h.install_biwrite_models_fixture(&[ASTRA]).await;
    let ordinary = h.post(Harness::request(ASTRA)).send().await.unwrap();
    assert_eq!(ordinary.status(), StatusCode::OK);
    assert_eq!(h.calls(), 1);
    assert_eq!(h.in_flight(), 1);

    h.write_biwrite_fixture(&[ASTRA, SOL], "p-validation");
    let first = h.api_post("/api/sync", json!({}));
    let first = tokio::spawn(async move { first.send().await.unwrap() });
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.calls() != 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the new model should validate while the formal model lease is held");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        h.calls(),
        2,
        "formal pair validation must not exceed its occupied capacity"
    );
    assert_eq!(h.in_flight(), 1);
    assert_eq!(h.state.sync.read().unwrap().completed, 1);
    assert!(h.state.sync.read().unwrap().running);

    gate.notify_one();
    assert!(ordinary.text().await.unwrap().contains("resp_formal_lease"));
    let first = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("formal pair validation should continue after the lease drains")
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(h.calls(), 3);
    let accounts = h.state.scheduler.snapshot(&h.state.settings().policy);
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].in_flight, 0);
    let formal = accounts[0]
        .model_health
        .iter()
        .find(|model| model.model == ASTRA)
        .unwrap();
    assert_eq!(
        formal.successes, 2,
        "formal cumulative health must be retained"
    );
    let new_model = accounts[0]
        .model_health
        .iter()
        .find(|model| model.model == SOL)
        .unwrap();
    assert_eq!(new_model.successes, 1);
}

#[tokio::test]
async fn paused_startup_initializes_read_only_and_periodic_pause_preserves_stale_pool() {
    let h = Harness::new(vec![], 0, &[ASTRA]).await;
    h.state.config.write().unwrap().sync_paused = true;
    h.install_biwrite_models_fixture(&[ASTRA]).await;
    assert!(
        h.state
            .scheduler
            .snapshot(&h.state.settings().policy)
            .is_empty()
    );

    h.state.sync_source_startup().await.unwrap();
    assert_eq!(
        h.state.scheduler.snapshot(&h.state.settings().policy).len(),
        1
    );
    assert_eq!(h.calls(), 0, "startup must never probe");
    assert!(!h.state.sync.read().unwrap().running);

    let dir = h.state.settings().biwrite_dir;
    let parked = h._dir.path().join("paused-startup-fixture");
    fs::rename(&dir, &parked).unwrap();
    h.state.source.write().unwrap().read_at_ms = 0;
    h.state.sync_source().await.unwrap();
    assert_eq!(
        h.state.scheduler.snapshot(&h.state.settings().policy).len(),
        1,
        "an explicit pause must preserve the pool even after read_at expires"
    );
    assert_eq!(h.calls(), 0);
}

#[tokio::test]
async fn stale_read_failure_clears_only_an_unpaused_pool() {
    let h = Harness::new(vec![], 0, &[ASTRA]).await;
    h.install_biwrite_models_fixture(&[ASTRA]).await;
    assert_eq!(
        h.state.scheduler.snapshot(&h.state.settings().policy).len(),
        1
    );
    let dir = h.state.settings().biwrite_dir;
    let parked = h._dir.path().join("read-failure-fixture");
    fs::rename(&dir, &parked).unwrap();
    h.state.source.write().unwrap().read_at_ms = 0;
    assert!(h.state.sync_source().await.is_err());
    assert!(
        h.state
            .scheduler
            .snapshot(&h.state.settings().policy)
            .is_empty()
    );
    assert_eq!(h.calls(), 0);
}

#[tokio::test]
async fn paused_source_sync_preserves_active_pool_and_allows_explicit_sync_then_resume() {
    let h = Harness::new(
        vec![
            success("resp_while_sync_paused", ASTRA),
            success("resp_manual_sync", ASTRA),
        ],
        1,
        &[ASTRA],
    )
    .await;
    let before_id = h.state.scheduler.snapshot(&h.state.settings().policy)[0]
        .id
        .clone();
    let paused = h
        .api_post("/api/config", json!({"sync_paused":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);
    assert!(h.state.settings().sync_paused);
    h.install_biwrite_fixture().await;
    let dir = h.state.settings().biwrite_dir;
    let settings_path = dir.join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    let fingerprint_b = blake3::hash(TEST_KEY_B.as_bytes()).to_hex().to_string()[..12].to_owned();
    settings["keptKeys"]["p-auto-fixture"] = json!([fingerprint_b]);
    fs::write(&settings_path, settings.to_string()).unwrap();
    fs::write(
        dir.join("keys.json"),
        json!({"keys":{"p-auto-fixture":TEST_KEY_B}}).to_string(),
    )
    .unwrap();
    let parked = h._dir.path().join("paused-biwrite-fixture");
    fs::rename(&dir, &parked).unwrap();
    h.state.sync_source().await.unwrap();
    assert_eq!(
        h.state.scheduler.snapshot(&h.state.settings().policy)[0].id,
        before_id
    );
    assert!(h.state.source.read().unwrap().last_error.is_none());
    let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("resp_while_sync_paused")
    );
    assert_eq!(
        h.mock.seen.lock().unwrap()[0].authorization,
        format!("Bearer {TEST_KEY_A}")
    );
    fs::rename(&parked, &dir).unwrap();

    let sync = h.api_post("/api/sync", json!({})).send().await.unwrap();
    assert_eq!(sync.status(), StatusCode::OK);
    assert!(h.state.settings().sync_paused);
    let expected_b = blake3::hash(TEST_KEY_B.as_bytes()).to_hex().to_string();
    let snapshot = h.state.scheduler.snapshot(&h.state.settings().policy);
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].id, expected_b);
    // The manual sync above verified the new candidate against the local mock.
    let selection = Selection {
        model: ASTRA.into(),
        ..Selection::default()
    };
    let lease = h
        .state
        .scheduler
        .try_acquire(&selection, &h.state.settings().policy)
        .unwrap();
    assert_eq!(lease.account.id, expected_b);
    drop(lease);

    let fingerprint_a = blake3::hash(TEST_KEY_A.as_bytes()).to_hex().to_string()[..12].to_owned();
    settings["keptKeys"]["p-auto-fixture"] = json!([fingerprint_a]);
    fs::write(&settings_path, settings.to_string()).unwrap();
    fs::write(
        dir.join("keys.json"),
        json!({"keys":{"p-auto-fixture":TEST_KEY_A}}).to_string(),
    )
    .unwrap();
    h.state.sync_source().await.unwrap();
    assert_eq!(
        h.state.scheduler.snapshot(&h.state.settings().policy)[0].id,
        expected_b
    );
    let resumed = h
        .api_post("/api/config", json!({"sync_paused":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);
    assert!(!h.state.settings().sync_paused);
    assert_eq!(
        h.state.scheduler.snapshot(&h.state.settings().policy)[0].id,
        before_id
    );
    assert!(
        !pool_router::config::load(&h.state.config_path)
            .unwrap()
            .sync_paused
    );
    assert_eq!(h.calls(), 2);
}

#[tokio::test]
async fn previous_response_keeps_its_original_session_after_later_independent_rotations() {
    let h = Harness::new(
        vec![
            success("resp_original_session", ASTRA),
            recovery_error(StatusCode::BAD_GATEWAY, "resource_not_found"),
            success("resp_first_rotated_session", ASTRA),
            recovery_error(StatusCode::BAD_GATEWAY, "resource_not_found"),
            success("resp_second_rotated_session", ASTRA),
            success("resp_first_session_continuation", ASTRA),
            success("resp_original_session_continuation", ASTRA),
            success("resp_latest_independent_request", ASTRA),
        ],
        1,
        &[ASTRA],
    )
    .await;
    use_short_cooldown(&h);
    let original = session_request();
    for expected in [
        "resp_original_session",
        "resp_first_rotated_session",
        "resp_second_rotated_session",
    ] {
        let response = h
            .post(original.clone())
            .header("session-id", "fixture-session")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.text().await.unwrap().contains(expected));
    }
    let (first_session, second_session) = {
        let seen = h.mock.seen.lock().unwrap();
        assert_eq!(seen.len(), 5);
        assert_eq!(seen[0].headers["session-id"], "fixture-session");
        assert_eq!(seen[1].headers["session-id"], "fixture-session");
        let first = seen[2].headers["session-id"].to_str().unwrap().to_owned();
        let second = seen[4].headers["session-id"].to_str().unwrap().to_owned();
        assert!(uuid::Uuid::parse_str(&first).is_ok());
        assert!(uuid::Uuid::parse_str(&second).is_ok());
        assert_ne!(first, second);
        assert_eq!(seen[3].headers["session-id"], first);
        (first, second)
    };
    for (parent, expected) in [
        (
            "resp_first_rotated_session",
            "resp_first_session_continuation",
        ),
        (
            "resp_original_session",
            "resp_original_session_continuation",
        ),
    ] {
        let mut continuation = original.clone();
        continuation["previous_response_id"] = json!(parent);
        let response = h
            .post(continuation)
            .header("session-id", "fixture-session")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.text().await.unwrap().contains(expected));
    }
    let response = h
        .post(original.clone())
        .header("session-id", "fixture-session")
        .send()
        .await
        .unwrap();
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("resp_latest_independent_request")
    );
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 8);
    for (index, expected_session, previous) in [
        (
            5,
            first_session.as_str(),
            Some("resp_first_rotated_session"),
        ),
        (6, "fixture-session", Some("resp_original_session")),
        (7, second_session.as_str(), None),
    ] {
        assert_eq!(seen[index].authorization, seen[0].authorization);
        assert_eq!(seen[index].headers["session-id"], expected_session);
        let body: Value = serde_json::from_slice(&seen[index].body).unwrap();
        assert_eq!(body["prompt_cache_key"], expected_session);
        assert_eq!(body["client_metadata"]["session_id"], expected_session);
        assert_eq!(body["client_metadata"]["thread_id"], expected_session);
        assert_eq!(body["input"], original["input"]);
        assert_eq!(body["previous_response_id"].as_str(), previous);
    }
}

#[tokio::test]
async fn flat_response_error_recovers_only_before_output_and_records_redacted_message() {
    for visible_output in [false, true] {
        let h = Harness::new(vec![], 1, &[ASTRA]).await;
        use_short_cooldown(&h);
        let message = format!("fixture flat resource unavailable; credential {TEST_KEY_A}");
        let flat_error = format!(
            "event: response.error\ndata: {}\n\n",
            json!({"status":502,"code":"resource_not_found","message":message})
        );
        let mut chunks = vec![(None, created("resp_flat_error", ASTRA))];
        if visible_output {
            chunks.push((None, delta("already-visible")));
        }
        chunks.push((None, flat_error));
        h.mock.script.lock().unwrap().extend([
            Reply::Sse(chunks),
            success("resp_after_flat_resource_error", ASTRA),
        ]);
        let response = h
            .post(session_request())
            .header("session-id", "fixture-session")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let text = response.text().await.unwrap();
        let seen = h.mock.seen.lock().unwrap();
        if visible_output {
            assert_eq!(seen.len(), 1);
            assert!(text.contains("already-visible"));
            assert!(text.contains("event: response.error"));
            assert!(!text.contains("resp_after_flat_resource_error"));
        } else {
            assert_eq!(seen.len(), 2);
            assert_eq!(seen[0].authorization, seen[1].authorization);
            assert_ne!(seen[1].headers["session-id"], "fixture-session");
            assert!(text.contains("resp_after_flat_resource_error"));
            assert!(!text.contains("resp_flat_error"));
            assert!(!text.contains("event: response.error"));
        }
        drop(seen);
        let records = h.state.records.records();
        let record = &records[0];
        assert_eq!(record.attempts.len(), if visible_output { 1 } else { 2 });
        assert_eq!(record.attempts[0].reason, "resource_not_found");
        let detail = record.attempts[0].error_message.as_deref().unwrap();
        assert!(detail.contains("fixture flat resource unavailable"));
        assert!(!detail.contains(TEST_KEY_A));
        assert!(detail.contains("[REDACTED]"));
        assert_eq!(
            record
                .gateway_actions
                .iter()
                .any(|a| a.action == "session_reset"),
            !visible_output
        );
        assert_eq!(
            record.outcome,
            if visible_output {
                "upstream_failed"
            } else {
                "completed"
            }
        );
        assert_eq!(h.in_flight(), 0);
    }
}

#[tokio::test]
async fn cancelling_a_manual_sync_clears_the_running_flag_and_allows_an_immediate_retry() {
    let gate = Arc::new(Notify::new());
    let h = Harness::new(
        vec![
            Reply::Sse(vec![
                (Some(gate.clone()), created("resp_probe_gated", ASTRA)),
                (None, completed("resp_probe_gated", ASTRA)),
            ]),
            success("resp_probe_retry", ASTRA),
        ],
        1,
        &[ASTRA],
    )
    .await;
    h.write_biwrite_fixture(&[ASTRA], "p-cancel-sync");
    let cancelled =
        tokio::time::timeout(Duration::from_millis(300), h.state.sync_source_now()).await;
    assert!(cancelled.is_err());
    let during: Value = h
        .api_get("/api/state")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(during["sync"]["running"], json!(false));
    gate.notify_waiters();
    assert!(h.state.sync_source_now().await.is_ok());
    let after: Value = h
        .api_get("/api/state")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["sync"]["running"], json!(false));
    assert_eq!(after["sync"]["healthy"], json!(1));
    assert!(after["sync"]["last_completed_ms"].as_u64().is_some());
    assert_eq!(
        h.state.scheduler.snapshot(&h.state.settings().policy).len(),
        1
    );
}

#[tokio::test]
async fn a_manual_sync_answers_with_the_result_of_that_very_round() {
    let h = Harness::new(vec![success("resp_sync_verify", ASTRA)], 0, &[ASTRA]).await;
    h.install_biwrite_models_fixture(&[ASTRA]).await;
    let body: Value = h
        .api_post("/api/sync", json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // The panel reports the summary from this body, so it never has to wait for
    // the next poll and never shows an empty result.
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["sync"]["running"], json!(false));
    assert_eq!(body["sync"]["total"], json!(1));
    assert_eq!(body["sync"]["healthy"], json!(1));
    assert_eq!(body["sync"]["removed"], json!(0));
    assert!(body["sync"]["last_completed_ms"].as_u64().is_some());
    let state: Value = h
        .api_get("/api/state")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["sync"], body["sync"]);
}

#[tokio::test]
async fn a_served_request_lands_in_the_daily_ledger_with_its_cost() {
    let h = Harness::new(vec![success("resp_billed", ASTRA)], 1, &[ASTRA]).await;
    let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
    response.bytes().await.unwrap();

    let value: Value = h
        .api_get("/api/usage")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // The fixture completion frame reports 100 input of which 80 were cached,
    // 12 output and 4 reasoning tokens.
    assert_eq!(value["today"]["requests"], json!(1));
    assert_eq!(value["today"]["uncounted_requests"], json!(0));
    assert_eq!(value["today"]["tokens"]["input"], json!(100));
    assert_eq!(value["today"]["tokens"]["cached"], json!(80));
    assert_eq!(value["today"]["tokens"]["ordinary"], json!(20));
    assert_eq!(value["today"]["tokens"]["output"], json!(12));
    assert_eq!(value["today"]["tokens"]["reasoning"], json!(4));
    assert!(value["today"]["cost_usd"].as_f64().expect("cost") > 0.0);
    let models = value["by_model"].as_array().expect("rows");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["name"], json!(ASTRA));
    assert_eq!(models[0]["requests"], json!(1));
    let efforts = value["by_effort"].as_array().expect("rows");
    assert_eq!(efforts[0]["name"], json!("high"));
    assert_eq!(value["by_key"].as_array().expect("rows").len(), 1);
}

#[tokio::test]
async fn a_request_that_never_reported_usage_is_listed_as_uncounted() {
    let h = Harness::new(
        vec![Reply::Json(
            StatusCode::BAD_REQUEST,
            json!({"error":{"code":"bad_request","message":"fixture rejected"}}),
            None,
        )],
        1,
        &[ASTRA],
    )
    .await;
    use_short_cooldown(&h);
    let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
    response.bytes().await.unwrap();

    let value: Value = h
        .api_get("/api/usage")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(value["today"]["requests"], json!(0));
    assert!(
        value["today"]["uncounted_requests"]
            .as_u64()
            .expect("count")
            >= 1
    );
    assert_eq!(value["today"]["tokens"]["input"], json!(0));
    assert_eq!(value["today"]["cost_usd"], json!(0.0));
}

#[tokio::test]
async fn a_heartbeat_never_splits_an_upstream_event_across_the_wire() {
    // The upstream stops mid line and resumes only after a heartbeat is due.
    // Inserting the comment there would corrupt the event and the client would
    // report a decoding error instead of the result.
    let resume = Arc::new(Notify::new());
    let event = completed("resp_split", ASTRA);
    let cut = event.len() / 2;
    let (head, tail) = event.split_at(cut);
    assert!(!head.ends_with('\n'), "the fixture must stop mid line");
    let h = Harness::new(
        vec![Reply::Sse(vec![
            (None, created("resp_split", ASTRA)),
            (None, delta("partial")),
            (None, head.to_owned()),
            (Some(resume.clone()), tail.to_owned()),
        ])],
        1,
        &[ASTRA],
    )
    .await;
    {
        // The pause must come from the fixture, not from an idle deadline.
        let mut config = h.state.config.write().unwrap();
        config.queue_timeout_ms = 30_000;
        config.upstream_header_timeout_ms = 30_000;
        config.stream_idle_timeout_ms = 30_000;
        config.first_output_timeout_ms = 30_000;
    }
    let release = tokio::spawn(async move {
        // Longer than the five second heartbeat interval.
        tokio::time::sleep(Duration::from_secs(7)).await;
        resume.notify_one();
    });
    let response = h
        .post(Harness::request(ASTRA))
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    release.await.unwrap();

    assert!(body.contains("resp_split"), "{body}");
    // Every comment must occupy a whole line of its own.
    for (index, line) in body.lines().enumerate() {
        if line.contains(": pool-router waiting") {
            assert!(
                line.trim_start().starts_with(':'),
                "line {index} carries a comment inside other content: {line}"
            );
        }
    }
    // The split event must arrive intact, not interrupted by a comment.
    assert!(
        !body.contains("pool-router waiting\n\n\"status\""),
        "a comment landed inside the completion event: {body}"
    );
    for line in body.lines().filter(|line| line.starts_with("data:")) {
        let payload = line.trim_start_matches("data:").trim();
        serde_json::from_str::<Value>(payload)
            .unwrap_or_else(|e| panic!("client could not decode {payload}: {e}"));
    }
}

#[tokio::test]
async fn a_slow_account_is_overtaken_by_a_hedge_on_another_account() {
    // The first account holds its response headers far longer than the hedge
    // delay; the second answers at once and must win the race.
    let h = Harness::new(
        vec![
            Reply::SlowHeaders(
                Duration::from_secs(9),
                Box::new(success("resp_slow", ASTRA)),
            ),
            success("resp_hedged", ASTRA),
        ],
        2,
        &[ASTRA],
    )
    .await;
    {
        let mut config = h.state.config.write().unwrap();
        config.upstream_header_timeout_ms = 20_000;
        config.stream_idle_timeout_ms = 20_000;
        config.queue_timeout_ms = 20_000;
        config.first_output_timeout_ms = 20_000;
        config.hedge_after_ms = 1_000;
    }
    let started = std::time::Instant::now();
    let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    let elapsed = started.elapsed();

    assert!(
        body.contains("resp_hedged"),
        "the hedge should have won: {body}"
    );
    assert!(
        elapsed < Duration::from_secs(7),
        "the client waited for the slow account: {elapsed:?}"
    );
    assert_eq!(h.calls(), 2, "both accounts were asked");
    let records = h.state.records.records();
    let actions: Vec<String> = records
        .iter()
        .flat_map(|record| record.gateway_actions.iter())
        .map(|action| action.action.clone())
        .collect();
    assert!(
        actions.iter().any(|action| action == "hedged_attempt"),
        "records={} actions={actions:?}",
        records.len()
    );
    // Accounting must follow the account that actually served the request.
    assert!(
        actions.iter().any(|action| action == "hedged_win"),
        "the winner was not adopted: {actions:?}"
    );
    assert!(records[0].key_id.is_some());
}

#[tokio::test]
async fn a_hedge_never_starts_when_the_delay_exceeds_the_budget() {
    let h = Harness::new(vec![success("resp_single", ASTRA)], 2, &[ASTRA]).await;
    {
        let mut config = h.state.config.write().unwrap();
        // The default budget is shorter than the hedge delay, so racing would
        // only ever waste a second account.
        config.hedge_after_ms = 60_000;
    }
    let response = h.post(Harness::request(ASTRA)).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    assert_eq!(h.calls(), 1, "only one account should have been used");
    let records = h.state.records.records();
    assert!(
        !records[0]
            .gateway_actions
            .iter()
            .any(|action| action.action == "hedged_attempt")
    );
}

#[tokio::test]
async fn the_panel_can_tune_or_switch_off_the_hedge_but_not_set_a_useless_delay() {
    let h = Harness::new(vec![success("resp_cfg", ASTRA)], 1, &[ASTRA]).await;

    let off = h
        .api_post("/api/config", json!({"hedge_after_ms": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(off.status(), StatusCode::OK);
    assert_eq!(h.state.settings().hedge_after_ms, 0);

    let tuned = h
        .api_post("/api/config", json!({"hedge_after_ms": 8_000}))
        .send()
        .await
        .unwrap();
    assert_eq!(tuned.status(), StatusCode::OK);
    assert_eq!(h.state.settings().hedge_after_ms, 8_000);

    // Sub-second delays would race every request against a second account.
    let rejected = h
        .api_post("/api/config", json!({"hedge_after_ms": 200}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(h.state.settings().hedge_after_ms, 8_000);
}

#[tokio::test]
async fn a_continuation_is_never_raced_because_its_session_lives_on_one_account() {
    // The follow-up holds its headers past the hedge delay. A hedge would have
    // to send it to an account that does not hold the session.
    let h = Harness::new(
        vec![
            success("resp_parent", ASTRA),
            Reply::SlowHeaders(
                Duration::from_secs(3),
                Box::new(success("resp_child", ASTRA)),
            ),
        ],
        2,
        &[ASTRA],
    )
    .await;
    {
        let mut config = h.state.config.write().unwrap();
        config.upstream_header_timeout_ms = 20_000;
        config.stream_idle_timeout_ms = 20_000;
        config.queue_timeout_ms = 20_000;
        config.first_output_timeout_ms = 20_000;
        config.hedge_after_ms = 1_000;
    }
    h.post(Harness::request(ASTRA))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    let mut followup = Harness::request(ASTRA);
    followup["previous_response_id"] = json!("resp_parent");
    let second = h.post(followup).send().await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    assert!(second.text().await.unwrap().contains("resp_child"));

    assert_eq!(h.calls(), 2, "the continuation was sent once");
    let seen = h.mock.seen.lock().unwrap();
    assert_eq!(seen[0].authorization, seen[1].authorization);
    drop(seen);
    let actions: Vec<String> = h
        .state
        .records
        .records()
        .iter()
        .flat_map(|record| record.gateway_actions.iter())
        .map(|action| action.action.clone())
        .collect();
    assert!(
        !actions.iter().any(|action| action.starts_with("hedged")),
        "a continuation must not be raced: {actions:?}"
    );
}
