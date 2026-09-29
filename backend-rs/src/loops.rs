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
///   * `last_seen IS NOT NULL` is part of the filter, and it does NOT
///     do the work its presence suggests. A row that has never been
///     heard from has no transition to announce — emitting "went
///     offline" for it would invent an event that never happened — but
///     what actually excludes it is SQL's three-valued logic:
///     `NULL < timestamp` is NULL, and `WHERE` treats that as not-true.
///     The predicate is Python's, carried for fidelity and because it
///     states the intent; removing it changes no row. Verified against
///     the fixture, and both mutations for it are marked equivalent in
///     `mutations/loops.json` rather than left looking uncovered.
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

/// `STRANDED_RUN_AGE_MINUTES` — how long a `running` row may sit before
/// the reaper calls it lost.
const STRANDED_RUN_AGE_MINUTES: i64 = 20;
/// A `pending` row unclaimed for this long re-fires the wakeup.
const STALE_PENDING_MINUTES: i64 = 2;
/// And one unclaimed for this long is given up on.
const ABANDONED_PENDING_HOURS: i64 = 6;

/// What one reaper pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReaperSummary {
    pub reaped: u64,
    /// The ids the SELECT found — `stranded_ids if reaped else []`.
    ///
    /// The ORDER is not a contract: the SELECT behind it has no
    /// ORDER BY on either side, so Postgres answers in physical order
    /// and may answer differently twice. `loops_run.sh` sorts it before
    /// comparing.
    ///
    /// Note that this is the ids the SELECT found, not the ids the
    /// UPDATE changed. When a concurrent `/complete` lands between the
    /// two, the count is lower than the list and Python reports both as
    /// they are. Reproduced rather than tidied: a log line that names a
    /// run the reaper did NOT touch is a smaller problem than a port
    /// that quietly disagrees with the one on the other machine.
    pub ids: Vec<String>,
    pub rewoken_pending: i64,
    pub abandoned: u64,
}

impl ReaperSummary {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "reaped": self.reaped,
            "ids": self.ids,
            "rewoken_pending": self.rewoken_pending,
            "abandoned": self.abandoned,
        })
    }
}

/// `reap_stranded_runs` — three sweeps over `sentinel_runs`, in order.
///
/// The agent's own wall-clock wrapper handles the common timeout by
/// POSTing `/complete`. This is the backstop for when the agent never
/// got that far: an OOM, a container kill, a partition long enough that
/// the cleanup POST itself failed. Without it the row sits at `running`
/// forever — `/runs/pending` only returns `pending`, `/start` does not
/// re-claim `running`, and the dashboard spins.
///
/// Cross-org by design: a stranded run is stranded regardless of who
/// owns it.
pub async fn reap_stranded_runs(state: &AppState) -> Result<ReaperSummary, sqlx::Error> {
    let now = crate::models::now_naive();
    let cutoff = now - chrono::Duration::minutes(STRANDED_RUN_AGE_MINUTES);

    let stranded: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM sentinel_runs
          WHERE outcome = 'running' AND started_at IS NOT NULL AND started_at < $1",
    )
    .bind(cutoff)
    .fetch_all(&state.pool)
    .await?;
    let stranded_ids: Vec<String> = stranded.into_iter().map(|(id,)| id).collect();

    let mut summary = ReaperSummary::default();
    if !stranded_ids.is_empty() {
        // The `outcome = 'running'` re-check belongs in the WRITE, not
        // just the SELECT. The load-then-stamp version had a window in
        // which a concurrent `/complete` landed and had its real
        // outcome overwritten with `error` — leaving a row carrying
        // `error` beside the completion's own severity and incident_id,
        // and no repair path, because the agent's POST had already
        // succeeded.
        summary.reaped = sqlx::query(
            "UPDATE sentinel_runs
                SET outcome = 'error', summary = $1, completed_at = $2
              WHERE id = ANY($3) AND outcome = 'running'",
        )
        .bind(format!(
            "Stranded — agent never completed within {STRANDED_RUN_AGE_MINUTES} min.  \
             Reaped automatically."
        ))
        .bind(now)
        .bind(&stranded_ids)
        .execute(&state.pool)
        .await?
        .rows_affected();
        if summary.reaped > 0 {
            tracing::warn!(
                reaped = summary.reaped,
                "sentinel: reaper marked stranded run(s) as error"
            );
        }
        summary.ids = stranded_ids;
    }

    // A lost wakeup used to strand rows at `pending` FOREVER on a quiet
    // system: the reaper only handled `running`, `/runs/pending` only
    // helps an agent already awake, and "the next wakeup" never comes
    // when this org's motion was the only trigger. One webhook wakes the
    // agent, which then drains every pending run across all orgs.
    let pending_cutoff = now - chrono::Duration::minutes(STALE_PENDING_MINUTES);
    let (stale_pending,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sentinel_runs
          WHERE outcome = 'pending' AND triggered_at < $1",
    )
    .bind(pending_cutoff)
    .fetch_one(&state.pool)
    .await?;
    summary.rewoken_pending = stale_pending;
    if stale_pending > 0 {
        tracing::warn!(
            pending = stale_pending,
            "sentinel: pending run(s) unclaimed for >2 min — re-firing wakeup"
        );
        crate::api::sentinel_config::fire_wakeup_webhook(state);
    }

    // Terminal backstop. Six hours of pending means the agent has been
    // unreachable across seventy-odd re-fired wakeups; surface the
    // failure rather than hold a cap slot and a spinner forever. The
    // error → real-outcome upgrade path still applies if the agent ever
    // completes one of these later.
    let abandoned_cutoff = now - chrono::Duration::hours(ABANDONED_PENDING_HOURS);
    summary.abandoned = sqlx::query(
        "UPDATE sentinel_runs
            SET outcome = 'error', summary = $1, completed_at = $2
          WHERE outcome = 'pending' AND triggered_at < $3",
    )
    .bind(
        "Abandoned — agent never claimed this run within 6 hours \
         (wakeup webhook unreachable?).  Marked errored automatically.",
    )
    .bind(now)
    .bind(abandoned_cutoff)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if summary.abandoned > 0 {
        tracing::warn!(
            abandoned = summary.abandoned,
            "sentinel: marked abandoned pending run(s) as error"
        );
    }

    Ok(summary)
}

/// `DISK_CRITICAL_THRESHOLD_PERCENT`.
const DISK_CRITICAL_THRESHOLD_PERCENT: f64 = 95.0;
/// `DISK_CRITICAL_REEMIT_INTERVAL_SECONDS` — six hours.
const DISK_CRITICAL_REEMIT_INTERVAL_SECONDS: f64 = 6.0 * 3600.0;

/// The operator alert the disk check emits, or nothing.
///
/// Returned rather than logged from the decision function so the choice
/// is testable: the whole of this loop's behaviour is a boolean and the
/// numbers on one log line, and neither reaches a database.
#[derive(Debug, PartialEq)]
pub struct DiskAlert {
    /// `round(pct, 1)`.
    pub percent_used: f64,
    pub bytes_free: u64,
    pub path: String,
}

/// The debounce state, which is per PROCESS and not per org.
///
/// `None` means nothing has been emitted, or that usage fell back below
/// the threshold since the last one — a recovery clears the debounce so
/// the next crossing alerts immediately instead of waiting out a stale
/// six-hour cooldown from an incident that is already over.
#[derive(Debug, Default)]
pub struct DiskDebounce {
    last_emit_seconds: Option<f64>,
}

/// `_check_and_emit_disk_critical`, with the reading and the clock as
/// arguments.
///
/// **Operator-side only.** This deliberately does not reach customer
/// notifications: a customer cannot `fly volumes extend` SourceBox's
/// infrastructure, and routing platform state through their inbox was a
/// multi-tenant violation removed in May 2026. The channels that matter
/// are `/api/health/detailed`, which any external monitor polls, and
/// Sentry, which is why the real loop logs this at ERROR — a warning
/// gets sampled away by default and never wakes anyone.
///
/// `db` is in Python's signature and unused there for the same reason;
/// it is simply absent here.
pub fn check_disk_critical(
    debounce: &mut DiskDebounce,
    path: &str,
    total: u64,
    free: u64,
    used: u64,
    now_seconds: f64,
) -> Option<DiskAlert> {
    if total == 0 {
        return None;
    }
    let pct = (used as f64 / total as f64) * 100.0;

    if pct < DISK_CRITICAL_THRESHOLD_PERCENT {
        debounce.last_emit_seconds = None;
        return None;
    }

    if let Some(last) = debounce.last_emit_seconds {
        if now_seconds - last < DISK_CRITICAL_REEMIT_INTERVAL_SECONDS {
            return None;
        }
    }

    debounce.last_emit_seconds = Some(now_seconds);
    Some(DiskAlert {
        percent_used: crate::pyrepr::round_to(pct, 1),
        bytes_free: free,
        path: path.to_string(),
    })
}

/// What one digest tick did, per anchor.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DigestSummary {
    pub anchors_seen: usize,
    /// Dropped without a digest: no colon in the key, an empty value,
    /// or a timestamp that would not parse.
    pub anchors_dropped: usize,
    /// Left in place because the window is still open.
    pub anchors_open: usize,
    pub digests_emitted: usize,
    /// Closed with nothing extra to report — the anchor still goes.
    pub anchors_closed_empty: usize,
    /// The anchor was re-armed while this tick worked, so it was left
    /// for the next one. See the note in `run_motion_digest`.
    pub anchors_rearmed: usize,
}

impl DigestSummary {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "anchors_seen": self.anchors_seen,
            "anchors_dropped": self.anchors_dropped,
            "anchors_open": self.anchors_open,
            "digests_emitted": self.digests_emitted,
            "anchors_closed_empty": self.anchors_closed_empty,
            "anchors_rearmed": self.anchors_rearmed,
        })
    }
}

/// `_motion_digest_loop`'s body — drain the expired motion cooldown
/// anchors and emit one digest per camera that had extras.
///
/// An anchor is a `Setting` row keyed `motion_email_cooldown_start:<camera_id>`,
/// written by the immediate-email path when it sends the FIRST alert for
/// a camera. Everything after that is silenced until the window closes;
/// this is what closes it, and what tells the operator how much they
/// missed.
///
/// Python's body is inline in the loop rather than extracted, so this is
/// the one place the port has no `run_*` counterpart to diff against
/// directly — the probe drives the real loop for exactly one tick
/// instead. See `py_loops_probe.py`.
pub async fn run_motion_digest(state: &AppState) -> Result<DigestSummary, sqlx::Error> {
    let now = crate::models::now_naive();
    let mut summary = DigestSummary::default();

    let anchors: Vec<(i32, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, org_id, key, value FROM settings
          WHERE key LIKE 'motion_email_cooldown_start:%'",
    )
    .fetch_all(&state.pool)
    .await?;
    summary.anchors_seen = anchors.len();

    for (id, org_id, key, value) in anchors {
        // Each anchor is handled independently: Python wraps the body in
        // its own try/except so one corrupt row cannot poison the tick,
        // and the loop keeps going rather than leaving the rest of the
        // cameras silenced until someone notices.
        let Some((_, camera_id)) = key.split_once(':') else {
            // A key that matched the LIKE but holds no colon cannot
            // name a camera. Drop it rather than carry it forever.
            drop_anchor(state, id).await?;
            summary.anchors_dropped += 1;
            continue;
        };
        let Some(anchor_value) = value.filter(|v| !v.is_empty()) else {
            drop_anchor(state, id).await?;
            summary.anchors_dropped += 1;
            continue;
        };
        // The anchor is written naive by the immediate-email path, so
        // the naive half is the one to compare against `now()`. A value
        // carrying an offset would still parse; Python compares the
        // naive datetime too, and would raise on a mixed comparison
        // rather than convert.
        let Ok(anchor_ts) = crate::pydatetime::fromisoformat(&anchor_value).map(|t| t.naive)
        else {
            // Corrupt timestamp. Dropped so the next motion event starts
            // a fresh window, rather than the camera being silenced
            // forever by a value nothing can parse.
            drop_anchor(state, id).await?;
            summary.anchors_dropped += 1;
            continue;
        };

        let cooldown_min = crate::notifications::motion_cooldown_minutes(&state.pool, &org_id).await;
        if (now - anchor_ts).num_seconds() < cooldown_min * 60 {
            summary.anchors_open += 1;
            continue;
        }

        let window_end = anchor_ts + chrono::Duration::minutes(cooldown_min);
        // Strictly AFTER the anchor: the immediate email already covered
        // the event at anchor time. The upper bound is defensive —
        // events past `window_end` belong to a later cycle, and no later
        // cycle exists yet because this anchor is still here.
        let (extra_count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM motion_events
              WHERE org_id = $1 AND camera_id = $2 AND timestamp > $3 AND timestamp <= $4",
        )
        .bind(&org_id)
        .bind(camera_id)
        .bind(anchor_ts)
        .bind(window_end)
        .fetch_one(&state.pool)
        .await?;

        let mut emitted = false;
        if extra_count > 0
            && crate::notifications::email_enabled(&state.config, &state.pool, &org_id, "motion")
                .await
        {
            // Re-resolved at emit time: the camera may have been renamed
            // since the immediate email, and the digest should say what
            // it is called now.
            let (display,): (String,) = sqlx::query_as(
                "SELECT COALESCE(NULLIF(name, ''), $2) FROM cameras
                  WHERE camera_id = $2 AND org_id = $1",
            )
            .bind(&org_id)
            .bind(camera_id)
            .fetch_optional(&state.pool)
            .await?
            .unwrap_or_else(|| (camera_id.to_string(),));

            let plural = if extra_count != 1 { "s" } else { "" };
            let were = if extra_count != 1 { "s were" } else { " was" };
            let mut notification = crate::notifications::NewNotification::new(
                "motion_digest",
                format!("{extra_count} more motion event{plural} on {display}"),
            )
            .body(format!(
                "{extra_count} additional motion event{were} detected on \"{display}\" \
                 in the {cooldown_min}-minute window after the first alert."
            ))
            .severity("info")
            .audience("all")
            .link(format!("/dashboard?camera={camera_id}"))
            .camera(camera_id);
            notification.meta = Some(serde_json::json!({
                "event_count": extra_count,
                "window_start": crate::models::iso_naive(anchor_ts),
                "window_end": crate::models::iso_naive(window_end),
                "cooldown_minutes": cooldown_min,
            }));
            crate::notifications::create_notification(state, &org_id, notification).await;
            emitted = true;
            summary.digests_emitted += 1;
        }

        // Delete the anchor — the window has closed — but ONLY if it
        // still holds the value this tick processed. The counting and
        // emitting above take real time (a plan lookup, an outbox
        // commit); a motion event landing in that gap sees the same
        // expired anchor, sends its own immediate email, and re-arms the
        // row with a fresh timestamp. An unconditional delete would
        // erase that brand-new window, so the NEXT event would email
        // immediately again — double immediate emails on exactly the
        // busy cameras digests exist for.
        let removed = sqlx::query("DELETE FROM settings WHERE id = $1 AND value = $2")
            .bind(id)
            .bind(&anchor_value)
            .execute(&state.pool)
            .await?
            .rows_affected();
        if removed == 0 {
            summary.anchors_rearmed += 1;
        } else if !emitted {
            summary.anchors_closed_empty += 1;
        }
    }

    Ok(summary)
}

async fn drop_anchor(state: &AppState, id: i32) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM settings WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(())
}

/// What one plan reconcile corrected.
///
/// Python returns a bare `int` — the count — so `changed` is the only
/// field with a counterpart to diff against. The rest feeds the port's
/// log line, which is worth more than the count alone when someone is
/// reading it at three in the morning wondering which org moved.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileSummary {
    pub checked: usize,
    pub changed: usize,
    /// `(org_id, cached, live)` per correction, for the log line.
    pub corrections: Vec<(String, String, String)>,
}

impl ReconcileSummary {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "checked": self.checked,
            "changed": self.changed,
            "corrections": self.corrections.iter()
                .map(|(org, cached, live)| serde_json::json!([org, cached, live]))
                .collect::<Vec<_>>(),
        })
    }
}

/// `_reconcile_org_plans` — re-verify every PAID cached plan against
/// Clerk, hourly.
///
/// The gap this closes: the webhook handler is the only path that ever
/// writes free over a paid `org_plan`, and `resolve_org_plan`'s live
/// fallback only fires when the cached slug is NOT paid. So a single
/// missed cancellation — the endpoint down past Svix's retry window, a
/// rotated secret, an out-of-order redelivery rewriting an old snapshot
/// — left an org on Pro caps forever, free of charge. Nothing else in
/// the system would ever notice.
///
/// Corrects in BOTH directions, which also rescues a scheduled
/// downgrade that the `.ended` handler defaulted to free on a failed
/// lookup.
///
/// `live is None` is a SKIP, not a downgrade. An unreachable Clerk must
/// not cost a paying customer their plan — the whole sweep exists
/// because a missing answer was treated as an answer once already.
pub async fn reconcile_org_plans(state: &AppState) -> Result<ReconcileSummary, sqlx::Error> {
    let paid: Vec<(String, String)> = sqlx::query_as(
        "SELECT org_id, value FROM settings WHERE key = 'org_plan' AND value = ANY($1)",
    )
    .bind(&crate::plans::PAID_PLAN_SLUGS[..])
    .fetch_all(&state.pool)
    .await?;

    let mut summary = ReconcileSummary { checked: paid.len(), ..Default::default() };
    for (org_id, cached) in paid {
        let live = crate::plans::fetch_live_plan_slug(
            &state.http,
            &state.config.clerk_api_url,
            &state.config.clerk_secret_key,
            &org_id,
        )
        .await;
        let Some(live) = live.filter(|slug| *slug != cached) else {
            continue;
        };
        tracing::warn!(
            org = %org_id, cached = %cached, live = %live,
            "[PlanReconcile] cached plan disagrees with Clerk — correcting"
        );
        crate::plans::invalidate_effective_plan_cache(Some(&org_id));
        crate::settings::set(&state.pool, &org_id, "org_plan", &live).await.ok();
        crate::api::clerk_webhook::set_org_member_limit(
            state,
            &org_id,
            crate::api::clerk_webhook::plan_member_limit(&live),
        )
        .await;
        // The cap runs AFTER the setting is written, because it reads
        // the plan back: enforcing before the write would apply the cap
        // the org is leaving rather than the one it is arriving at.
        let ctx = crate::plans::PlanContext {
            pool: &state.pool,
            client: &state.http,
            clerk_base_url: &state.config.clerk_api_url,
            clerk_secret: &state.config.clerk_secret_key,
            local_auth: state.config.is_local_auth(),
        };
        crate::plans::enforce_camera_cap(&ctx, &state.pool, &org_id).await.ok();
        summary.changed += 1;
        summary.corrections.push((org_id, cached, live));
    }

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

    /// The reconcile's summary carries more than Python's int return,
    /// so the differential compares only `changed`. Its shape is
    /// pinned here instead of nowhere.
    #[test]
    fn a_reconcile_summary_names_both_sides_of_each_correction() {
        let summary = ReconcileSummary {
            checked: 3,
            changed: 1,
            corrections: vec![("org-a".into(), "pro".into(), "free_org".into())],
        };
        let json = summary.to_json();
        assert_eq!(json["checked"], 3);
        assert_eq!(json["changed"], 1);
        // Cached first, live second. Inverted, the log line says an org
        // moved the opposite way — and this is the sweep whose entire
        // purpose is telling those two apart.
        assert_eq!(json["corrections"][0][1], "pro");
        assert_eq!(json["corrections"][0][2], "free_org");
        assert_eq!(ReconcileSummary::default().to_json()["corrections"], serde_json::json!([]));
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
