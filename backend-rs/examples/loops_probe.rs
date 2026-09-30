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
    let mut clerk_url = String::new();
    let mut sync_url = String::new();
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
            "--clerk-url" => {
                clerk_url = args[i + 1].clone();
                i += 2;
            }
            "--sync-url" => {
                sync_url = args[i + 1].clone();
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
        "sync" => {
            let base = sync_url.trim_end_matches('/').to_string();
            let mut sync_state = state.clone();
            let mut config = (*state.config).clone();
            config.sentinel_sync_service_url = base.clone();
            config.sentinel_license_key = Some(license_key.clone());
            // LOCAL auth for this body, and only this body. The mirror
            // is a self-host feature — `is_sync_enabled` returns false
            // for a hosted org before it looks at anything else — while
            // the reconcile above needs the opposite, because a
            // self-host short-circuit would skip the live Clerk lookup
            // that is its entire purpose. Set per body rather than for
            // the process, which is what left this one pushing nothing
            // while both sides agreed about it.
            config.auth_provider = "local".to_string();
            sync_state.config = Arc::new(config);
            let http = &state.http;

            let mut out = serde_json::Map::new();
            post(http, &base, "/__reset", &json!({})).await?;
            sentinel_command::sync::push_pending_changes(&sync_state).await;
            out.insert("first".into(), summarise_pushes(http, &base).await?);
            out.insert("cursors_after_first".into(), read_cursors(&pool, &org).await?);

            // Nothing has changed since, so a correct cursor means an
            // empty cycle. This is the case a port that never advanced
            // its cursor fails, and the only one that catches it.
            post(http, &base, "/__reset", &json!({})).await?;
            sentinel_command::sync::push_pending_changes(&sync_state).await;
            out.insert("second".into(), summarise_pushes(http, &base).await?);

            // One table scripted to 500. Its cursor must NOT advance,
            // and every other table must still push — the cursors are
            // independent and partial progress is the design.
            post(http, &base, "/__reset", &json!({})).await?;
            post(http, &base, "/__fail", &json!({"tables": ["motion_events"]})).await?;
            sqlx::query("DELETE FROM settings WHERE key LIKE 'sentinel_sync_cursor_%'")
                .execute(&pool)
                .await?;
            sentinel_command::sync::push_pending_changes(&sync_state).await;
            out.insert("with_failure".into(), summarise_pushes(http, &base).await?);
            out.insert("cursors_after_failure".into(), read_cursors(&pool, &org).await?);

            println!(
                "{}",
                serde_json::to_string(&json!({"summary": serde_json::Value::Object(out)}))?
            );
        }
        "reconcile" => {
            // NOT local auth for this body: `fetch_live_plan_slug` is
            // the whole point and a self-host short-circuit would skip
            // it. The probe's AUTH_PROVIDER is already clerk.
            let base = format!("{}/v1", clerk_url.trim_end_matches('/'));
            state
                .http
                .post(format!("{}/__scenario", clerk_url.trim_end_matches('/')))
                .json(&reconcile_scenarios())
                .send()
                .await?
                .error_for_status()?;
            let mut reconcile_state = state.clone();
            let mut config = (*state.config).clone();
            config.clerk_api_url = base;
            config.clerk_secret_key = "sk_test_probe".to_string();
            reconcile_state.config = Arc::new(config);

            let summary = sentinel_command::loops::reconcile_org_plans(&reconcile_state).await?;
            // Only `changed` is printed, because only `changed` is what
            // Python's `_reconcile_org_plans` RETURNS — the rest of the
            // struct feeds the port's log line and has no counterpart
            // to compare against. What it would show is in the rows
            // below anyway: `corrections` is the `plans` snapshot said
            // twice.
            println!(
                "{}",
                serde_json::to_string(&json!({"summary": {"changed": summary.changed}}))?
            );
            for (label, sql) in RECONCILE_ROWS {
                dump(&pool, label, sql).await?;
            }
        }
        "license" => {
            let mut out = serde_json::Map::new();
            let mut seen_install_id: Option<String> = None;
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
                let mut row = read_license_settings(&pool, &org).await?;
                // STABILITY, not presence. `has_install_id` was true
                // whether the id was reused or re-minted, so the reuse —
                // which is the behaviour, since the licence service
                // counts installs and a per-boot id reads as an install
                // per boot — was invisible and the mutation for it
                // scored zero.
                let current = install_id(&pool, &org).await?;
                let stable = match &seen_install_id {
                    None => serde_json::Value::Null,
                    Some(previous) => json!(previous == &current),
                };
                if let Some(map) = row.as_object_mut() {
                    map.insert("install_id_stable".into(), stable);
                }
                seen_install_id = Some(current);
                out.insert(scenario.to_string(), row);
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

async fn post(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    client.post(format!("{base}{path}")).json(body).send().await?.error_for_status()?;
    Ok(())
}

/// What was sent, reduced to what is actually a contract.
///
/// Row VALUES are not compared — they are the fixture, and comparing
/// them would make this a slow copy of the write differential. The
/// column NAMES are, because the denylist is only observable in the
/// request body, and so is the id list, the table order and the counts.
async fn summarise_pushes(
    client: &reqwest::Client,
    base: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let pushes: Vec<serde_json::Value> =
        client.get(format!("{base}/__pushes")).send().await?.json().await?;
    let mut out = Vec::new();
    for push in pushes {
        let rows = push["rows"].as_array().cloned().unwrap_or_default();
        let mut columns = std::collections::BTreeSet::new();
        let mut envelope = std::collections::BTreeSet::new();
        for row in &rows {
            if let Some(map) = row.as_object() {
                envelope.extend(map.keys().cloned());
                if let Some(data) = map.get("data").and_then(|d| d.as_object()) {
                    columns.extend(data.keys().cloned());
                }
            }
        }
        let known_ids = match push["known_ids"].as_array() {
            // Sorted: the id set is a set, and the query behind it has
            // no ORDER BY on either side.
            Some(ids) => {
                let mut ids: Vec<String> =
                    ids.iter().filter_map(|i| i.as_str().map(str::to_string)).collect();
                ids.sort();
                json!(ids)
            }
            None => serde_json::Value::Null,
        };
        out.push(json!({
            "table": push["table"],
            "authorized": push["authorized"],
            "row_count": push["row_count"],
            "columns": columns.into_iter().collect::<Vec<_>>(),
            "known_ids": known_ids,
            "envelope": envelope.into_iter().collect::<Vec<_>>(),
        }));
    }
    Ok(json!(out))
}

async fn install_id(pool: &sqlx::PgPool, org: &str) -> Result<String, sqlx::Error> {
    let got: Option<(String,)> = sqlx::query_as(
        "SELECT value FROM settings WHERE org_id = $1 AND key = 'sentinel_install_id'",
    )
    .bind(org)
    .fetch_optional(pool)
    .await?;
    Ok(got.map(|(v,)| v).unwrap_or_default())
}

/// Cursor presence, not value: the values are fixture timestamps.
async fn read_cursors(
    pool: &sqlx::PgPool,
    org: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT key, value FROM settings
          WHERE org_id = $1 AND key LIKE 'sentinel_sync_cursor_%' ORDER BY key",
    )
    .bind(org)
    .fetch_all(pool)
    .await?;
    let mut out = serde_json::Map::new();
    for (key, value) in rows {
        out.insert(key, json!(value.is_some_and(|v| !v.is_empty())));
    }
    Ok(serde_json::Value::Object(out))
}

/// Which answer the fake Clerk gives for each org. The names are the
/// fake's own contract; `loops_fixture.sql` says what each org proves.
fn reconcile_scenarios() -> serde_json::Value {
    json!({
        "rec-agree": "active_pro",
        "rec-downgrade": "active_free",
        "rec-upgrade": "active_pro_plus",
        "rec-unreachable": "error_500",
        "rec-free": "active_pro",
    })
}

const RECONCILE_ROWS: [(&str, &str); 2] = [
    (
        "plans",
        "SELECT json_agg(json_build_array(org_id, value)) FROM (
           SELECT org_id, value FROM settings
            WHERE key = 'org_plan' AND org_id LIKE 'rec-%' ORDER BY org_id) t",
    ),
    (
        // The cap's work. A reconcile that wrote the setting and skipped
        // `enforce_camera_cap` looks right in `settings` and leaves the
        // org streaming past its new plan.
        "capped",
        "SELECT json_agg(json_build_array(camera_id, disabled_by_plan)) FROM (
           SELECT camera_id, disabled_by_plan FROM cameras
            WHERE org_id LIKE 'rec-%' ORDER BY camera_id) t",
    ),
];

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
        // `touched` is whether `updated_at` was stamped by THIS sweep,
        // not its value — the two probes run seconds apart. It is here
        // because the column is the data-sync cursor for both these
        // tables: a row flipped offline without bumping it is a row the
        // mirror never hears about again. The snapshot missed that until
        // `column_defaults.py` found the omission, which was a gap here
        // as much as a bug in the code.
        "nodes",
        "SELECT json_agg(json_build_array(node_id, status, touched)) FROM (
           SELECT node_id, status, updated_at >= now()::timestamp - interval '2 minutes' AS touched
             FROM camera_nodes WHERE org_id LIKE 'loops-%' ORDER BY node_id) t",
    ),
    (
        "cameras",
        "SELECT json_agg(json_build_array(camera_id, status, touched)) FROM (
           SELECT camera_id, status, updated_at >= now()::timestamp - interval '2 minutes' AS touched
             FROM cameras WHERE org_id LIKE 'loops-%' ORDER BY camera_id) t",
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
    "SELECT json_agg(json_build_array(id, outcome, summary, completed, touched)) FROM (
       SELECT id, outcome, summary, completed_at IS NOT NULL AS completed,
              updated_at >= now()::timestamp - interval '2 minutes' AS touched
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
