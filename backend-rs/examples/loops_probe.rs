//! Drive the ported loop bodies, and print what they did.
//!
//! The counterpart of `tests/differential/py_loops_probe.py`. Run both
//! against the same freshly seeded database and diff the output:
//! `tests/differential/loops_run.sh`.
//!
//! Why a probe and not HTTP cases: neither loop has an HTTP surface.
//! Nothing calls them and nothing returns, which the plan names as the
//! reason they are "the least testable part and the most likely to
//! silently diverge". Both bodies are separated from their schedulers on
//! both sides, so a probe can call them directly.
//!
//! Prints the summary AND the resulting rows, because a body that
//! returns the right counts while deleting the wrong rows is exactly
//! what a count-based check cannot see — the same reason the write
//! differential snapshots tables rather than trusting a response.
//!
//! ```
//! cargo run --example loops_probe -- --db postgresql://cc:cc@127.0.0.1:15434/cc \
//!     --body sweep
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use sentinel_command::app::AppState;
use sentinel_command::config::Config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut db = String::new();
    let mut body = String::new();
    let mut license_url = String::new();
    let mut license_key = "probe-license-key".to_string();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                db = args[i + 1].clone();
                i += 2;
            }
            "--body" => {
                body = args[i + 1].clone();
                i += 2;
            }
            "--license-url" => {
                license_url = args[i + 1].clone();
                i += 2;
            }
            "--license-key" => {
                license_key = args[i + 1].clone();
                i += 2;
            }
            _ => i += 1,
        }
    }
    if !db.is_empty() {
        std::env::set_var("DATABASE_URL", &db);
    }
    // NOT local auth: `resolve_org_plan` short-circuits to `self_host`
    // before it reads a setting, which would give every org the same
    // 365-day retention and collapse the three tiers the fixture exists
    // to separate. The Python probe sets the same thing for the same
    // reason.
    std::env::set_var("AUTH_PROVIDER", "clerk");
    // The digest's emit branch is behind `email_enabled`, whose first
    // gate is the global EMAIL_ENABLED kill-switch. Left off, the branch
    // never runs — and both sides agree on having done nothing, which is
    // the exact failure loops_run.sh's coverage guard exists to catch.
    // It caught it.
    std::env::set_var("EMAIL_ENABLED", "true");

    let config = Config::from_env();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&db)
        .await?;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;

    // A real AppState, not a narrower context: the sweep's
    // notifications go through `create_notification`, which broadcasts
    // SSE and enqueues email, and that path takes the state the server
    // runs with. Building it here keeps the probe on the same code the
    // loop uses rather than a reduced version of it.
    let state = AppState {
        auth: Arc::new(sentinel_command::auth::Authenticator::from_config(
            &config,
            http.clone(),
        )),
        proxy: sentinel_command::proxy::build_client(),
        cors: sentinel_command::cors::CorsConfig::from_env(
            &config.frontend_url,
            &config.cors_allowed_origins,
        ),
        hls: Arc::new(sentinel_command::hls::HlsCache::new()),
        limiter: Arc::new(sentinel_command::ratelimit::Limiter::from_env("").await),
        http,
        config: Arc::new(config),
        pool: pool.clone(),
        started_at: Instant::now(),
        started_at_wall: chrono::Utc::now(),
    };

    // `LOCAL_ORG_ID` — the licence is a self-host concern and every one
    // of its Settings lives under the single local org.
    let org = state.config.local_org_id.clone();

    match body.as_str() {
        "sweep" => {
            let summary = sentinel_command::loops::run_offline_sweep(&state).await?;
            println!(
                "{}",
                serde_json::to_string(&json!({"summary": {
                    "cameras_flipped": summary.cameras_flipped,
                    "nodes_flipped": summary.nodes_flipped,
                }}))?
            );
            for (label, sql) in SWEEP_ROWS {
                dump(&pool, label, sql).await?;
            }
        }
        "license" => {
            let mut out = serde_json::Map::new();
            for scenario in LICENSE_SCENARIOS {
                // Selected by URL prefix so the fake stays stateless;
                // see fake_license.py. `unreachable` points at a port
                // nothing listens on, which is the grace window's whole
                // reason for existing and the branch a port is most
                // likely to turn into an error.
                let url = if scenario == "unreachable" {
                    "http://127.0.0.1:1".to_string()
                } else {
                    format!("{}/scenario/{scenario}", license_url.trim_end_matches('/'))
                };
                // Wiped between scenarios: what is compared is what THIS
                // check-in wrote, not what survived the last one.
                clear_license_settings(&pool, &org).await?;
                sentinel_command::license::check_in(
                    &pool,
                    &state.http,
                    &org,
                    &url,
                    Some(&license_key),
                )
                .await;
                out.insert(scenario.to_string(), read_license_settings(&pool, &org).await?);
            }
            println!(
                "{}",
                serde_json::to_string(&json!({"summary": serde_json::Value::Object(out)}))?
            );
        }
        "digest" => {
            // The summary is NOT printed for this body. Python's loop
            // keeps no tally — it logs per anchor and moves on — so
            // there is nothing on that side to compare a count against,
            // and printing one would be comparing the probe's own
            // arithmetic. The rows are the comparison.
            let _ = sentinel_command::loops::run_motion_digest(&state).await?;
            println!("{}", serde_json::to_string(&json!({"summary": {"ticked": true}}))?);
            for (label, sql) in DIGEST_ROWS {
                dump(&pool, label, sql).await?;
            }
        }
        "reaper" => {
            let summary = sentinel_command::loops::reap_stranded_runs(&state).await?;
            println!("{}", serde_json::to_string(&json!({"summary": summary.to_json()}))?);
            for (label, sql) in REAPER_ROWS {
                dump(&pool, label, sql).await?;
            }
        }
        "cleanup" => {
            let summary = sentinel_command::loops::run_log_cleanup(&state).await?;
            println!("{}", serde_json::to_string(&json!({"summary": summary.to_json()}))?);
            for (label, sql) in CLEANUP_ROWS {
                dump(&pool, label, sql).await?;
            }
        }
        other => {
            eprintln!("--body must be sweep or cleanup, not {other:?}");
            return Ok(());
        }
    }
    Ok(())
}

/// Every answer the check-in has to tell apart.
const LICENSE_SCENARIOS: [&str; 11] = [
    "valid",
    "valid-sync",
    "revoked",
    "sync-without-valid",
    "truthy",
    "falsy",
    "not-an-object",
    "garbage",
    "server-error",
    "missing-valid",
    "unreachable",
];

async fn clear_license_settings(pool: &sqlx::PgPool, org: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM settings
          WHERE org_id = $1
            AND (key LIKE 'sentinel_license_%' OR key = 'sentinel_data_sync_enabled')",
    )
    .bind(org)
    .execute(pool)
    .await?;
    Ok(())
}

/// The rows the check-in wrote.
///
/// `install_id` is reported as whether it EXISTS, not as its value: it
/// is sixteen random bytes, and the behaviour under test is that one
/// gets minted and reused, not which one. The timestamps likewise.
async fn read_license_settings(
    pool: &sqlx::PgPool,
    org: &str,
) -> Result<serde_json::Value, sqlx::Error> {
    let (value,): (Option<serde_json::Value>,) = sqlx::query_as(
        "SELECT json_build_object(
           'valid',             max(value) FILTER (WHERE key = 'sentinel_license_valid'),
           'reachable',         max(value) FILTER (WHERE key = 'sentinel_license_last_check_reachable'),
           'sync_enabled',      max(value) FILTER (WHERE key = 'sentinel_data_sync_enabled'),
           'has_install_id',    COALESCE(bool_or(key = 'sentinel_install_id'), false),
           'has_last_check_at', COALESCE(bool_or(key = 'sentinel_license_last_check_at'), false),
           'has_last_ok_at',    COALESCE(bool_or(key = 'sentinel_license_last_ok_at'), false))
           FROM settings WHERE org_id = $1",
    )
    .bind(org)
    .fetch_one(pool)
    .await?;
    Ok(value.unwrap_or_else(|| json!({})))
}

/// One snapshot query, printed as the Python probe prints it.
///
/// Rows come back as JSON arrays built in SQL rather than through typed
/// structs: the two probes must agree on the VALUES, and routing them
/// through Rust types would introduce a second place for a number to
/// change shape (an i32 that serialises differently from Python's int).
/// `to_jsonb` on the row and `json_agg` over it leaves Postgres as the
/// single renderer for both sides.
async fn dump(pool: &sqlx::PgPool, label: &str, sql: &str) -> Result<(), sqlx::Error> {
    let (rows,): (Option<serde_json::Value>,) = sqlx::query_as(sql).fetch_one(pool).await?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "rows": label,
            "value": rows.unwrap_or_else(|| json!([])),
        }))
        .unwrap_or_default()
    );
    Ok(())
}

/// `json_agg` over an ORDERED subquery, so the aggregate's own
/// unspecified order cannot leak into the comparison. The bodies' reads
/// have no ORDER BY either, but what is compared here is the RESULT, and
/// an unordered snapshot of a result is a flake rather than a finding.
const SWEEP_ROWS: [(&str, &str); 4] = [
    (
        "nodes",
        "SELECT json_agg(json_build_array(node_id, status)) FROM (
           SELECT node_id, status FROM camera_nodes
            WHERE org_id LIKE 'loops-%' ORDER BY node_id) t",
    ),
    (
        "cameras",
        "SELECT json_agg(json_build_array(camera_id, status)) FROM (
           SELECT camera_id, status FROM cameras
            WHERE org_id LIKE 'loops-%' ORDER BY camera_id) t",
    ),
    (
        "notifications",
        "SELECT json_agg(json_build_array(org_id, kind, audience, title, body,
                                          severity, link, camera_id, node_id)) FROM (
           SELECT org_id, kind, audience, title, body, severity, link, camera_id, node_id
             FROM notifications
            WHERE org_id LIKE 'loops-%' AND kind IN ('node_offline', 'camera_offline')
            ORDER BY org_id, kind, title) t",
    ),
    (
        // The emit ORDER, reduced to the one thing about it that IS a
        // claim. The row order WITHIN each group is not: the sweep reads
        // its stale rows with no ORDER BY on either side, so two stale
        // cameras may be announced in either order. What the code does
        // claim is that every NODE is announced before every CAMERA, so
        // an operator sees the uplink drop before the cameras behind it.
        // A row_number comparison caught the within-group order too and
        // differed for that reason alone.
        "nodes_before_cameras",
        // Wrapped as a one-row, one-column result like every other
        // entry here: the Python probe returns rows and this has to be
        // the same SHAPE, not merely the same value.
        "SELECT json_agg(json_build_array(claim)) FROM (
           SELECT COALESCE(
             (SELECT max(id) FILTER (WHERE kind = 'node_offline')
                   < min(id) FILTER (WHERE kind = 'camera_offline')
                FROM notifications
               WHERE org_id LIKE 'loops-%'
                 AND kind IN ('node_offline', 'camera_offline')), false) AS claim) t",
    ),
];

/// The digest's rows: which anchors survived and what was announced.
const DIGEST_ROWS: [(&str, &str); 2] = [
    (
        "anchors",
        "SELECT json_agg(json_build_array(org_id, key, rearmed, blank)) FROM (
           SELECT org_id, key, value = '@rearmed' AS rearmed,
                  value IS NULL OR value = '' AS blank
             FROM settings WHERE key LIKE 'motion_email_cooldown_start:%'
            ORDER BY org_id, key) t",
    ),
    (
        "digests",
        "SELECT json_agg(json_build_array(org_id, title, body, severity, audience,
                                          link, camera_id, meta_json)) FROM (
           SELECT org_id, title, body, severity, audience, link, camera_id, meta_json
             FROM notifications WHERE kind = 'motion_digest'
            ORDER BY org_id, title) t",
    ),
];

/// The reaper's rows. `completed_at` is reduced to whether it is set:
/// both sides stamp `now()`, seconds apart, so the value itself is not
/// comparable and its PRESENCE is the behaviour.
const REAPER_ROWS: [(&str, &str); 1] = [(
    "runs",
    "SELECT json_agg(json_build_array(id, outcome, summary, completed)) FROM (
       SELECT id, outcome, summary, completed_at IS NOT NULL AS completed
         FROM sentinel_runs WHERE org_id LIKE 'loops-%' ORDER BY id) t",
)];

/// Ages in whole days rather than timestamps: the two probes run seconds
/// apart, and a literal timestamp would differ for that reason alone.
/// Whole days is the granularity every cutoff here uses.
const CLEANUP_ROWS: [(&str, &str); 8] = [
    (
        "stream",
        "SELECT json_agg(json_build_array(org_id, age)) FROM (
           SELECT org_id, round(extract(epoch FROM now()::timestamp - accessed_at)
                                / 86400)::int AS age
             FROM stream_access_logs WHERE org_id LIKE 'loops-%' ORDER BY org_id, age) t",
    ),
    (
        "mcp",
        "SELECT json_agg(json_build_array(org_id, age)) FROM (
           SELECT org_id, round(extract(epoch FROM now()::timestamp - timestamp)
                                / 86400)::int AS age
             FROM mcp_activity_logs WHERE org_id LIKE 'loops-%' ORDER BY org_id, age) t",
    ),
    (
        "audit",
        "SELECT json_agg(json_build_array(org_id, age)) FROM (
           SELECT org_id, round(extract(epoch FROM now()::timestamp - timestamp)
                                / 86400)::int AS age
             FROM audit_log WHERE org_id LIKE 'loops-%' ORDER BY org_id, age) t",
    ),
    (
        "motion",
        "SELECT json_agg(json_build_array(org_id, age)) FROM (
           SELECT org_id, round(extract(epoch FROM now()::timestamp - timestamp)
                                / 86400)::int AS age
             FROM motion_events WHERE org_id LIKE 'loops-%' ORDER BY org_id, age) t",
    ),
    (
        "notif",
        "SELECT json_agg(json_build_array(org_id, age)) FROM (
           SELECT org_id, round(extract(epoch FROM now()::timestamp - created_at)
                                / 86400)::int AS age
             FROM notifications WHERE org_id LIKE 'loops-%' ORDER BY org_id, age) t",
    ),
    (
        "email_log",
        "SELECT json_agg(json_build_array(org_id, age)) FROM (
           SELECT org_id, round(extract(epoch FROM now()::timestamp - timestamp)
                                / 86400)::int AS age
             FROM email_log WHERE org_id LIKE 'loops-%' ORDER BY org_id, age) t",
    ),
    (
        // Status as well as age: the whole point of the outbox rule is
        // that a pending row survives an age a sent row does not.
        "email_outbox",
        "SELECT json_agg(json_build_array(org_id, status, age)) FROM (
           SELECT org_id, status, round(extract(epoch FROM now()::timestamp - created_at)
                                        / 86400)::int AS age
             FROM email_outbox WHERE org_id LIKE 'loops-%'
            ORDER BY org_id, status, age) t",
    ),
    (
        "processed_webhooks",
        "SELECT json_agg(json_build_array(svix_msg_id, age)) FROM (
           SELECT svix_msg_id, round(extract(epoch FROM now()::timestamp - processed_at)
                                     / 86400)::int AS age
             FROM processed_webhooks WHERE svix_msg_id LIKE 'loops-%'
            ORDER BY svix_msg_id) t",
    ),
];
