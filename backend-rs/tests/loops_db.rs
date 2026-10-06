//! The background loops' bodies, against a real database.
//!
//! These were held to the Python by `loops_run.sh` (7/7), which needs the
//! deleted Python to run — so after the cut nothing in CI exercised the
//! offline sweep, log retention, the stranded-run reaper or the motion
//! digest at all. Each runs on a timer and returns nothing anyone reads,
//! which is exactly the code that breaks silently.
//!
//! Every test scopes its rows by a fresh org id and asserts on those rows
//! only: on PostgreSQL this shares a database with the other suites, and
//! the loops are cross-org by design, so a global count is not a stable
//! thing to assert. Clerk is pointed at a closed port, so a plan lookup
//! fails fast and locally instead of reaching the network.
//!
//! Runs on PostgreSQL when `TEST_DATABASE_URL` is set, and always on the
//! SQLite build.

use std::sync::Arc;
use std::time::Instant;

use chrono::{Duration, NaiveDateTime};
use sentinel_command::app::AppState;
use sentinel_command::config::Config;
use sentinel_command::loops;

/// Held for the whole of every test. The loop bodies are cross-org by
/// design, so two tests running at once see each other's half-built
/// fixtures: one test's digest pass closed another's anchor before its
/// motion events were inserted, and a cleanup pass could sweep an org's
/// logs at the free tier's 30 days before its Pro setting landed.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn state(tweak: impl FnOnce(&mut Config)) -> Option<AppState> {
    let pool = sentinel_command::db::test_pool(2).await?;
    // Every test in this binary sets the same values, so the reads race
    // harmlessly; the fields that matter are overridden below.
    std::env::set_var("AUTH_PROVIDER", "local");
    std::env::set_var("APP_SECRET_KEY", "x".repeat(32));
    let mut config = Config::from_env();
    let http = reqwest::Client::new();
    let auth = Arc::new(sentinel_command::auth::Authenticator::from_config(
        &config,
        http.clone(),
    ));
    // Hosted mode, so plans are tiered — under local auth every org is
    // `self_host` and retention would not vary.
    config.auth_provider = "clerk".into();
    config.clerk_api_url = "http://127.0.0.1:9".into();
    config.sentinel_agent_webhook_url = None;
    tweak(&mut config);
    Some(AppState {
        auth,
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

/// Remove every row a test wrote for `org`. Without it the shared
/// PostgreSQL database accumulates orgs, and the cleanup pass — which
/// walks every org with a log row — gets slower on every run.
async fn forget(state: &AppState, org: &str) {
    for table in [
        "cameras",
        "camera_nodes",
        "settings",
        "notifications",
        "stream_access_logs",
        "mcp_activity_logs",
        "audit_log",
        "motion_events",
        "email_log",
        "email_outbox",
        "sentinel_runs",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1"))
            .bind(org)
            .execute(&state.pool)
            .await
            .unwrap();
    }
}

fn org(tag: &str) -> String {
    format!("loops-{tag}-{}", uuid::Uuid::new_v4().simple())
}

/// Microsecond clock, as the service writes it (`models::now_naive`):
/// SQLite stores whatever precision it is handed, and a nanosecond
/// fixture would sort just after a microsecond anchor it equals.
fn ago(d: Duration) -> NaiveDateTime {
    sentinel_command::models::now_naive() - d
}

/// A statement with binds, panicking with the SQL on failure.
macro_rules! exec {
    ($state:expr, $sql:expr $(, $bind:expr)* $(,)?) => {{
        let sql = $sql;
        sqlx::query(sql)
            $(.bind($bind))*
            .execute(&$state.pool)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }};
}

async fn count(state: &AppState, sql: &str, org: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(org)
        .fetch_one(&state.pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn add_node(
    state: &AppState,
    org: &str,
    node_id: &str,
    last_seen: Option<NaiveDateTime>,
) -> i64 {
    exec!(state, "INSERT INTO camera_nodes (node_id, org_id, api_key_hash, name, status, last_seen, created_at, updated_at)
         VALUES ($1, $2, $1, $1, 'online', $3, $4, $4)", &node_id, &org, &last_seen, &ago(Duration::days(1)));
    sqlx::query_scalar::<_, i32>("SELECT id FROM camera_nodes WHERE node_id = $1")
        .bind(node_id)
        .fetch_one(&state.pool)
        .await
        .unwrap()
        .into()
}

// ---- the offline sweep -------------------------------------------------

#[tokio::test]
async fn the_sweep_flips_only_stale_online_rows_and_announces_each() {
    let _serial = SERIAL.lock().await;
    let Some(state) = state(|_| {}).await else {
        return;
    };
    let org = org("sweep");
    let stale = format!("{org}-stale");
    let fresh = format!("{org}-fresh");
    let never = format!("{org}-never");
    let stale_id = add_node(&state, &org, &stale, Some(ago(Duration::seconds(200)))).await;
    add_node(&state, &org, &fresh, Some(ago(Duration::seconds(10)))).await;
    add_node(&state, &org, &never, None).await;
    sqlx::query(
        "INSERT INTO cameras (camera_id, org_id, name, node_id, status, last_seen, disabled_by_plan,
                              continuous_24_7, scheduled_recording, created_at, updated_at)
         VALUES ($1, $2, 'Porch', $3, 'streaming', $4, false, false, false, $5, $5)",
    )
    .bind(format!("{org}-cam"))
    .bind(&org)
    .bind(stale_id as i32)
    .bind(ago(Duration::seconds(200)))
    .bind(ago(Duration::days(1)))
    .execute(&state.pool)
    .await
    .unwrap();
    // `streaming`, which is what CameraNode actually reports — the
    // Python's sweep matched only `online`, so no real camera ever
    // flipped.
    let before = ago(Duration::seconds(1));

    let summary = loops::run_offline_sweep_with(&state, 90).await.unwrap();
    assert!(
        summary.nodes_flipped >= 1 && summary.cameras_flipped >= 1,
        "{summary:?}"
    );

    let status = |node: String| {
        let pool = state.pool.clone();
        async move {
            sqlx::query_as::<_, (String, NaiveDateTime)>(
                "SELECT status, updated_at FROM camera_nodes WHERE node_id = $1",
            )
            .bind(node)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (s, updated) = status(stale.clone()).await;
    assert_eq!(s, "offline");
    // `updated_at` is the data-sync cursor; a flip that does not bump it
    // is a flip the cloud mirror never hears about.
    assert!(updated >= before, "updated_at not bumped: {updated}");
    assert_eq!(status(fresh).await.0, "online");
    assert_eq!(
        status(never).await.0,
        "online",
        "never heard from: no transition to announce"
    );
    assert_eq!(
        count(
            &state,
            "SELECT COUNT(*) FROM cameras WHERE org_id = $1 AND status = 'offline'",
            &org
        )
        .await,
        1
    );

    let kinds: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT kind, node_id, camera_id FROM notifications WHERE org_id = $1 ORDER BY id",
    )
    .bind(&org)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        kinds,
        vec![
            ("node_offline".into(), Some(stale.clone()), None),
            (
                "camera_offline".into(),
                Some(stale),
                Some(format!("{org}-cam"))
            ),
        ],
        "nodes first, then cameras, each naming what went dark"
    );

    // A second pass finds nothing of ours left to flip.
    loops::run_offline_sweep_with(&state, 90).await.unwrap();
    assert_eq!(
        count(
            &state,
            "SELECT COUNT(*) FROM notifications WHERE org_id = $1",
            &org
        )
        .await,
        2
    );
    forget(&state, &org).await;
}

// ---- log retention -----------------------------------------------------

/// One row in each of the six tiered log tables, `age_days` old.
async fn add_logs(state: &AppState, org: &str, age_days: i64) {
    let at = ago(Duration::days(age_days));
    let tag = format!("age{age_days}");
    for sql in [
        "INSERT INTO stream_access_logs (user_id, org_id, camera_id, node_id, accessed_at) VALUES ($3, $1, 'c', 'n', $2)",
        "INSERT INTO mcp_activity_logs (org_id, tool_name, key_name, status, timestamp) VALUES ($1, $3, 'k', 'success', $2)",
        "INSERT INTO audit_log (org_id, event, timestamp) VALUES ($1, $3, $2)",
        "INSERT INTO motion_events (org_id, camera_id, node_id, score, timestamp) VALUES ($1, $3, 'n', 50, $2)",
        "INSERT INTO notifications (org_id, kind, audience, title, body, severity, created_at) VALUES ($1, 'motion', 'all', $3, '', 'info', $2)",
        "INSERT INTO email_log (org_id, recipient_email, kind, status, timestamp) VALUES ($1, 'a@example.com', $3, 'sent', $2)",
    ] {
        exec!(state, sql, &org, &at, &tag);
    }
}

async fn surviving_ages(state: &AppState, org: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for sql in [
        "SELECT user_id FROM stream_access_logs WHERE org_id = $1 ORDER BY user_id",
        "SELECT tool_name FROM mcp_activity_logs WHERE org_id = $1 ORDER BY tool_name",
        "SELECT event FROM audit_log WHERE org_id = $1 ORDER BY event",
        "SELECT camera_id FROM motion_events WHERE org_id = $1 ORDER BY camera_id",
        "SELECT title FROM notifications WHERE org_id = $1 ORDER BY title",
        "SELECT kind FROM email_log WHERE org_id = $1 ORDER BY kind",
    ] {
        out.push(
            sqlx::query_scalar(sql)
                .bind(org)
                .fetch_all(&state.pool)
                .await
                .unwrap(),
        );
    }
    out
}

#[tokio::test]
async fn retention_follows_each_orgs_plan_in_every_log_table() {
    let _serial = SERIAL.lock().await;
    let Some(state) = state(|_| {}).await else {
        return;
    };
    let free = org("free");
    let pro = org("pro");
    for age in [20, 40, 100] {
        add_logs(&state, &free, age).await;
        add_logs(&state, &pro, age).await;
    }
    exec!(
        state,
        "INSERT INTO settings (org_id, key, value, updated_at) VALUES ($1, 'org_plan', 'pro', $2)",
        &pro,
        &ago(Duration::zero())
    );

    loops::run_log_cleanup(&state).await.unwrap();

    // Free keeps 30 days, Pro 90 — in all six tables, each on its own
    // timestamp column.
    let six = |ages: &[&str]| vec![ages.iter().map(|a| a.to_string()).collect::<Vec<_>>(); 6];
    assert_eq!(surviving_ages(&state, &free).await, six(&["age20"]));
    assert_eq!(surviving_ages(&state, &pro).await, six(&["age20", "age40"]));
    forget(&state, &free).await;
    forget(&state, &pro).await;
}

#[tokio::test]
async fn the_outbox_sheds_only_old_terminal_rows_and_never_an_email_in_flight() {
    let _serial = SERIAL.lock().await;
    let Some(state) = state(|_| {}).await else {
        return;
    };
    let org = org("outbox");
    for (status, age_days) in [
        ("sent", 8),
        ("failed", 8),
        ("suppressed", 8),
        ("sent", 3),
        ("pending", 30),
        ("sending", 30),
    ] {
        exec!(state, "INSERT INTO email_outbox (org_id, recipient_email, subject, body_text, body_html, kind,
                                       status, attempts, created_at)
             VALUES ($1, 'a@example.com', $2, '', '', 'motion', $2, 0, $3)", &org.as_str(), &status, &ago(Duration::days(age_days)));
    }
    // processed_webhooks has no org: unique ids instead.
    let old_marker = format!("{org}-31d");
    let new_marker = format!("{org}-29d");
    for (id, days) in [(&old_marker, 31), (&new_marker, 29)] {
        exec!(state, "INSERT INTO processed_webhooks (svix_msg_id, event_type, processed_at) VALUES ($1, 'x', $2)", id, &ago(Duration::days(days)));
    }

    loops::run_log_cleanup(&state).await.unwrap();

    let left: Vec<(String, String)> = sqlx::query_as(
        "SELECT status, subject FROM email_outbox WHERE org_id = $1 ORDER BY status",
    )
    .bind(&org)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    let statuses: Vec<&str> = left.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(statuses, ["pending", "sending", "sent"], "{left:?}");

    let markers: Vec<String> =
        sqlx::query_scalar("SELECT svix_msg_id FROM processed_webhooks WHERE svix_msg_id LIKE $1")
            .bind(format!("{org}%"))
            .fetch_all(&state.pool)
            .await
            .unwrap();
    assert_eq!(markers, [new_marker]);
    sqlx::query("DELETE FROM processed_webhooks WHERE svix_msg_id LIKE $1")
        .bind(format!("{org}%"))
        .execute(&state.pool)
        .await
        .unwrap();
    forget(&state, &org).await;
}

// ---- the Sentinel reaper -----------------------------------------------

#[tokio::test]
async fn the_reaper_errors_stranded_and_abandoned_runs_and_leaves_live_ones() {
    let _serial = SERIAL.lock().await;
    let Some(state) = state(|_| {}).await else {
        return;
    };
    let org = org("reaper");
    // `id` is VARCHAR(32) on PostgreSQL (a uuid hex); SQLite would not
    // have noticed a longer one.
    let prefix = uuid::Uuid::new_v4().simple().to_string()[..20].to_string();
    let run = |name: &str| format!("{prefix}{name}");
    for (name, outcome, triggered_ago, started_ago) in [
        (
            "stranded",
            "running",
            Duration::minutes(30),
            Some(Duration::minutes(25)),
        ),
        (
            "working",
            "running",
            Duration::minutes(6),
            Some(Duration::minutes(5)),
        ),
        ("abandoned", "pending", Duration::hours(7), None),
        ("waiting", "pending", Duration::minutes(1), None),
        (
            "done",
            "no_action",
            Duration::hours(9),
            Some(Duration::hours(9)),
        ),
    ] {
        exec!(
            state,
            "INSERT INTO sentinel_runs (id, org_id, triggered_at, trigger_type, tool_call_count,
                                        outcome, started_at)
             VALUES ($1, $2, $3, 'manual', 0, $4, $5)",
            &run(name),
            &org.as_str(),
            &ago(triggered_ago),
            &outcome,
            &started_ago.map(ago)
        );
    }

    let summary = loops::reap_stranded_runs(&state).await.unwrap();
    assert!(summary.ids.contains(&run("stranded")), "{summary:?}");
    assert!(!summary.ids.contains(&run("working")));

    let outcomes: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT id, outcome, completed_at IS NOT NULL FROM sentinel_runs WHERE org_id = $1 ORDER BY id",
    )
    .bind(&org)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        outcomes,
        vec![
            (run("abandoned"), "error".into(), true),
            (run("done"), "no_action".into(), false),
            (run("stranded"), "error".into(), true),
            (run("waiting"), "pending".into(), false),
            (run("working"), "running".into(), false),
        ]
    );
    forget(&state, &org).await;
}

// ---- the motion digest -------------------------------------------------

async fn anchor(state: &AppState, org: &str, camera: &str, value: &str) {
    exec!(
        state,
        "INSERT INTO settings (org_id, key, value, updated_at) VALUES ($1, $2, $3, $4)",
        &org,
        &format!("motion_email_cooldown_start:{camera}"),
        &value,
        &ago(Duration::zero())
    );
}

async fn motion_at(state: &AppState, org: &str, camera: &str, at: NaiveDateTime) {
    exec!(state, "INSERT INTO motion_events (org_id, camera_id, node_id, score, timestamp) VALUES ($1, $2, 'n', 60, $3)", &org, &camera, &at);
}

fn iso(t: NaiveDateTime) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
}

#[tokio::test]
async fn the_digest_counts_the_window_reports_it_once_and_closes_it() {
    let _serial = SERIAL.lock().await;
    let Some(state) = state(|c| c.email_enabled = true).await else {
        return;
    };
    let org = org("digest");
    exec!(state, "INSERT INTO settings (org_id, key, value, updated_at) VALUES ($1, 'email_motion', 'true', $2)", &org.as_str(), &ago(Duration::zero()));
    exec!(
        state,
        "INSERT INTO cameras (camera_id, org_id, name, disabled_by_plan, continuous_24_7,
                              scheduled_recording, created_at, updated_at)
         VALUES ($1, $2, 'Back Gate', false, false, false, $3, $3)",
        &format!("{org}-busy"),
        &org.as_str(),
        &ago(Duration::days(1))
    );

    // Busy: anchor 20 min ago, the default window is 15. Three events
    // inside it count; the one AT the anchor was the immediate email,
    // and the one after the window belongs to a later cycle.
    let start = ago(Duration::minutes(20));
    let busy = format!("{org}-busy");
    anchor(&state, &org, &busy, &iso(start)).await;
    motion_at(&state, &org, &busy, start).await;
    for minutes in [1, 5, 14] {
        motion_at(&state, &org, &busy, start + Duration::minutes(minutes)).await;
    }
    motion_at(&state, &org, &busy, start + Duration::minutes(17)).await;

    // Quiet: expired with nothing after it. Open: still inside its
    // window. Corrupt: a value nothing can parse.
    let quiet = format!("{org}-quiet");
    let open = format!("{org}-open");
    anchor(&state, &org, &quiet, &iso(ago(Duration::minutes(30)))).await;
    anchor(&state, &org, &open, &iso(ago(Duration::minutes(5)))).await;
    anchor(&state, &org, &format!("{org}-corrupt"), "not a timestamp").await;

    loops::run_motion_digest(&state).await.unwrap();

    let digests: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT title, camera_id FROM notifications WHERE org_id = $1 AND kind = 'motion_digest'",
    )
    .bind(&org)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        digests,
        vec![("3 more motion events on Back Gate".to_string(), Some(busy))],
        "one digest, for the camera that had extras, under its current name"
    );

    let anchors: Vec<String> = sqlx::query_scalar(
        "SELECT key FROM settings WHERE org_id = $1 AND key LIKE 'motion_email_cooldown_start:%'",
    )
    .bind(&org)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(
        anchors,
        [format!("motion_email_cooldown_start:{open}")],
        "closed and corrupt anchors go; the open window stays"
    );
    forget(&state, &org).await;
}

#[tokio::test]
async fn no_digest_is_sent_when_the_org_has_motion_email_off() {
    let _serial = SERIAL.lock().await;
    let Some(state) = state(|c| c.email_enabled = true).await else {
        return;
    };
    let org = org("digest-off");
    let camera = format!("{org}-cam");
    let start = ago(Duration::minutes(20));
    anchor(&state, &org, &camera, &iso(start)).await;
    motion_at(&state, &org, &camera, start + Duration::minutes(2)).await;

    loops::run_motion_digest(&state).await.unwrap();

    assert_eq!(
        count(
            &state,
            "SELECT COUNT(*) FROM notifications WHERE org_id = $1",
            &org
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &state,
            "SELECT COUNT(*) FROM settings WHERE org_id = $1",
            &org
        )
        .await,
        0,
        "the window still closes"
    );
    forget(&state, &org).await;
}

// ---- coming back ---------------------------------------------------------

async fn heartbeat(state: &AppState, node_id: &str, key: &str, cameras: serde_json::Value) {
    use tower::ServiceExt;
    let body = serde_json::json!({ "node_id": node_id, "cameras": cameras });
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/nodes/heartbeat")
        .header("x-node-api-key", key)
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
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

/// A node that went offline and comes back is announced, and so is each
/// camera that went with it — reporting `streaming`, as CameraNode does.
/// Before, the HTTP heartbeat and re-registration set `online` and said
/// nothing, so "went offline" was the last word the inbox ever had.
#[tokio::test]
async fn a_node_and_camera_coming_back_from_offline_are_announced_once() {
    let _serial = SERIAL.lock().await;
    let Some(state) = state(|_| {}).await else {
        return;
    };
    let org = org("back");
    let node = format!("{org}-node");
    let camera = format!("{org}-cam");
    let key = format!("{org}-key");
    let key_hash: String = {
        use sha2::Digest;
        sha2::Sha256::digest(key.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    exec!(
        state,
        "INSERT INTO camera_nodes (node_id, org_id, api_key_hash, name, status, last_seen, created_at, updated_at)
         VALUES ($1, $2, $3, 'Shed', 'offline', $4, $4, $4)",
        &node,
        &org,
        &key_hash,
        &ago(Duration::minutes(10))
    );
    let pk: i32 = sqlx::query_scalar("SELECT id FROM camera_nodes WHERE node_id = $1")
        .bind(&node)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO cameras (camera_id, org_id, name, node_id, status, last_seen, disabled_by_plan,
                              continuous_24_7, scheduled_recording, created_at, updated_at)
         VALUES ($1, $2, 'Shed Cam', $3, 'offline', $4, false, false, false, $4, $4)",
    )
    .bind(&camera)
    .bind(&org)
    .bind(pk)
    .bind(ago(Duration::minutes(10)))
    .execute(&state.pool)
    .await
    .unwrap();

    let cameras = serde_json::json!([{ "camera_id": camera, "status": "streaming" }]);
    heartbeat(&state, &node, &key, cameras.clone()).await;
    // Already back: the second heartbeat has nothing to announce.
    heartbeat(&state, &node, &key, cameras).await;

    let announced: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, title FROM notifications WHERE org_id = $1 ORDER BY id")
            .bind(&org)
            .fetch_all(&state.pool)
            .await
            .unwrap();
    let kinds: Vec<&str> = announced.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kinds, ["node_online", "camera_online"], "{announced:?}");
    forget(&state, &org).await;
}
