//! The outbox drain.
//!
//! Ported from `backend/app/core/email_worker.py`. `run_one_tick` is
//! the whole worker; the loop around it only sleeps. That split is
//! Python's and it is kept, because it is what lets a differential
//! drive the worker directly instead of racing a timer.
//!
//! Three things in here are easy to get subtly wrong:
//!
//! The tick is stamped at the START, so an empty outbox still counts as
//! "the loop is alive". Stamping on completion would make a healthy
//! worker with nothing to do look wedged to the health probe.
//!
//! The summary counts TERMINAL states. A row that fails twice and
//! succeeds on the third attempt is one `sent`, not two `failed` and a
//! `sent` spread over three ticks — so a mid-retry row, which
//! `finalize` has just flipped back to `pending`, is counted in
//! neither.
//!
//! And the log row is written only on a terminal outcome, for the same
//! reason: a row that eventually succeeds leaves one `sent` line in the
//! audit trail rather than three.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use chrono::NaiveDateTime;

use crate::config::Config;
use crate::email::send_email;
use crate::models::now_naive;

/// `_SENDING_RECLAIM_AGE_SECONDS`.
const SENDING_RECLAIM_AGE_SECONDS: i64 = 60;

/// Milliseconds since process start at the last tick, or 0 for "never".
///
/// Python keeps `time.monotonic()` in a module global; this is the same
/// thing in a form that can be read without a lock, because the health
/// probe reads it on every request.
static LAST_TICK_MILLIS: AtomicU64 = AtomicU64::new(0);

fn process_start() -> Instant {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn stamp_tick() {
    let millis = process_start().elapsed().as_millis() as u64;
    // Zero means "never ticked", so a tick landing in the first
    // millisecond of the process is reported as one millisecond in.
    LAST_TICK_MILLIS.store(millis.max(1), Ordering::Relaxed);
}

/// `seconds_since_last_tick()` — None when the worker has never run.
pub fn seconds_since_last_tick() -> Option<f64> {
    let stamped = LAST_TICK_MILLIS.load(Ordering::Relaxed);
    if stamped == 0 {
        return None;
    }
    let now = process_start().elapsed().as_millis() as u64;
    Some((now.saturating_sub(stamped)) as f64 / 1000.0)
}

/// Test-only: forget that the worker ever ticked.
pub fn reset_tick_for_tests() {
    LAST_TICK_MILLIS.store(0, Ordering::Relaxed);
}

/// What a tick needs, which is less than an `AppState`.
///
/// Split out so the differential probe can build one from a pool, a
/// config and a client — constructing a whole `AppState` in an example
/// would mean a JWKS cache, a proxy client and the segment caches, none
/// of which the worker touches.
pub struct EmailContext<'a> {
    pub pool: &'a sqlx::PgPool,
    pub config: &'a Config,
    pub client: &'a reqwest::Client,
}

#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct TickSummary {
    pub sent: i64,
    pub failed: i64,
    pub suppressed: i64,
    pub reclaimed: i64,
}

#[derive(sqlx::FromRow)]
struct OutboxRow {
    id: i32,
    org_id: String,
    recipient_email: String,
    subject: String,
    // All three are NOT NULL in the schema.
    body_text: String,
    body_html: String,
    kind: String,
    attempts: i32,
}

/// `run_one_tick(db)` — drain one batch.
pub async fn run_one_tick(ctx: &EmailContext<'_>) -> Result<TickSummary, sqlx::Error> {
    // Stamped first: an empty outbox returns early below, and a healthy
    // worker with nothing to send must not read as wedged.
    stamp_tick();

    let mut summary = TickSummary::default();

    // A worker that died mid-send leaves a row wedged in `sending`
    // forever. The idempotency key on the send is what makes putting it
    // back safe — if the original attempt did land, Resend answers with
    // the first message rather than delivering a second.
    let cutoff = now_naive() - chrono::Duration::seconds(SENDING_RECLAIM_AGE_SECONDS);
    let reclaimed = sqlx::query(
        "UPDATE email_outbox SET status = 'pending'
          WHERE status = 'sending' AND last_attempt_at < $1",
    )
    .bind(cutoff)
    .execute(ctx.pool)
    .await?
    .rows_affected() as i64;
    if reclaimed > 0 {
        summary.reclaimed = reclaimed;
        tracing::info!(reclaimed, "reclaimed stuck 'sending' rows");
    }

    let pending: Vec<OutboxRow> = sqlx::query_as(
        "SELECT id, org_id, recipient_email, subject, body_text, body_html, kind, attempts
           FROM email_outbox
          WHERE status = 'pending'
          ORDER BY created_at
          LIMIT $1",
    )
    .bind(ctx.config.email_worker_batch_size)
    .fetch_all(ctx.pool)
    .await?;
    if pending.is_empty() {
        return Ok(summary);
    }

    let now = now_naive();
    let ids: Vec<i32> = pending.iter().map(|row| row.id).collect();
    sqlx::query("UPDATE email_outbox SET status = 'sending', last_attempt_at = $1 WHERE id = ANY($2)")
        .bind(now)
        .bind(&ids)
        .execute(ctx.pool)
        .await?;

    for row in &pending {
        let (status, message_id, error) = process_row(ctx, row).await;
        let terminal = finalize_row(ctx, row, &status, message_id.as_deref(), error.as_deref())
            .await?;
        write_log(ctx, row, &status, message_id.as_deref(), error.as_deref(), &terminal)
            .await;
        match terminal.as_str() {
            "sent" => summary.sent += 1,
            "failed" => summary.failed += 1,
            "suppressed" => summary.suppressed += 1,
            // `pending` — a retry. Counted in the tick it finally
            // settles in, not this one.
            _ => {}
        }
    }

    Ok(summary)
}

/// The outcome for one row, before anything is persisted.
async fn process_row(
    ctx: &EmailContext<'_>,
    row: &OutboxRow,
) -> (String, Option<String>, Option<String>) {
    // Checked before the API call: it saves a round trip, and
    // re-sending to an address Resend already suppressed is what dings
    // a sender's reputation.
    let suppressed: Option<(Option<String>,)> =
        sqlx::query_as("SELECT reason FROM email_suppression WHERE address = $1 LIMIT 1")
            .bind(row.recipient_email.to_lowercase())
            .fetch_optional(ctx.pool)
            .await
            .unwrap_or(None);
    if let Some((reason,)) = suppressed {
        return (
            "suppressed".to_string(),
            None,
            Some(format!(
                "address_suppressed: reason={}",
                // The column is NOT NULL, so this fallback is
                // unreachable — kept because the row is read as
                // Option and inventing an unwrap here would be the
                // one place this file could panic.
                reason.unwrap_or_else(|| "None".to_string())
            )),
        );
    }

    // Derived from the row id, NOT the attempt count: every retry of a
    // row must share one key, or the idempotency buys nothing.
    let idempotency_key = format!("outbox-{}", row.id);
    let result = send_email(
        ctx.config,
        ctx.client,
        &crate::email::OutgoingEmail {
            to: &row.recipient_email,
            subject: &row.subject,
            body_text: &row.body_text,
            body_html: &row.body_html,
            kind: &row.kind,
            idempotency_key: &idempotency_key,
        },
    )
    .await;

    if result.skipped {
        // The switch is off. The row leaves the outbox as though it
        // were sent, with no Resend id — so the log shows an operator
        // what would have happened.
        return ("sent".to_string(), None, None);
    }
    if result.ok {
        return ("sent".to_string(), result.message_id, None);
    }
    ("failed".to_string(), None, result.error)
}

/// Persist the outcome, and return the state the row ENDED in — which
/// is not the outcome when a failure still has attempts left.
async fn finalize_row(
    ctx: &EmailContext<'_>,
    row: &OutboxRow,
    status: &str,
    message_id: Option<&str>,
    error: Option<&str>,
) -> Result<String, sqlx::Error> {
    let attempts = row.attempts + 1;
    let terminal = match status {
        "sent" => {
            sqlx::query(
                "UPDATE email_outbox
                    SET attempts = $1, status = 'sent', sent_at = $2,
                        resend_message_id = $3, error = NULL
                  WHERE id = $4",
            )
            .bind(attempts)
            .bind(now_naive())
            .bind(message_id)
            .bind(row.id)
            .execute(ctx.pool)
            .await?;
            "sent"
        }
        "suppressed" => {
            sqlx::query(
                "UPDATE email_outbox SET attempts = $1, status = 'suppressed', error = $2
                  WHERE id = $3",
            )
            .bind(attempts)
            .bind(error)
            .bind(row.id)
            .execute(ctx.pool)
            .await?;
            "suppressed"
        }
        _ => {
            // At the cap it stays failed and the worker stops trying;
            // below it, back to pending for the next tick.
            let give_up = i64::from(attempts) >= ctx.config.email_max_attempts;
            let next = if give_up { "failed" } else { "pending" };
            if give_up {
                tracing::warn!(id = row.id, attempts, error, "giving up on outbox row");
            }
            sqlx::query("UPDATE email_outbox SET attempts = $1, status = $2, error = $3 WHERE id = $4")
                .bind(attempts)
                .bind(next)
                .bind(error)
                .bind(row.id)
                .execute(ctx.pool)
                .await?;
            next
        }
    };
    Ok(terminal.to_string())
}

/// Append an `EmailLog` row, but only for an outcome that settled.
async fn write_log(
    ctx: &EmailContext<'_>,
    row: &OutboxRow,
    status: &str,
    message_id: Option<&str>,
    error: Option<&str>,
    terminal: &str,
) {
    // Mid-retry: the row went back to pending, so nothing is logged and
    // the audit trail stays readable.
    if status == "failed" && terminal == "pending" {
        return;
    }
    let inserted = sqlx::query(
        "INSERT INTO email_log
            (org_id, recipient_email, kind, status, resend_message_id, error, timestamp)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&row.org_id)
    .bind(&row.recipient_email)
    .bind(&row.kind)
    .bind(status)
    .bind(message_id)
    .bind(error)
    .bind(now_naive())
    .execute(ctx.pool)
    .await;
    if let Err(err) = inserted {
        // An audit write must never block the worker.
        tracing::error!(error = %err, id = row.id, "failed to write EmailLog");
    }
}

/// The loop `main.py` spawns. It sleeps and calls the tick; everything
/// that matters is in `run_one_tick`.
pub async fn email_worker_loop(state: crate::app::AppState) {
    let interval = state.config.email_worker_interval_seconds.max(1);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        let ctx = EmailContext {
            pool: &state.pool,
            config: &state.config,
            client: &state.http,
        };
        match run_one_tick(&ctx).await {
            Ok(summary) => {
                if summary.sent + summary.failed + summary.suppressed + summary.reclaimed > 0 {
                    tracing::info!(
                        sent = summary.sent,
                        failed = summary.failed,
                        suppressed = summary.suppressed,
                        reclaimed = summary.reclaimed,
                        "email worker tick"
                    );
                }
            }
            Err(err) => tracing::error!(error = %err, "email worker tick failed"),
        }
    }
}

/// Timestamps the differential needs to compare are naive UTC, like
/// every other column in this schema.
#[allow(dead_code)]
fn assert_naive(_: NaiveDateTime) {}
