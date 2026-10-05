//! The MCP scope gate, against a real database.
//!
//! PYTHON_BUGS #13: a scoped per-org agent key (`osa_`, the credential
//! handed to customers who run the agent themselves) was not recognised
//! by the scope lookup, so it got no filter at all — and the next pass
//! authenticated it, so it could call `set_camera_recording_policy`, the
//! one tool the agent allowlist exists to withhold. The differential
//! harnesses now compare two Rust builds, which agree with each other
//! whether or not the hole is open; this is what asserts it is closed.
//!
//! Runs on PostgreSQL when `TEST_DATABASE_URL` is set, and always on the
//! SQLite build.

use std::sync::Arc;
use std::time::Instant;

use axum::http::HeaderMap;
use sentinel_command::app::AppState;
use sentinel_command::config::Config;
use sentinel_command::mcp::{auth, scope};

async fn state() -> Option<AppState> {
    let pool = sentinel_command::db::test_pool(2).await?;
    std::env::set_var("AUTH_PROVIDER", "local");
    std::env::set_var("APP_SECRET_KEY", "x".repeat(32));
    let config = Config::from_env();
    let http = reqwest::Client::new();
    Some(AppState {
        auth: Arc::new(sentinel_command::auth::Authenticator::from_config(
            &config,
            http.clone(),
        )),
        cors: sentinel_command::cors::CorsConfig::from_env(&config.frontend_url, ""),
        hls: Arc::new(sentinel_command::hls::HlsCache::new()),
        limiter: Arc::new(sentinel_command::ratelimit::Limiter::from_env("").await),
        http,
        config: Arc::new(config),
        pool,
        started_at: Instant::now(),
        started_at_wall: chrono::Utc::now(),
    })
}

fn bearer(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    headers
}

fn sha256_hex(raw: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(raw.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn a_scoped_agent_key_gets_the_agent_allowlist_and_not_the_recording_tool() {
    let Some(state) = state().await else { return };
    // Unique per run: on PostgreSQL this shares a database with others.
    let token = format!("osa_scope_test_{}", uuid::Uuid::new_v4().simple());
    let hash = sha256_hex(&token);
    sqlx::query(
        "INSERT INTO sentinel_agent_keys (org_id, key_hash, key_last4, name, created_at, revoked)
         VALUES ('scope-test-org', $1, 'test', 'scope test', $2, false)",
    )
    .bind(&hash)
    .bind(chrono::Utc::now().naive_utc())
    .execute(&state.pool)
    .await
    .unwrap();

    let allowed = auth::lookup_allowed(&state, &bearer(&token))
        .await
        .expect("a scoped agent key must be recognised by the scope gate");
    assert_eq!(allowed, scope::agent_allowed_tools());
    assert!(
        !allowed.contains("set_camera_recording_policy"),
        "the agent must not be able to switch a camera's recording off"
    );

    // Revoked: no longer the agent's allowlist. It falls through to the
    // org pass, which refuses it.
    sqlx::query("UPDATE sentinel_agent_keys SET revoked = true WHERE key_hash = $1")
        .bind(&hash)
        .execute(&state.pool)
        .await
        .unwrap();
    assert_eq!(auth::lookup_allowed(&state, &bearer(&token)).await, None);

    sqlx::query("DELETE FROM sentinel_agent_keys WHERE key_hash = $1")
        .bind(&hash)
        .execute(&state.pool)
        .await
        .unwrap();
}

async fn create_key(state: &AppState, body: &str) -> (axum::http::StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let token =
        sentinel_command::auth::local::issue_token(&"x".repeat(32), &state.config.local_org_id)
            .unwrap();
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/mcp/keys")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            9,
        ))));
    let response = sentinel_command::app::build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// A field the key-creation body does not declare is a 422, not ignored.
/// Ignored, `{"scopeMode": "readonly"}` — the camelCase AGENTS.md once
/// documented — minted a key with every tool, write tools included: the
/// one field that narrows a key was the one silently dropped.
#[tokio::test]
async fn key_creation_refuses_an_undeclared_field_and_honours_the_real_one() {
    let Some(state) = state().await else { return };

    let (status, body) = create_key(&state, r#"{"name":"Agent","scopeMode":"readonly"}"#).await;
    assert_eq!(
        status,
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "{body}"
    );
    assert_eq!(
        body["detail"]["errors"][0]["type"], "extra_forbidden",
        "{body}"
    );
    assert_eq!(
        body["detail"]["errors"][0]["loc"],
        serde_json::json!(["body", "scopeMode"])
    );

    let (status, body) = create_key(&state, r#"{"name":"Agent","scope_mode":"readonly"}"#).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["scope_mode"], "readonly", "{body}");
    let raw = body["key"]
        .as_str()
        .expect("the plaintext key, shown once")
        .to_string();
    let allowed = auth::lookup_allowed(&state, &bearer(&raw))
        .await
        .expect("a fresh key is recognised");
    assert!(allowed.contains("list_cameras"));
    assert!(
        !allowed.contains("create_incident"),
        "read-only must not write"
    );

    sqlx::query("DELETE FROM mcp_api_keys WHERE key_hash = $1")
        .bind(sha256_hex(&raw))
        .execute(&state.pool)
        .await
        .unwrap();
}
