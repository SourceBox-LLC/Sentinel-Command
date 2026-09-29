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
const SWEEP_ROWS: [(&str, &str); 3] = [
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
        // `seq` is a row_number over id, not the id itself: it makes the
        // emit ORDER comparable — nodes before cameras, which is a claim
        // the code makes and nothing else here checks — without pinning
        // absolute ids that any fixture change would shift.
        "notifications",
        "SELECT json_agg(json_build_array(seq, org_id, kind, audience, title, body,
                                          severity, link, camera_id, node_id)) FROM (
           SELECT row_number() OVER (ORDER BY id) AS seq,
                  org_id, kind, audience, title, body, severity, link, camera_id, node_id
             FROM notifications
            WHERE org_id LIKE 'loops-%' AND kind IN ('node_offline', 'camera_offline')
            ORDER BY id) t",
    ),
];

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
