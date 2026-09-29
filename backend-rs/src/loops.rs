//! The background loops, and the bodies they schedule.
//!
//! Ported from `backend/app/main.py`'s lifespan. Two shapes here, and
//! the split is Python's rather than mine: each loop is a thin
//! scheduler around a body that takes a connection and returns a
//! summary, because a loop with its work inlined cannot be driven by a
//! test at all — nothing calls it and nothing returns.
//!
//! **A loop is the least testable thing in this codebase**, which is
//! why the bodies are separated so carefully. `run_log_cleanup`'s own
//! docstring in the Python records what that costs when it is not: an
//! `AttributeError` from a SQLAlchemy 2.x incompatibility ran nightly
//! for an unknown stretch, swallowed by the loop's outer `try/except`,
//! until a Sentry alert surfaced it. The response was to extract the
//! body — which is the shape this port follows, and the reason both of
//! these are `pub`.
//!
//! The outer `try/except` is reproduced (a loop that dies on one bad
//! tick stops sweeping forever), but it LOGS at error level rather than
//! swallowing silently, which is the one thing that made that bug
//! invisible.

use crate::app::AppState;

/// `OFFLINE_HEARTBEAT_TIMEOUT_SECONDS`. Not configurable in Python
/// either — the 90 seconds is also what `effective_status` uses, and
/// the two have to agree or a row reads offline while the sweep still
/// thinks it is online.
pub const OFFLINE_HEARTBEAT_TIMEOUT_SECONDS: i64 = 90;

/// What one sweep did, for the log line and for a test to assert on.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepSummary {
    pub nodes_flipped: usize,
    pub cameras_flipped: usize,
}

/// A row the sweep is about to flip, held between the UPDATE and the
/// notification so the emit happens after the commit.
#[derive(Debug, sqlx::FromRow)]
struct StaleRow {
    ident: String,
    org_id: String,
    display: String,
    /// The camera's parent node id, resolved through the FK. Always
    /// `None` for a node's own row.
    parent: Option<String>,
}

/// `run_offline_sweep` — flip stale `online` rows to `offline`.
///
/// A heart-beating node reports every ~30s. If it crashes, nothing
/// else ever writes `offline` and no transition notification fires, so
/// the dashboard shows a camera that has been dark for hours as live.
///
/// Two details that are easy to lose:
///
///   * `last_seen IS NOT NULL` is part of the filter. A row that has
///     never been heard from has no transition to announce — if its
///     status says `online` that is a different bug, and emitting
///     "went offline" for it would invent an event that never happened.
///   * the notifications are emitted AFTER the commit, so one can never
///     reference a row that was rolled back. That ordering is why the
///     rows are collected first rather than emitted in the loop.
pub async fn run_offline_sweep(state: &AppState) -> Result<SweepSummary, sqlx::Error> {
    run_offline_sweep_with(state, OFFLINE_HEARTBEAT_TIMEOUT_SECONDS).await
}

/// The sweep with the threshold as an argument, so a test can place a
/// fixture on either side of it without waiting ninety seconds.
pub async fn run_offline_sweep_with(
    state: &AppState,
    timeout_seconds: i64,
) -> Result<SweepSummary, sqlx::Error> {
    let cutoff = crate::models::now_naive() - chrono::Duration::seconds(timeout_seconds);

    // RETURNING rather than SELECT-then-UPDATE: Python reads the rows,
    // mutates them and commits in one session, and a separate SELECT
    // here would let a heartbeat land between the two and flip a row
    // that had just come back.
    let nodes: Vec<StaleRow> = sqlx::query_as(
        "UPDATE camera_nodes SET status = 'offline'
          WHERE status = 'online' AND last_seen IS NOT NULL AND last_seen < $1
         RETURNING node_id AS ident, org_id,
                   COALESCE(NULLIF(name, ''), node_id) AS display,
                   NULL::text AS parent",
    )
    .bind(cutoff)
    .fetch_all(&state.pool)
    .await?;

    let cameras: Vec<StaleRow> = sqlx::query_as(
        "UPDATE cameras c SET status = 'offline'
          WHERE c.status = 'online' AND c.last_seen IS NOT NULL AND c.last_seen < $1
         RETURNING c.camera_id AS ident, c.org_id,
                   COALESCE(NULLIF(c.name, ''), c.camera_id) AS display,
                   (SELECT n.node_id FROM camera_nodes n WHERE n.id = c.node_id) AS parent",
    )
    .bind(cutoff)
    .fetch_all(&state.pool)
    .await?;

    let summary = SweepSummary {
        nodes_flipped: nodes.len(),
        cameras_flipped: cameras.len(),
    };

    // Nodes first, then cameras — the notification ids follow this
    // order and a client renders the inbox by id.
    for node in &nodes {
        crate::notifications::emit_node_transition(
            state,
            &node.org_id,
            &node.ident,
            &node.display,
            "offline",
        )
        .await;
    }
    for camera in &cameras {
        crate::notifications::emit_camera_transition(
            state,
            &camera.org_id,
            &camera.ident,
            &camera.display,
            "offline",
            camera.parent.as_deref(),
        )
        .await;
    }

    Ok(summary)
}

/// What one cleanup pass deleted, per table.
///
/// The key order is Python's `totals` dict order, because the summary
/// is logged and read by a human comparing two runs.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupSummary {
    pub orgs_processed: usize,
    pub stream: u64,
    pub mcp: u64,
    pub audit: u64,
    pub motion: u64,
    pub notif: u64,
    pub email_log: u64,
    pub email_outbox: u64,
    pub processed_webhooks: u64,
}

impl CleanupSummary {
    pub fn total_deleted(&self) -> u64 {
        self.stream
            + self.mcp
            + self.audit
            + self.motion
            + self.notif
            + self.email_log
            + self.email_outbox
            + self.processed_webhooks
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "orgs_processed": self.orgs_processed,
            "totals": {
                "stream": self.stream,
                "mcp": self.mcp,
                "audit": self.audit,
                "motion": self.motion,
                "notif": self.notif,
                "email_log": self.email_log,
                "email_outbox": self.email_outbox,
                "processed_webhooks": self.processed_webhooks,
            },
            "total_deleted": self.total_deleted(),
        })
    }
}

/// Terminal outbox rows older than this go, whatever the org's tier.
///
/// The outbox is a QUEUE, not a log: the worker scans it on every tick,
/// so it wants to be small at all times, and the long-term per-org
/// audit trail lives in `email_log` with the tiered retention. Pending
/// and sending rows are never deleted at any age — dropping an
/// in-flight retry loses the email silently.
const OUTBOX_TERMINAL_DAYS: i64 = 7;

/// Webhook dedup markers older than this go.
///
/// Thirty days comfortably exceeds Svix's ~5-day retry window, so a
/// swept marker can never let a retried delivery double-process. The
/// table has no `org_id` and cannot be tiered.
const WEBHOOK_MARKER_DAYS: i64 = 30;

/// `run_log_cleanup` — delete log rows past each org's retention window.
///
/// Retention is tiered (Free 30d / Pro 90d / Pro Plus 365d), so this
/// iterates orgs rather than running one global cutoff. The org list is
/// a UNION across the six log tables, which is also how an org with
/// email-only activity still gets swept.
///
/// Python takes a `default_retention_days` for an org whose plan cannot
/// be resolved. It is unreachable: `get_plan_limits` falls back to the
/// free tier's whole dict, which always carries `log_retention_days`,
/// so the `.get(..., default)` never fires. Its own docstring calls it
/// "a parameter for test override". Not carried here — a parameter that
/// cannot change an answer is a parameter that will be wrong one day
/// and never noticed.
pub async fn run_log_cleanup(state: &AppState) -> Result<CleanupSummary, sqlx::Error> {
    let now = crate::models::now_naive();

    // UNION, not UNION ALL: the set is deduplicated. Empty strings are
    // dropped as well as NULLs, because Python filters on `if row[0]`
    // and an empty org_id is falsy — a row with one is corrupt and
    // sweeping it would resolve a plan for an org that does not exist.
    let org_ids: Vec<(String,)> = sqlx::query_as(
        "SELECT org_id FROM stream_access_logs
         UNION SELECT org_id FROM mcp_activity_logs
         UNION SELECT org_id FROM audit_log
         UNION SELECT org_id FROM motion_events
         UNION SELECT org_id FROM notifications
         UNION SELECT org_id FROM email_log",
    )
    .fetch_all(&state.pool)
    .await?;

    let ctx = crate::plans::PlanContext {
        pool: &state.pool,
        client: &state.http,
        clerk_base_url: &state.config.clerk_api_url,
        clerk_secret: &state.config.clerk_secret_key,
        local_auth: state.config.is_local_auth(),
    };

    let mut summary = CleanupSummary::default();
    for (org_id,) in org_ids.iter().filter(|(id,)| !id.is_empty()) {
        summary.orgs_processed += 1;
        // Python wraps the resolve in a bare `except Exception` and
        // falls back to `free_org`. The Rust resolver cannot raise — it
        // returns a slug either way — so there is nothing to catch, and
        // an unknown slug already resolves to the free tier's limits.
        let plan = crate::plans::resolve_org_plan(&ctx, org_id).await;
        let retention = crate::plans::get_plan_limits(&plan).log_retention_days;
        let cutoff = now - chrono::Duration::days(retention);

        // One statement per table, each on its OWN timestamp column.
        // They are not interchangeable: `accessed_at` on stream logs,
        // `timestamp` on three, `created_at` on notifications.
        for (sql, counter) in [
            (
                "DELETE FROM stream_access_logs WHERE org_id = $1 AND accessed_at < $2",
                &mut summary.stream,
            ),
            (
                "DELETE FROM mcp_activity_logs WHERE org_id = $1 AND timestamp < $2",
                &mut summary.mcp,
            ),
            (
                "DELETE FROM audit_log WHERE org_id = $1 AND timestamp < $2",
                &mut summary.audit,
            ),
            (
                "DELETE FROM motion_events WHERE org_id = $1 AND timestamp < $2",
                &mut summary.motion,
            ),
            (
                "DELETE FROM notifications WHERE org_id = $1 AND created_at < $2",
                &mut summary.notif,
            ),
            (
                "DELETE FROM email_log WHERE org_id = $1 AND timestamp < $2",
                &mut summary.email_log,
            ),
        ] {
            let done = sqlx::query(sql)
                .bind(org_id)
                .bind(cutoff)
                .execute(&state.pool)
                .await?;
            *counter += done.rows_affected();
        }
    }

    // Cross-org, terminal-state only, fixed window.
    let outbox_cutoff = now - chrono::Duration::days(OUTBOX_TERMINAL_DAYS);
    summary.email_outbox = sqlx::query(
        "DELETE FROM email_outbox
          WHERE status IN ('sent', 'failed', 'suppressed') AND created_at < $1",
    )
    .bind(outbox_cutoff)
    .execute(&state.pool)
    .await?
    .rows_affected();

    // `processed_at`, not `created_at` — the column does not exist
    // under that name, and a port that guessed it 500'd every
    // message-id case in the webhook differential.
    let webhook_cutoff = now - chrono::Duration::days(WEBHOOK_MARKER_DAYS);
    summary.processed_webhooks =
        sqlx::query("DELETE FROM processed_webhooks WHERE processed_at < $1")
            .bind(webhook_cutoff)
            .execute(&state.pool)
            .await?
            .rows_affected();

    Ok(summary)
}

/// Spawn the two loops.
///
/// Each catches its own body's failure and keeps its cadence. A loop
/// that exits on one bad tick stops sweeping for the process's
/// lifetime, and nothing announces that it has — which is strictly
/// worse than a tick that failed loudly and will be retried.
pub fn spawn_loops(state: AppState) {
    let sweep_interval = state.config.offline_sweep_interval_seconds;
    let sweep_state = state.clone();
    tokio::spawn(async move {
        loop {
            // Sleep FIRST, like Python: neither loop does a pass at
            // startup. For the sweep that is deliberate — a node that
            // reconnects during a deploy gets its heartbeat in before
            // the first sweep rather than a spurious offline
            // notification from the gap the restart itself caused.
            tokio::time::sleep(std::time::Duration::from_secs(sweep_interval)).await;
            match run_offline_sweep(&sweep_state).await {
                Ok(summary) => {
                    let total = summary.nodes_flipped + summary.cameras_flipped;
                    if total > 0 {
                        tracing::info!(
                            nodes = summary.nodes_flipped,
                            cameras = summary.cameras_flipped,
                            "offline sweep flipped {total} entit{} to offline",
                            if total == 1 { "y" } else { "ies" }
                        );
                    }
                }
                Err(error) => tracing::error!(%error, "offline sweep failed"),
            }
        }
    });

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(
                LOG_CLEANUP_INTERVAL_HOURS * 3600,
            ))
            .await;
            match run_log_cleanup(&state).await {
                Ok(summary) => tracing::info!(
                    orgs = summary.orgs_processed,
                    deleted = summary.total_deleted(),
                    "log cleanup swept {} org(s)",
                    summary.orgs_processed
                ),
                // Logged, not swallowed. The Python's outer handler is
                // what hid a nightly AttributeError for an unknown
                // stretch; see the module note.
                Err(error) => tracing::error!(%error, "log cleanup failed"),
            }
        }
    });
}

/// `LOG_CLEANUP_INTERVAL_HOURS` — once a day, and not configurable in
/// Python either.
const LOG_CLEANUP_INTERVAL_HOURS: u64 = 24;

#[cfg(test)]
mod tests {
    use super::*;

    /// The summary's arithmetic, which is what the log line reports and
    /// the one number an operator reads. Easy to get wrong by leaving a
    /// table out of the sum — which would under-report a sweep that
    /// deleted hundreds of thousands of rows.
    #[test]
    fn the_total_counts_every_table() {
        let summary = CleanupSummary {
            orgs_processed: 2,
            stream: 1,
            mcp: 2,
            audit: 4,
            motion: 8,
            notif: 16,
            email_log: 32,
            email_outbox: 64,
            processed_webhooks: 128,
        };
        // Powers of two, so a missing term is visible in the total
        // rather than merely wrong.
        assert_eq!(summary.total_deleted(), 255);
        assert_eq!(summary.to_json()["total_deleted"], 255);
        assert_eq!(summary.to_json()["orgs_processed"], 2);
        assert_eq!(summary.to_json()["totals"]["processed_webhooks"], 128);
    }

    /// An empty pass reports zeroes rather than omitting the keys — a
    /// consumer reading `totals.stream` should not have to check.
    #[test]
    fn an_empty_pass_still_reports_every_key() {
        let json = CleanupSummary::default().to_json();
        for key in [
            "stream",
            "mcp",
            "audit",
            "motion",
            "notif",
            "email_log",
            "email_outbox",
            "processed_webhooks",
        ] {
            assert_eq!(json["totals"][key], 0, "{key}");
        }
        assert_eq!(json["total_deleted"], 0);
    }
}
