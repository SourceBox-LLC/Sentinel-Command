//! The cloud mirror's paging, against a real database and a stand-in
//! Sync service that records every row it is sent.
//!
//! Runs on PostgreSQL when `TEST_DATABASE_URL` is set, and always on the
//! SQLite build.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use sentinel_command::app::AppState;
use sentinel_command::config::Config;

/// A Sync service that accepts every push and remembers the rows.
async fn stand_in_sync() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let record = seen.clone();
    let app = axum::Router::new().route(
        "/v1/sync/push",
        axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let record = record.clone();
            async move {
                let table = body["table"].clone();
                for row in body["rows"].as_array().cloned().unwrap_or_default() {
                    record
                        .lock()
                        .unwrap()
                        .push(serde_json::json!({"table": table, "row": row}));
                }
                axum::Json(serde_json::json!({"accepted": true}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

/// 501 rows sharing one timestamp straddle the 500-row batch. Paged by
/// the timestamp alone, the second batch asked for rows after it and
/// the 501st was never pushed. A row stamped just now waits for a later
/// cycle, so a slow commit cannot be skipped past.
#[tokio::test]
async fn a_tie_across_the_batch_boundary_is_pushed_and_fresh_rows_wait() {
    let Some(pool) = sentinel_command::db::test_pool(4).await else {
        return;
    };
    std::env::set_var("AUTH_PROVIDER", "local");
    std::env::set_var("APP_SECRET_KEY", "x".repeat(32));
    let (url, seen) = stand_in_sync().await;
    let mut config = Config::from_env();
    let org = format!("sync-tie-{}", uuid::Uuid::new_v4().simple());
    config.local_org_id = org.clone();
    config.sentinel_license_key = Some("slk_test".into());
    config.sentinel_sync_service_url = url;
    let http = reqwest::Client::new();
    let state = AppState {
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
    };
    let now = chrono::Utc::now().naive_utc();
    for (key, value) in [
        (sentinel_command::license::LAST_CHECK_REACHABLE, "true"),
        (sentinel_command::license::LICENSE_VALID, "true"),
        (sentinel_command::license::SYNC_ENABLED, "true"),
    ] {
        sqlx::query(
            "INSERT INTO settings (org_id, key, value, updated_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(&org)
        .bind(key)
        .bind(value)
        .bind(now)
        .execute(&state.pool)
        .await
        .unwrap();
    }

    let tied = now - chrono::Duration::minutes(5);
    let mut tx = state.pool.begin().await.unwrap();
    for i in 0..501 {
        sqlx::query(
            "INSERT INTO notifications (org_id, kind, audience, title, body, severity, created_at)
             VALUES ($1, 'motion', 'all', $2, '', 'info', $3)",
        )
        .bind(&org)
        .bind(format!("tied {i}"))
        .bind(tied)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO notifications (org_id, kind, audience, title, body, severity, created_at)
         VALUES ($1, 'motion', 'all', 'fresh', '', 'info', $2)",
    )
    .bind(&org)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    sentinel_command::sync::push_pending_changes(&state).await;

    let titles: HashSet<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r["table"] == "notifications" && r["row"]["data"]["org_id"] == org.as_str())
        .filter_map(|r| r["row"]["data"]["title"].as_str().map(str::to_string))
        .collect();
    let tied_pushed = titles.iter().filter(|t| t.starts_with("tied ")).count();
    assert_eq!(tied_pushed, 501, "every tied row reaches the mirror");
    assert!(
        !titles.contains("fresh"),
        "a row younger than the settle window waits"
    );

    for table in ["notifications", "settings"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1"))
            .bind(&org)
            .execute(&state.pool)
            .await
            .unwrap();
    }
}

/// A 429 from Sync-Service is waited out, not treated as a failed push:
/// the rows still land this cycle. Before, the cursor stalled until the
/// next cycle, thirty minutes later.
#[tokio::test]
async fn a_rate_limited_push_waits_and_lands_in_the_same_cycle() {
    let Some(pool) = sentinel_command::db::test_pool(4).await else {
        return;
    };
    std::env::set_var("AUTH_PROVIDER", "local");
    std::env::set_var("APP_SECRET_KEY", "x".repeat(32));

    // Refuses the very first push with a one-second Retry-After.
    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let refused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (record, first) = (seen.clone(), refused.clone());
    let app = axum::Router::new().route(
        "/v1/sync/push",
        axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let (record, first) = (record.clone(), first.clone());
            async move {
                use axum::response::IntoResponse;
                if !first.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "1")],
                        "slow down",
                    )
                        .into_response();
                }
                for row in body["rows"].as_array().cloned().unwrap_or_default() {
                    record
                        .lock()
                        .unwrap()
                        .push(serde_json::json!({"table": body["table"], "row": row}));
                }
                axum::Json(serde_json::json!({"accepted": true})).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut config = Config::from_env();
    let org = format!("sync-429-{}", uuid::Uuid::new_v4().simple());
    config.local_org_id = org.clone();
    config.sentinel_license_key = Some("slk_test".into());
    config.sentinel_sync_service_url = url;
    let http = reqwest::Client::new();
    let state = AppState {
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
    };
    let now = chrono::Utc::now().naive_utc();
    for (key, value) in [
        (sentinel_command::license::LAST_CHECK_REACHABLE, "true"),
        (sentinel_command::license::LICENSE_VALID, "true"),
        (sentinel_command::license::SYNC_ENABLED, "true"),
    ] {
        sqlx::query("INSERT INTO settings (org_id, key, value, updated_at) VALUES ($1, $2, $3, $4)")
            .bind(&org)
            .bind(key)
            .bind(value)
            .bind(now)
            .execute(&state.pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO notifications (org_id, kind, audience, title, body, severity, created_at)
         VALUES ($1, 'motion', 'all', 'after the wait', '', 'info', $2)",
    )
    .bind(&org)
    .bind(now - chrono::Duration::minutes(5))
    .execute(&state.pool)
    .await
    .unwrap();

    let started = Instant::now();
    sentinel_command::sync::push_pending_changes(&state).await;

    assert!(refused.load(std::sync::atomic::Ordering::SeqCst), "the stand-in refused once");
    assert!(started.elapsed() >= std::time::Duration::from_secs(1), "the Retry-After was honoured");
    let landed = seen.lock().unwrap().iter().any(|r| {
        r["table"] == "notifications"
            && r["row"]["data"]["org_id"] == org.as_str()
            && r["row"]["data"]["title"] == "after the wait"
    });
    assert!(landed, "the row reached the mirror in the same cycle");

    for table in ["notifications", "settings"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1"))
            .bind(&org)
            .execute(&state.pool)
            .await
            .unwrap();
    }
}
