use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use pool_router::{
    config::{Config, SourceMode},
    proxy::{self, AppState},
};
use serde_json::{Value, json};
use tempfile::tempdir;
use tower::ServiceExt;

fn get(token: Option<&str>, uri: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(uri)
        .header("host", "127.0.0.1:10813");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

fn post(token: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", "127.0.0.1:10813")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_of(response: axum::response::Response) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn usage_requires_local_auth_and_reports_an_empty_ledger_with_default_prices() {
    let directory = tempdir().unwrap();
    let config = Config {
        source_mode: SourceMode::Manual,
        ..Config::default()
    };
    let token = config.local_token.clone();
    let state = AppState::new(config, directory.path().join("config.json"), false).unwrap();
    let app = proxy::router(state);

    let denied = app.clone().oneshot(get(None, "/api/usage")).await.unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

    let response = app
        .clone()
        .oneshot(get(Some(&token), "/api/usage?days=7"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_of(response).await;
    assert_eq!(value["today"]["requests"], json!(0));
    assert_eq!(value["today"]["cost_usd"], json!(0.0));
    assert_eq!(value["range"]["requests"], json!(0));
    assert_eq!(value["long_context_threshold"], json!(272_000));
    // Defaults ship with prices for the models this build routes.
    assert!(value["pricing"]["models"]["gpt-6-astra"]["short"]["input"].is_number());
    assert_eq!(value["pricing"]["rate_multiplier"], json!(1.0));
    assert!(!value.to_string().contains(&token));
}

#[tokio::test]
async fn pricing_updates_persist_and_invalid_tables_leave_the_previous_one() {
    let directory = tempdir().unwrap();
    let config = Config {
        source_mode: SourceMode::Manual,
        ..Config::default()
    };
    let token = config.local_token.clone();
    let path = directory.path().join("config.json");
    let state = AppState::new(config.clone(), path.clone(), false).unwrap();
    let app = proxy::router(state);

    let denied = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/usage/pricing")
                .header("host", "127.0.0.1:10813")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

    let current = body_of(
        app.clone()
            .oneshot(get(Some(&token), "/api/usage"))
            .await
            .unwrap(),
    )
    .await;
    let mut pricing = current["pricing"].clone();
    pricing["rate_multiplier"] = json!(1.4);
    pricing["usd_to_display"] = json!(7.1);
    pricing["display_currency"] = json!("CNY");
    let accepted = app
        .clone()
        .oneshot(post(&token, "/api/usage/pricing", pricing.clone()))
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    assert_eq!(
        body_of(accepted).await["pricing"]["rate_multiplier"],
        json!(1.4)
    );
    assert!(directory.path().join("pricing.json").exists());

    for bad in [
        json!({"rate_multiplier": -1.0}),
        json!({"unknown_field": 1}),
        json!("not an object"),
    ] {
        let mut invalid = pricing.clone();
        if let Some(object) = bad.as_object() {
            for (key, value) in object {
                invalid[key] = value.clone();
            }
        } else {
            invalid = bad;
        }
        let response = app
            .clone()
            .oneshot(post(&token, "/api/usage/pricing", invalid))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_of(response).await["error"]["code"], "invalid_pricing");
    }

    // A fresh service reads the accepted table, not any rejected one.
    let reopened = AppState::new(config, path, false).unwrap();
    let app = proxy::router(reopened);
    let value = body_of(app.oneshot(get(Some(&token), "/api/usage")).await.unwrap()).await;
    assert_eq!(value["pricing"]["rate_multiplier"], json!(1.4));
    assert_eq!(value["pricing"]["usd_to_display"], json!(7.1));
}

#[tokio::test]
async fn the_config_preview_masks_credentials_and_needs_local_auth() {
    let directory = tempdir().unwrap();
    let config = Config {
        source_mode: SourceMode::Manual,
        ..Config::default()
    };
    let token = config.local_token.clone();
    let path = directory.path().join("config.json");
    let state = AppState::new(config, path.clone(), false).unwrap();
    // Saving the configuration writes the local token into the file.
    std::fs::write(
        &path,
        serde_json::json!({"local_token": token, "listen": "127.0.0.1:10813"}).to_string(),
    )
    .unwrap();
    let app = proxy::router(state);

    let denied = app
        .clone()
        .oneshot(get(None, "/api/config/files"))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

    let response = app
        .oneshot(get(Some(&token), "/api/config/files"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_of(response).await;
    let files = value["files"].as_array().expect("files");
    assert_eq!(files.len(), 2);
    assert!(
        files[0]["path"]
            .as_str()
            .expect("path")
            .ends_with("config.json")
    );
    // The preview must never carry a credential, not even this session's token.
    let body = value.to_string();
    assert!(!body.contains(&token), "preview leaked the local token");
    assert!(body.contains("已隐去"));
}

#[test]
fn masking_covers_credential_keys_and_leaves_the_rest_readable() {
    use pool_router::integration::mask_secret_values;
    let raw = concat!(
        "# comment with token = keep\n",
        "model = \"gpt-6.1-sol\"\n",
        "stream_max_retries = 15\n",
        "experimental_bearer_token = \"sk-should-not-appear\"\n",
        "  api_key = \"secret-value\"\n",
        "\"authorization\" = \"Bearer nope\"\n",
        "base_url = \"http://127.0.0.1:10813/v1\"\n",
    );
    let masked = mask_secret_values(raw);
    assert!(!masked.contains("sk-should-not-appear"));
    assert!(!masked.contains("secret-value"));
    assert!(!masked.contains("Bearer nope"));
    assert!(masked.contains("model = \"gpt-6.1-sol\""));
    assert!(masked.contains("stream_max_retries = 15"));
    assert!(masked.contains("base_url = \"http://127.0.0.1:10813/v1\""));
    // A comment is not a key/value pair and stays as written.
    assert!(masked.contains("# comment with token = keep"));
}

#[tokio::test]
async fn disconnecting_needs_local_auth_and_refuses_when_there_is_nothing_to_undo() {
    // The panel's disconnect button must never guess at a Codex configuration it
    // did not write, so without a stored backup it has to refuse rather than edit.
    let directory = tempdir().unwrap();
    let config = Config {
        source_mode: SourceMode::Manual,
        ..Config::default()
    };
    let token = config.local_token.clone();
    let state = AppState::new(config, directory.path().join("config.json"), false).unwrap();
    let app = proxy::router(state);

    let denied = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/restore")
                .header("host", "127.0.0.1:10813")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

    let response = app
        .clone()
        .oneshot(post(&token, "/api/restore", json!({})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let value = body_of(response).await;
    assert_eq!(value["error"]["code"], "restore_failed");
    assert!(!value.to_string().contains(&token));
}
