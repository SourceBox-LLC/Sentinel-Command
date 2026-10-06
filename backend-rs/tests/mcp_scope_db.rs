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

/// `GET /api/sentinel/runs` reports the cap the gate allows, as
/// `GET /config` does: an unlicensed self-hosted install has 0 runs, not
/// the 500 its plan nominally carries.
#[tokio::test]
async fn an_unlicensed_install_has_no_monthly_runs_on_either_endpoint() {
    use tower::ServiceExt;
    let Some(state) = state().await else { return };
    let token =
        sentinel_command::auth::local::issue_token(&"x".repeat(32), &state.config.local_org_id)
            .unwrap();
    for path in ["/api/sentinel/config", "/api/sentinel/runs"] {
        let mut request = axum::http::Request::builder()
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
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
        assert_eq!(response.status(), axum::http::StatusCode::OK, "{path}");
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let cap = if path.ends_with("config") {
            &body["monthly_cap"]
        } else {
            &body["stats"]["monthly_cap"]
        };
        assert_eq!(cap, 0, "{path}: {body}");
    }
}

/// A wrong-typed field in an incident PATCH is a 422 naming it. It used
/// to empty the whole patch and answer 200 having changed nothing, so
/// `{"severity": 5, "status": "resolved"}` resolved nothing.
#[tokio::test]
async fn an_incident_patch_with_a_wrong_typed_field_is_refused_not_ignored() {
    use tower::ServiceExt;
    let Some(state) = state().await else { return };
    let org = state.config.local_org_id.clone();
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO incidents (org_id, title, summary, report, severity, status, created_by, created_at, updated_at)
         VALUES ($1, 'patch test', 's', '', 'low', 'open', 'test', $2, $2) RETURNING id",
    )
    .bind(&org)
    .bind(chrono::Utc::now().naive_utc())
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let token = sentinel_command::auth::local::issue_token(&"x".repeat(32), &org).unwrap();
    let patch =
        |body: &'static str| {
            let mut request = axum::http::Request::builder()
                .method("PATCH")
                .uri(format!("/api/incidents/{id}"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                std::net::SocketAddr::from(([127, 0, 0, 1], 9)),
            ));
            sentinel_command::app::build_router(state.clone()).oneshot(request)
        };

    let response = patch(r#"{"severity": 5, "status": "resolved"}"#)
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["detail"]["errors"][0]["loc"],
        serde_json::json!(["body", "severity"]),
        "{body}"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM incidents WHERE id = $1")
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(status, "open", "nothing applied from a refused patch");

    let response = patch(r#"{"status": "resolved"}"#).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    sqlx::query("DELETE FROM incidents WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await
        .unwrap();
}

/// Ten agents claiming one run at once: exactly one is told it won.
/// The claim used to read `pending` and then write, so all ten read
/// `pending` and all ten answered `claimed: true` — overlapping drains
/// each ran the agent on the same run.
#[tokio::test]
async fn concurrent_claims_on_one_run_have_exactly_one_winner() {
    use tower::ServiceExt;
    let Some(base) = state().await else { return };
    let mut config = (*base.config).clone();
    config.sentinel_agent_key = Some("claim-race-secret".into());
    let state = AppState {
        config: std::sync::Arc::new(config),
        ..base
    };
    let run_id = format!(
        "claimrace{}",
        &uuid::Uuid::new_v4().simple().to_string()[..20]
    );
    sqlx::query(
        "INSERT INTO sentinel_runs (id, org_id, triggered_at, trigger_type, tool_call_count, outcome)
         VALUES ($1, 'claim-race-org', $2, 'manual', 0, 'pending')",
    )
    .bind(&run_id)
    .bind(chrono::Utc::now().naive_utc())
    .execute(&state.pool)
    .await
    .unwrap();

    let claims = (0..10).map(|_| {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/sentinel/runs/{run_id}/start"))
            .header("x-sentinel-agent-key", "claim-race-secret")
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();
        let router = sentinel_command::app::build_router(state.clone());
        async move {
            let response = router.oneshot(request).await.unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            body["claimed"] == serde_json::Value::Bool(true)
        }
    });
    let winners = futures_util::future::join_all(claims)
        .await
        .into_iter()
        .filter(|won| *won)
        .count();
    assert_eq!(winners, 1);

    sqlx::query("DELETE FROM sentinel_runs WHERE id = $1")
        .bind(&run_id)
        .execute(&state.pool)
        .await
        .unwrap();
}

/// Five creates at once with one slot left under the plan's node cap:
/// one succeeds. Counted outside a lock, all five saw room.
#[tokio::test]
async fn concurrent_node_creates_cannot_overshoot_the_plan_cap() {
    use tower::ServiceExt;
    let Some(state) = state().await else { return };
    let org = "node-cap-race".to_string();
    let cap = sentinel_command::plans::get_plan_limits("self_host").max_nodes;
    sqlx::query("DELETE FROM camera_nodes WHERE org_id = $1")
        .bind(&org)
        .execute(&state.pool)
        .await
        .unwrap();
    let mut tx = state.pool.begin().await.unwrap();
    for i in 0..cap - 1 {
        sqlx::query(
            "INSERT INTO camera_nodes (node_id, org_id, api_key_hash, name, status, created_at, updated_at)
             VALUES ($1, $2, $1, $1, 'pending', $3, $3)",
        )
        .bind(format!("ncr{i:05}"))
        .bind(&org)
        .bind(chrono::Utc::now().naive_utc())
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    let token = sentinel_command::auth::local::issue_token(&"x".repeat(32), &org).unwrap();
    let creates =
        (0..5).map(|i| {
            let mut request = axum::http::Request::builder()
                .method("POST")
                .uri("/api/nodes")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(format!(r#"{{"name":"race {i}"}}"#)))
                .unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                std::net::SocketAddr::from(([127, 0, 0, 1], 9)),
            ));
            let router = sentinel_command::app::build_router(state.clone());
            async move { router.oneshot(request).await.unwrap().status() }
        });
    let statuses = futures_util::future::join_all(creates).await;
    let created = statuses.iter().filter(|s| s.is_success()).count();
    assert_eq!(created, 1, "{statuses:?}");
    let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM camera_nodes WHERE org_id = $1")
        .bind(&org)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(total, cap);

    sqlx::query("DELETE FROM camera_nodes WHERE org_id = $1")
        .bind(&org)
        .execute(&state.pool)
        .await
        .unwrap();
}

/// A recording window that starts and ends at the same minute never
/// records, so neither the REST route nor the MCP tool will store one.
#[tokio::test]
async fn an_empty_recording_window_is_refused_on_both_paths() {
    use tower::ServiceExt;
    let Some(state) = state().await else { return };
    let org = state.config.local_org_id.clone();
    let camera = format!("emptywin-{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO cameras (camera_id, org_id, name, disabled_by_plan, continuous_24_7,
                              scheduled_recording, created_at, updated_at)
         VALUES ($1, $2, 'Gate', false, false, false, $3, $3)",
    )
    .bind(&camera)
    .bind(&org)
    .bind(chrono::Utc::now().naive_utc())
    .execute(&state.pool)
    .await
    .unwrap();
    let token = sentinel_command::auth::local::issue_token(&"x".repeat(32), &org).unwrap();
    let patch =
        |body: &'static str| {
            let mut request = axum::http::Request::builder()
                .method("PATCH")
                .uri(format!("/api/cameras/{camera}/recording-settings"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                std::net::SocketAddr::from(([127, 0, 0, 1], 9)),
            ));
            sentinel_command::app::build_router(state.clone()).oneshot(request)
        };
    let status =
        patch(r#"{"scheduled_recording":true,"scheduled_start":"08:00","scheduled_end":"08:00"}"#)
            .await
            .unwrap()
            .status();
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);

    let mut args = serde_json::Map::new();
    args.insert("camera_id".into(), serde_json::json!(camera));
    args.insert("scheduled_recording".into(), serde_json::json!(true));
    args.insert("scheduled_start".into(), serde_json::json!("22:00"));
    args.insert("scheduled_end".into(), serde_json::json!("22:00"));
    let refused =
        sentinel_command::mcp::tools::set_camera_recording_policy(&state, &org, &args).await;
    assert!(refused.is_err(), "{refused:?}");

    // A real window still saves, through either path.
    let status =
        patch(r#"{"scheduled_recording":true,"scheduled_start":"22:00","scheduled_end":"06:00"}"#)
            .await
            .unwrap()
            .status();
    assert_eq!(status, axum::http::StatusCode::OK);

    sqlx::query("DELETE FROM cameras WHERE camera_id = $1")
        .bind(&camera)
        .execute(&state.pool)
        .await
        .unwrap();
}

/// `tools/list` is refused to a key nothing recognises — no key, a
/// typo, a revoked one — rather than answered with the whole catalog.
/// A client holding a dead key used to report 23 tools and fail only on
/// first use; it now fails at connect, with the message a call gets.
#[tokio::test]
async fn tools_list_refuses_an_unrecognised_or_revoked_key() {
    use tower::ServiceExt;
    let Some(state) = state().await else { return };
    let token = format!("osa_list_test_{}", uuid::Uuid::new_v4().simple());
    let hash = sha256_hex(&token);
    sqlx::query(
        "INSERT INTO sentinel_agent_keys (org_id, key_hash, key_last4, name, created_at, revoked)
         VALUES ('list-test-org', $1, 'test', 'list test', $2, false)",
    )
    .bind(&hash)
    .bind(chrono::Utc::now().naive_utc())
    .execute(&state.pool)
    .await
    .unwrap();

    let list =
        |auth: Option<String>| {
            let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
            let mut request = axum::http::Request::builder()
                .method("POST")
                .uri("/mcp/")
                .header("host", "sentinel-command.com")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("content-length", body.len().to_string());
            if let Some(auth) = auth {
                request = request.header("authorization", format!("Bearer {auth}"));
            }
            let mut request = request.body(axum::body::Body::from(body)).unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                std::net::SocketAddr::from(([127, 0, 0, 1], 9)),
            ));
            let app = sentinel_command::app::build_router(state.clone());
            async move {
                let response = app.oneshot(request).await.unwrap();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                serde_json::from_slice::<serde_json::Value>(&bytes)
                    .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&bytes)))
            }
        };

    let listed = list(Some(token.clone())).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("a live key is listed its tools: {listed}"))
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(names.len(), scope::agent_allowed_tools().len(), "{names:?}");

    sqlx::query("UPDATE sentinel_agent_keys SET revoked = true WHERE key_hash = $1")
        .bind(&hash)
        .execute(&state.pool)
        .await
        .unwrap();
    for auth in [Some(token.clone()), Some("not-a-key".to_string()), None] {
        let refused = list(auth.clone()).await;
        assert!(
            refused.get("result").is_none(),
            "{auth:?} was listed: {refused}"
        );
        let message = refused["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("Unauthorized") || message.contains("Authorization"),
            "{auth:?}: {refused}"
        );
    }

    sqlx::query("DELETE FROM sentinel_agent_keys WHERE key_hash = $1")
        .bind(&hash)
        .execute(&state.pool)
        .await
        .unwrap();
}
