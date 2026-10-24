use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use pool_router::{
    config::{Config, SourceMode},
    proxy::{self, AppState},
};
use serde_json::Value;
use tempfile::tempdir;
use tower::ServiceExt;

#[tokio::test]
async fn update_management_requires_local_auth_and_rejects_downgrades_without_network() {
    let directory = tempdir().unwrap();
    let config = Config {
        source_mode: SourceMode::Manual,
        ..Config::default()
    };
    let token = config.local_token.clone();
    let state = AppState::new(config, directory.path().join("config.json"), false).unwrap();
    let app = proxy::router(state);
    for endpoint in ["/api/update/check", "/api/update/download"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(endpoint)
                    .header("host", "127.0.0.1:10813")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    for version in [
        env!("CARGO_PKG_VERSION"),
        "0.1.0",
        "../other",
        "https://example.com/update.exe",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/update/download")
                    .header("host", "127.0.0.1:10813")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"version":version}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let value: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(value["error"]["code"], "update_download_failed");
        assert!(!value.to_string().contains(&token));
    }
    assert!(!directory.path().join("updates").exists());
}
