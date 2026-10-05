//! What a wrong node key can write, against a real database.
//!
//! Runs on PostgreSQL when `TEST_DATABASE_URL` is set, and always on the
//! SQLite build.

use std::sync::Arc;
use std::time::Instant;

use sentinel_command::app::AppState;
use sentinel_command::config::Config;
use tower::ServiceExt;

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

/// A wrong key proves nothing about who sent it. Anyone who knows a node
/// id — it prefixes every camera id — could put "Registration failing:
/// rotate the key" on another org's working node, beside a Rotate
/// button that would break it. The note is still written for a node
/// that has never connected, which is the installer-got-the-wrong-key
/// case it exists for.
#[tokio::test]
async fn a_wrong_key_annotates_only_a_node_that_has_never_connected() {
    let Some(state) = state().await else { return };
    let org = format!("badkey-{}", uuid::Uuid::new_v4().simple());
    let now = chrono::Utc::now().naive_utc();
    let mut ids = Vec::new();
    for (suffix, last_seen) in [("new", None), ("live", Some(now))] {
        let node_id = format!("{}{suffix}", &org[7..15]);
        sqlx::query(
            "INSERT INTO camera_nodes (node_id, org_id, api_key_hash, name, status, last_seen,
                                       created_at, updated_at)
             VALUES ($1, $2, 'not-the-hash', 'Shed', $3, $4, $5, $5)",
        )
        .bind(&node_id)
        .bind(&org)
        .bind(if last_seen.is_some() { "online" } else { "pending" })
        .bind(last_seen)
        .bind(now)
        .execute(&state.pool)
        .await
        .unwrap();
        ids.push(node_id);
    }

    for path in ["/api/nodes/register", "/api/nodes/validate"] {
        for node_id in &ids {
            let body = serde_json::json!({ "node_id": node_id, "name": "Shed" }).to_string();
            let mut request = axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .header("x-node-api-key", "a-key-that-is-wrong")
                .body(axum::body::Body::from(body))
                .unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                std::net::SocketAddr::from(([127, 0, 0, 1], 9)),
            ));
            let response = sentinel_command::app::build_router(state.clone())
                .oneshot(request)
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN, "{path} {node_id}");
        }
        let notes: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT node_id, last_register_error FROM camera_nodes WHERE org_id = $1
              ORDER BY node_id",
        )
        .bind(&org)
        .fetch_all(&state.pool)
        .await
        .unwrap();
        for (node_id, note) in notes {
            if node_id.ends_with("new") {
                assert!(note.is_some(), "{path}: the never-connected node gets the note");
            } else {
                assert_eq!(note, None, "{path}: a working node is not told to rotate its key");
            }
        }
    }

    sqlx::query("DELETE FROM camera_nodes WHERE org_id = $1")
        .bind(&org)
        .execute(&state.pool)
        .await
        .unwrap();
}
