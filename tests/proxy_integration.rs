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
        body,
    });
    let reply = state.script.lock().unwrap().pop_front();
    match reply {
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
            [TEST_KEY_A, TEST_KEY_B]
                .into_iter()
                .take(accounts)
                .map(|secret| account(secret, &upstream, models))
                .collect(),
        );
        let proxy_router = proxy::router(state.clone());
        let proxy_job = tokio::spawn(async move {
            axum::serve(proxy_listener, proxy_router).await.unwrap();
        });
        Self {
            state,
            base: format!("http://{listen}"),
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

    async fn install_biwrite_fixture(&self) {
        let dir = self.state.settings().biwrite_dir;
        fs::create_dir_all(&dir).unwrap();
        let fingerprint = &blake3::hash(TEST_KEY_A.as_bytes()).to_hex().to_string()[..12];
        fs::write(
            dir.join("settings.json"),
            json!({
                "keysInFile": true,
                "providers": [{"id":"p-auto-fixture","name":"自动测试","kind":"openai_compatible",
                    "wireApi":"responses","baseUrl":"https://anyrouter.top/v1","model":ASTRA,
                    "hasKey":true,"keyCount":1,"keyConcurrency":1}],
                "keptKeys":{"p-auto-fixture":[fingerprint]}
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            dir.join("keys.json"),
            json!({"keys":{"p-auto-fixture":TEST_KEY_A}}).to_string(),
        )
        .unwrap();
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
        assert_eq!(h.state.settings().policy.strategy, Strategy::Random);
        h.state.sync_source().await.unwrap();
        let accounts = h.state.scheduler.snapshot(&h.state.settings().policy);
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, id);
        assert_eq!(accounts[0].models, [SOL]);
        let public_state = h.api_get("/api/state").send().await.unwrap().json::<Value>().await.unwrap();
        assert_eq!(public_state["config"]["source_mode"], "manual");
        assert_eq!(public_state["config"]["policy"]["strategy"], "random");
        assert_eq!(public_state["accounts"].as_array().unwrap().len(), 1);
        assert_eq!(public_state["accounts"][0]["id"], id);
        assert_eq!(public_state["accounts"][0]["models"], json!([SOL]));
        assert_eq!(public_state["connection"]["detail"], "isolated test");
        let public_text = public_state.to_string();
        assert!(!public_text.contains(TEST_MANUAL_KEY));
        assert!(!public_text.contains(TEST_KEY_A));
        assert!(!public_text.contains(&h.state.settings().local_token));
        // Selection proves manual source supports automatic scheduling without
        // ever sending fixture credentials to the fixed real upstream URL.
        let selection = Selection { model: SOL.into(), ..Selection::default() };
        let lease = h.state.scheduler.try_acquire(&selection, &h.state.settings().policy).unwrap();
        assert_eq!(lease.account.id, id);
        drop(lease);
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
