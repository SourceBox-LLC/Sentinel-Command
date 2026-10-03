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
