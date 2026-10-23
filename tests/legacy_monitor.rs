//! Isolated legacy-wiretap tests: only fixture credentials and localhost upstreams.
use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
};
use pool_router::{
    config::{Config, SourceMode},
    proxy::{self, AppState},
    scheduler::{SelectError, Selection},
    types::Account,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};
use tower::ServiceExt;

const CALLER: &str = "sk-legacy-fixture-caller-123456";
const STORED: &str = "sk-legacy-fixture-stored-654321";
const ASTRA: &str = "gpt-6-astra";
const SOL: &str = "gpt-6.1-sol";

struct Reply {
    status: StatusCode,
    content_type: &'static str,
    bytes: Vec<u8>,
    encoding: Option<&'static str>,
    delay_ms: u64,
}
impl Reply {
    fn json(value: Value) -> Self {
        Self {
            status: StatusCode::OK,
            content_type: "application/json",
            bytes: value.to_string().into_bytes(),
            encoding: None,
            delay_ms: 0,
        }
    }
    fn success(id: &str) -> Self {
        Self::json(
            json!({"id":id,"model":SOL,"status":"completed","usage":{"input_tokens":10,"output_tokens":2}}),
        )
    }
    fn sse(value: Value) -> Self {
        Self {
            status: StatusCode::OK,
            content_type: "text/event-stream",
            bytes: format!("data: {value}\n\n").into_bytes(),
            encoding: None,
            delay_ms: 0,
        }
    }
}
struct Seen {
    headers: HeaderMap,
    bytes: Bytes,
    uri: String,
}
#[derive(Clone)]
struct Upstream {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}
async fn upstream(State(s): State<Upstream>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
    s.seen.lock().unwrap().push(Seen {
        headers: parts.headers,
        bytes,
        uri: parts.uri.to_string(),
    });
    let r = s
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .expect("unexpected retry");
    let mut builder = Response::builder()
        .status(r.status)
        .header("content-type", r.content_type);
    if let Some(e) = r.encoding {
        builder = builder.header("content-encoding", e);
    }
    let stream = async_stream::stream! { if r.delay_ms>0 {tokio::time::sleep(Duration::from_millis(r.delay_ms)).await;} yield Ok::<Bytes,std::io::Error>(Bytes::from(r.bytes)); };
    builder.body(Body::from_stream(stream)).unwrap()
}
struct Harness {
    app: Router,
    state: Arc<AppState>,
    upstream: Upstream,
    task: JoinHandle<()>,
    _temp: tempfile::TempDir,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Harness {
    async fn new(replies: Vec<Reply>, with_pool: bool) -> Self {
        let upstream_state = Upstream {
            replies: Arc::new(Mutex::new(replies.into())),
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}/v1", listener.local_addr().unwrap());
        let mock = Router::new()
            .route("/v1/responses", post(upstream))
            .route("/v1/responses/compact", post(upstream))
            .with_state(upstream_state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let cfg = Config {
            biwrite_dir: temp.path().join("missing-biwrite"),
            model_override: Some(ASTRA.into()),
            reasoning_override: Some("high".into()),
            max_attempts: 3,
            queue_timeout_ms: 1000,
            upstream_header_timeout_ms: 1000,
            stream_idle_timeout_ms: 1000,
            ..Config::default()
        };
        let state = AppState::new(cfg, temp.path().join("config.json"), true).unwrap();
        *state.legacy_upstream_override.write().unwrap() = Some(origin.clone());
        if with_pool {
            state.scheduler.sync_accounts(vec![Account {
                id: blake3::hash(STORED.as_bytes()).to_hex().to_string(),
                secret: Arc::from(STORED),
                upstream: origin,
                label: "fixture".into(),
                models: vec![ASTRA.into(), SOL.into()],
                provider_ids: vec!["fixture".into()],
                source_max_concurrency: 1,
                model_concurrency: HashMap::from([(ASTRA.into(), 1), (SOL.into(), 1)]),
                last_source_success_ms: None,
                model_source_success_ms: HashMap::new(),
            }]);
        }
        Self {
            app: proxy::router(state.clone()),
            state,
            upstream: upstream_state,
            task,
            _temp: temp,
        }
    }
    async fn send(
        &self,
        method: &str,
        path: &str,
        key: &str,
        bytes: Vec<u8>,
        extra: &[(&str, &str)],
    ) -> Response {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:10813")
            .header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json");
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
        self.app
            .clone()
            .oneshot(builder.body(Body::from(bytes)).unwrap())
            .await
            .unwrap()
    }
    fn calls(&self) -> usize {
        self.upstream.seen.lock().unwrap().len()
    }
    async fn consume(response: Response) -> (StatusCode, Bytes) {
        let status = response.status();
        (
            status,
            to_bytes(response.into_body(), 1024 * 1024).await.unwrap(),
        )
    }
    fn assert_no_pool_health(&self) {
        for account in self.state.scheduler.snapshot(&self.state.settings().policy) {
            assert_eq!(account.in_flight, 0);
            assert_eq!(account.successes, 0);
            assert_eq!(account.failures, 0);
        }
    }
    fn assert_private_records(&self) {
        let text = serde_json::to_string(&self.state.records.records()).unwrap();
        for secret in [CALLER, STORED, "PRIVATE PROMPT"] {
            assert!(!text.contains(secret));
        }
    }
}
fn raw_request() -> Vec<u8> {
    br#"{ "model" : "gpt-6.1-sol", "reasoning":{"effort":"low"}, "service_tier":"priority", "previous_response_id":"resp_external", "input":[{"role":"user","content":"PRIVATE PROMPT"}], "store":false }"#.to_vec()
}

#[tokio::test]
async fn legacy_preserves_bytes_models_headers_and_caller_credential_with_pool_present() {
    let h = Harness::new(vec![Reply::success("resp_legacy")], true).await;
    let original = raw_request();
    let pool_id = blake3::hash(STORED.as_bytes()).to_hex().to_string();
    let r = h
        .send(
            "POST",
            "/v1/responses?fixture=1",
            CALLER,
            original.clone(),
            &[
                ("x-pool-router-key", &pool_id),
                ("x-pool-router-probe", "1"),
                ("x-pool-router-upstream", "http://invalid.test"),
                ("thread-id", "01a166ee-ea86-7d02-b7fa-ab476d5b2388"),
            ],
        )
        .await;
    assert_eq!(Harness::consume(r).await.0, StatusCode::OK);
    let seen = h.upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].bytes.as_ref(), original);
    assert_eq!(seen[0].headers["authorization"], format!("Bearer {CALLER}"));
    assert_eq!(seen[0].uri, "/v1/responses?fixture=1");
    assert!(!seen[0].headers.contains_key("x-pool-router-key"));
    assert!(!seen[0].headers.contains_key("x-pool-router-upstream"));
    drop(seen);
    let records = h.state.records.records();
    assert_eq!(records[0].route_mode, "legacy");
    assert_eq!(records[0].requested_model.as_deref(), Some(SOL));
    assert_eq!(records[0].effective_model.as_deref(), Some(SOL));
    assert_eq!(records[0].effective_effort.as_deref(), Some("low"));
    assert_eq!(
        records[0].requested_service_tier.as_deref(),
        Some("priority")
    );
    assert_eq!(records[0].response_service_tier, None);
    assert!(records[0].has_previous_response_id);
    assert_eq!(records[0].attempts.len(), 1);
    h.assert_no_pool_health();
    h.assert_private_records();
    let selection = Selection {
        model: SOL.into(),
        previous_response_id: Some("resp_legacy".into()),
        ..Default::default()
    };
    assert!(matches!(
        h.state
            .scheduler
            .try_acquire(&selection, &h.state.settings().policy),
        Err(SelectError::UnknownResponse)
    ));
}

#[tokio::test]
async fn legacy_works_without_biwrite_and_preserves_compressed_request_and_compact() {
    let h = Harness::new(
        vec![Reply::json(
            json!({"object":"response.compaction","output":[]}),
        )],
        false,
    )
    .await;
    assert!(h.state.sync_source().await.is_err());
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&raw_request()).unwrap();
    let original = encoder.finish().unwrap();
    let r = h
        .send(
            "POST",
            "/v1/responses/compact",
            CALLER,
            original.clone(),
            &[("content-encoding", "gzip")],
        )
        .await;
    assert_eq!(Harness::consume(r).await.0, StatusCode::OK);
    let seen = h.upstream.seen.lock().unwrap();
    assert_eq!(seen[0].bytes.as_ref(), original);
    assert_eq!(seen[0].headers["content-encoding"], "gzip");
    drop(seen);
    assert_eq!(h.state.records.records()[0].outcome, "completed");
    h.assert_private_records();
}

#[tokio::test]
async fn foreign_keys_cannot_access_management_models_or_disabled_legacy() {
    let h = Harness::new(vec![], true).await;
    for (method, path) in [
        ("GET", "/api/records"),
        ("GET", "/api/state"),
        ("POST", "/api/config"),
        ("POST", "/api/legacy/connect"),
        ("POST", "/api/legacy/restore"),
        ("GET", "/v1/models"),
        ("GET", "/v1/responses"),
    ] {
        assert_eq!(
            h.send(method, path, CALLER, b"{}".to_vec(), &[])
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let local = h.state.settings().local_token;
    assert_eq!(
        h.send("GET", "/v1/models", &local, vec![], &[])
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        h.send(
            "POST",
            "/api/config",
            &local,
            br#"{"legacy_upstream_override":"http://evil.test"}"#.to_vec(),
            &[]
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    h.state.config.write().unwrap().legacy_monitor_enabled = false;
    assert_eq!(
        h.send("POST", "/v1/responses", CALLER, raw_request(), &[])
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(h.calls(), 0);
    h.assert_no_pool_health();
}

#[tokio::test]
async fn legacy_non_sse_errors_and_unknown_bodies_keep_status_and_bytes_without_retry() {
    let raw = b"<html>unknown upstream body</html>".to_vec();
    let denied = br#"{ "error" : { "code":"rate_limit_exceeded", "message":"fixture" } }"#.to_vec();
    let h = Harness::new(
        vec![
            Reply {
                status: StatusCode::OK,
                content_type: "text/html",
                bytes: raw.clone(),
                encoding: None,
                delay_ms: 0,
            },
            Reply {
                status: StatusCode::TOO_MANY_REQUESTS,
                content_type: "application/json",
                bytes: denied.clone(),
                encoding: None,
                delay_ms: 0,
            },
            Reply {
                status: StatusCode::OK,
                content_type: "application/json",
                bytes: denied.clone(),
                encoding: None,
                delay_ms: 0,
            },
        ],
        true,
    )
    .await;
    for (expected_status, expected_bytes) in [
        (StatusCode::OK, raw),
        (StatusCode::TOO_MANY_REQUESTS, denied.clone()),
        (StatusCode::OK, denied),
    ] {
        let r = h
            .send(
                "POST",
                "/v1/responses",
                CALLER,
                raw_request(),
                &[("accept", "text/event-stream")],
            )
            .await;
        let (status, bytes) = Harness::consume(r).await;
        assert_eq!(status, expected_status);
        assert_eq!(bytes.as_ref(), expected_bytes);
    }
    assert_eq!(h.calls(), 3);
    for r in h.state.records.records() {
        assert_eq!(r.route_mode, "legacy");
        assert_ne!(r.outcome, "completed");
        assert_eq!(r.attempts.len(), 1);
    }
    h.assert_no_pool_health();
}

#[tokio::test]
async fn legacy_failure_sse_is_not_retried_and_does_not_consume_pool_health() {
    let failure = json!({"type":"response.failed","response":{"id":"resp_legacy_failed","status":"failed","error":{"code":"rate_limit_exceeded","message":"fixture"}}});
    let original = format!("data: {failure}\n\n");
    let h = Harness::new(vec![Reply::sse(failure)], true).await;
    let r = h
        .send(
            "POST",
            "/v1/responses",
            CALLER,
            raw_request(),
            &[("accept", "text/event-stream")],
        )
        .await;
    let (status, bytes) = Harness::consume(r).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .ends_with(&original)
    );
    assert_eq!(h.calls(), 1);
    assert_eq!(h.state.records.records()[0].outcome, "upstream_failed");
    h.assert_no_pool_health();
}

#[tokio::test]
async fn pool_and_legacy_coexist_and_internal_probe_remains_pool() {
    let completed = json!({"type":"response.completed","response":{"id":"resp_probe","model":ASTRA,"status":"completed"}});
    let h = Harness::new(
        vec![
            Reply::success("resp_external"),
            Reply::success("resp_pool"),
            Reply::sse(completed),
        ],
        true,
    )
    .await;
    let r = h
        .send("POST", "/v1/responses", CALLER, raw_request(), &[])
        .await;
    Harness::consume(r).await;
    let local = h.state.settings().local_token;
    let pool_body =
        json!({"model":SOL,"reasoning":{"effort":"low"},"input":"fixture","store":false})
            .to_string()
            .into_bytes();
    let r = h
        .send("POST", "/v1/responses", &local, pool_body, &[])
        .await;
    assert_eq!(Harness::consume(r).await.0, StatusCode::OK);
    let key_id = blake3::hash(STORED.as_bytes()).to_hex().to_string();
    let r = h
        .send(
            "POST",
            "/api/probe",
            &local,
            json!({"key_id":key_id,"model":ASTRA})
                .to_string()
                .into_bytes(),
            &[],
        )
        .await;
    let (_, body) = Harness::consume(r).await;
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["ok"], true);
    let seen = h.upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].headers["authorization"], format!("Bearer {CALLER}"));
    for row in &seen[1..] {
        assert_eq!(row.headers["authorization"], format!("Bearer {STORED}"));
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&seen[1].bytes).unwrap()["model"],
        ASTRA
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&seen[1].bytes).unwrap()["reasoning"]["effort"],
        "high"
    );
    drop(seen);
    let records = h.state.records.records();
    assert_eq!(
        records.iter().filter(|r| r.route_mode == "legacy").count(),
        1
    );
    assert_eq!(records.iter().filter(|r| r.route_mode == "pool").count(), 2);
    h.assert_private_records();
    assert!(matches!(
        h.state.settings().source_mode,
        SourceMode::Biwrite
    ));
}

#[tokio::test]
async fn legacy_compressed_error_response_keeps_original_bytes_and_encoding() {
    let original = br#"{"error":{"code":"server_error"}}"#;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(original).unwrap();
    let compressed = enc.finish().unwrap();
    let h = Harness::new(
        vec![Reply {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            content_type: "application/json",
            bytes: compressed.clone(),
            encoding: Some("gzip"),
            delay_ms: 0,
        }],
        false,
    )
    .await;
    let r = h
        .send("POST", "/v1/responses", CALLER, raw_request(), &[])
        .await;
    assert_eq!(r.headers()["content-encoding"], "gzip");
    let (status, bytes) = Harness::consume(r).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(bytes.as_ref(), compressed);
    assert_eq!(h.calls(), 1);
    assert_eq!(h.state.records.records()[0].outcome, "upstream_failed");
}

#[tokio::test]
async fn legacy_sse_emits_waiting_comment_without_rewriting_upstream_event() {
    let completed = json!({"type":"response.completed","response":{"id":"resp_waited","status":"completed","model":SOL}});
    let mut reply = Reply::sse(completed);
    let expected = reply.bytes.clone();
    reply.delay_ms = 100;
    let h = Harness::new(vec![reply], false).await;
    let r = h
        .send(
            "POST",
            "/v1/responses",
            CALLER,
            raw_request(),
            &[("accept", "text/event-stream")],
        )
        .await;
    let (_, bytes) = Harness::consume(r).await;
    assert!(bytes.starts_with(b": pool-router legacy waiting\n\n"));
    assert!(bytes.ends_with(&expected));
    assert_eq!(h.state.records.records()[0].outcome, "completed");
}

#[tokio::test]
async fn legacy_upstream_idle_timeout_is_not_reported_as_client_cancellation() {
    let mut reply =
        Reply::sse(json!({"type":"response.completed","response":{"status":"completed"}}));
    reply.delay_ms = 1500;
    let h = Harness::new(vec![reply], true).await;
    let response = h
        .send(
            "POST",
            "/v1/responses",
            CALLER,
            raw_request(),
            &[("accept", "text/event-stream")],
        )
        .await;
    assert!(to_bytes(response.into_body(), 1024 * 1024).await.is_err());
    assert_eq!(h.calls(), 1);
    assert_eq!(h.state.records.records()[0].outcome, "stream_read_failed");
    h.assert_no_pool_health();
}
