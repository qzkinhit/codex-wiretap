//! Local-only metadata enrichment; no upstream requests or real Codex data.
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use pool_router::{
    config::{Config, SourceMode},
    conversations::ConversationIndex,
    monitor::RequestRecord,
    proxy::{self, AppState},
};
use serde_json::Value;
use std::{fs, sync::Arc};
use tempfile::tempdir;
use tower::ServiceExt;

const THREAD: &str = "11111111-1111-4111-8111-111111111111";
const UNKNOWN: &str = "22222222-2222-4222-8222-222222222222";

#[tokio::test]
async fn records_add_current_local_names_without_persisting_them() {
    let directory = tempdir().unwrap();
    let codex = directory.path().join("codex");
    fs::create_dir(&codex).unwrap();
    let database_path = codex.join("state_5.sqlite");
    let database = rusqlite::Connection::open(&database_path).unwrap();
    database.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT, name TEXT, first_user_message TEXT)").unwrap();
    database
        .execute(
            "INSERT INTO threads VALUES (?1, ?2, ?3, ?4)",
            [
                THREAD,
                "原始标题",
                "对话名称 <script>text only</script>",
                "PRIVATE_BODY_MUST_NOT_APPEAR",
            ],
        )
        .unwrap();
    drop(database);
    let database_before = fs::read(&database_path).unwrap();
    let legacy_record = RequestRecord {
        id: "fixture-old-record".into(),
        conversation_id: Some(THREAD.into()),
        outcome: "completed".into(),
        ..RequestRecord::default()
    };
    let log = directory.path().join("records.jsonl");
    fs::write(
        &log,
        format!("{}\n", serde_json::to_string(&legacy_record).unwrap()),
    )
    .unwrap();
    let config = Config {
        source_mode: SourceMode::Manual,
        ..Config::default()
    };
    let token = config.local_token.clone();
    let mut state = AppState::new(config, directory.path().join("config.json"), true).unwrap();
    Arc::get_mut(&mut state).unwrap().conversations = Arc::new(ConversationIndex::new(codex));
    state
        .records
        .record(RequestRecord {
            id: "fixture-unmatched-record".into(),
            conversation_id: Some(UNKNOWN.into()),
            ..RequestRecord::default()
        })
        .unwrap();
    let log_before = fs::read(&log).unwrap();
    let app = proxy::router(state.clone());
    for authorization in [None, Some("Bearer sk-not-a-management-credential")] {
        let mut request = Request::builder()
            .uri("/api/records")
            .header("host", "127.0.0.1:10813");
        if let Some(value) = authorization {
            request = request.header("authorization", value);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/records")
                .header("host", "127.0.0.1:10813")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let rows = body["records"].as_array().unwrap();
    let matched = rows
        .iter()
        .find(|row| row["conversation_id"] == THREAD)
        .unwrap();
    assert_eq!(
        matched["conversation_title"],
        "对话名称 <script>text only</script>"
    );
    assert_eq!(matched["id"], "fixture-old-record");
    assert!(
        rows.iter()
            .find(|row| row["conversation_id"] == UNKNOWN)
            .unwrap()["conversation_title"]
            .is_null()
    );
    assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE_BODY_MUST_NOT_APPEAR"));
    assert_eq!(fs::read(&database_path).unwrap(), database_before);
    assert_eq!(fs::read(&log).unwrap(), log_before);
    assert!(
        !serde_json::to_string(&state.records.records())
            .unwrap()
            .contains("conversation_title")
    );
}

#[tokio::test]
async fn absent_local_index_preserves_records_and_conversation_ids() {
    let directory = tempdir().unwrap();
    let config = Config {
        source_mode: SourceMode::Manual,
        ..Config::default()
    };
    let token = config.local_token.clone();
    let mut state = AppState::new(config, directory.path().join("config.json"), false).unwrap();
    Arc::get_mut(&mut state).unwrap().conversations =
        Arc::new(ConversationIndex::new(directory.path().join("missing")));
    state
        .records
        .record(RequestRecord {
            conversation_id: Some(THREAD.into()),
            ..RequestRecord::default()
        })
        .unwrap();
    let response = proxy::router(state)
        .oneshot(
            Request::builder()
                .uri("/api/records")
                .header("host", "127.0.0.1:10813")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(body["records"][0]["conversation_id"], THREAD);
    assert!(body["records"][0]["conversation_title"].is_null());
}
