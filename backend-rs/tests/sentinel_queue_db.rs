//! The agent's work queue, against a real database.
//!
//! Runs on PostgreSQL when `TEST_DATABASE_URL` is set, and always on the
//! SQLite build.

use std::sync::Arc;
use std::time::Instant;

use sentinel_command::app::AppState;
use sentinel_command::config::Config;
use tower::ServiceExt;

const AGENT_KEY: &str = "queue-test-agent-key";

async fn state() -> Option<AppState> {
    let pool = sentinel_command::db::test_pool(2).await?;
    std::env::set_var("AUTH_PROVIDER", "local");
    std::env::set_var("APP_SECRET_KEY", "x".repeat(32));
    std::env::set_var("SENTINEL_AGENT_KEY", AGENT_KEY);
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

/// Every timestamp the agent sees is naive UTC. Without the org's local
/// time it judged an afternoon visitor in California as a 9 p.m. one
/// ("bright daylight despite a 21:09 trigger"), and the reverse is worse:
/// an 8 p.m. caller reads as 3 a.m.
#[tokio::test]
async fn a_pending_run_carries_the_orgs_local_time() {
    let Some(state) = state().await else { return };
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let (la_org, unset_org) = (format!("tzla-{suffix}"), format!("tzutc-{suffix}"));
    sqlx::query(r#"INSERT INTO settings (org_id, "key", value) VALUES ($1, 'timezone', $2)"#)
        .bind(&la_org)
        .bind("America/Los_Angeles")
        .execute(&state.pool)
        .await
        .unwrap();
    let at = chrono::NaiveDate::from_ymd_opt(2026, 10, 12)
        .unwrap()
        .and_hms_opt(21, 9, 53)
        .unwrap();
    for org in [&la_org, &unset_org] {
        sqlx::query(
            "INSERT INTO sentinel_runs (id, org_id, triggered_at, trigger_type, camera_id,
                                        tool_call_count, outcome)
             VALUES ($1, $2, $3, 'motion', 'cam', 0, 'pending')",
        )
        .bind(&org[..32.min(org.len())])
        .bind(org)
        .bind(at)
        .execute(&state.pool)
        .await
        .unwrap();
    }

    let request = axum::http::Request::builder()
        .uri("/api/sentinel/runs/pending?limit=100")
        .header("x-sentinel-agent-key", AGENT_KEY)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = sentinel_command::app::build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let runs: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let local_time = |org: &str| {
        runs["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["org_id"] == org)
            .unwrap_or_else(|| panic!("{org} not in {runs}"))["local_time"]
            .clone()
    };
    assert_eq!(
        local_time(&la_org),
        "Monday 2026-10-12 14:09 PDT (America/Los_Angeles)"
    );
    assert!(local_time(&unset_org)
        .as_str()
        .unwrap()
        .starts_with("Monday 2026-10-12 21:09 UTC (the organization has not set a time zone"));

    for org in [&la_org, &unset_org] {
        sqlx::query("DELETE FROM sentinel_runs WHERE org_id = $1")
            .bind(org)
            .execute(&state.pool)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM settings WHERE org_id = $1")
        .bind(&la_org)
        .execute(&state.pool)
        .await
        .unwrap();
}
