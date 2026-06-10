"""
Email worker — drains the EmailOutbox via Resend.

Spawned in ``app.main.lifespan`` as a fire-and-forget asyncio task.
Every ``EMAIL_WORKER_INTERVAL_SECONDS`` seconds:

  1. Reclaim 'sending' rows older than 60s (worker died mid-flight).
  2. SELECT N pending rows ordered by created_at.
  3. Mark each 'sending' (lock to prevent concurrent workers picking
     the same row — we run single-instance today, but the lock is
     cheap and future-proofs scale-up).
  4. For each row:
       - Skip if recipient address is in EmailSuppression → mark 'suppressed'.
       - Else call ``email.send_email`` → mark 'sent' or 'failed'.
       - Bump ``attempts``; on attempts >= MAX, mark 'failed' permanently.
  5. Write an EmailLog row for every outcome (audit trail).

The worker is the *only* thing that writes to ``status`` after the
initial 'pending' insert.  Everything else (callers, webhook handler)
just enqueues or queries — no other code path mutates outbox state.

Tests drive the worker by calling ``run_one_tick(db)`` directly.  The
loop wrapper is just a thin scheduler around it.
"""

from __future__ import annotations

import asyncio
import logging
import time
from datetime import UTC, datetime, timedelta
from typing import Optional

from sqlalchemy.orm import Session

from app.core.config import settings
from app.core.database import SessionLocal
from app.core.email import EmailSendResult, send_email
from app.models.models import EmailLog, EmailOutbox, EmailSuppression

logger = logging.getLogger(__name__)


# Reclaim 'sending' rows older than this — covers a worker process
# crash mid-send.  Conservative because we'd rather email twice than
# zero times for a security alert.  Resend's idempotency-key header
# (set in core/email.py) protects against actual duplicate sends.
_SENDING_RECLAIM_AGE_SECONDS = 60


# ── Health-probe surface ────────────────────────────────────────────
# In-process timestamp updated on every successful tick.  The
# health-readiness endpoint reads ``seconds_since_last_tick()`` to
# detect a wedged worker — outbox emails sit forever waiting for
# a tick that's never coming.
#
# We use monotonic time (not wall clock) so a system clock jump
# doesn't make a healthy worker look stale.  ``None`` until the
# first tick completes, which is what lets the health endpoint
# distinguish "never ticked, give it a grace window" from "stopped
# ticking, page someone."
#
# Module-level not Setting-table because: (a) avoids hot DB write
# every 5s, (b) a wedged worker is a per-process question — if the
# worker died, the in-process value goes stale and the health
# endpoint sees it.  Persisting across restarts would actively
# hide a crash-loop ("oh look, it ticked 4 seconds ago — but that
# was the previous incarnation").
_last_tick_monotonic: Optional[float] = None


def seconds_since_last_tick() -> Optional[float]:
    """Return seconds since the worker last completed a tick, or
    ``None`` if it has never ticked in this process.

    Health-readiness consumer: pair the result with process uptime
    to apply a grace window — a brand-new process hasn't had time
    to tick yet, that's normal, not a wedged worker.
    """
    if _last_tick_monotonic is None:
        return None
    return time.monotonic() - _last_tick_monotonic


def _reset_tick_for_tests() -> None:
    """Clear the in-process tick timestamp.  Used by health-endpoint
    tests that need the "never ticked" code path; production code
    never calls this."""
    global _last_tick_monotonic
    _last_tick_monotonic = None


# ── Public API ───────────────────────────────────────────────────────

def run_one_tick(db: Session) -> dict:
    """Drain one batch from the outbox.  Returns a summary dict.

    Pure function over the session — no asyncio, no global state.
    Tests call this directly to drive the worker without waiting on
    the loop's sleep.

    Summary shape::

        {"sent": int, "failed": int, "suppressed": int, "reclaimed": int}
    """
    # Stamp the tick at the START so every entry counts as "loop is
    # alive," not just successful drain cycles.  An empty outbox
    # returns early below — without stamping here, a healthy worker
    # processing zero emails would look wedged to the health probe.
    # Crashes inside the function show up as DB / Resend probe
    # failures elsewhere; the worker probe is specifically about
    # "is the loop body still being scheduled."
    global _last_tick_monotonic
    _last_tick_monotonic = time.monotonic()

    summary = {"sent": 0, "failed": 0, "suppressed": 0, "reclaimed": 0}

    # ── Reclaim stuck 'sending' rows ──────────────────────────────
    # If a previous worker died mid-send, the row is wedged in
    # 'sending' forever.  Flip back to 'pending' so the next pass
    # picks it up.  Idempotency-key on the Resend send protects
    # against duplicate delivery if the original send actually
    # succeeded before the crash.
    reclaim_cutoff = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(
        seconds=_SENDING_RECLAIM_AGE_SECONDS
    )
    reclaimed = (
        db.query(EmailOutbox)
        .filter(
            EmailOutbox.status == "sending",
            EmailOutbox.last_attempt_at < reclaim_cutoff,
        )
        .update({"status": "pending"}, synchronize_session=False)
    )
    if reclaimed:
        summary["reclaimed"] = reclaimed
        logger.info("[EmailWorker] reclaimed %d stuck 'sending' rows", reclaimed)
        db.commit()

    # ── Claim a batch ─────────────────────────────────────────────
    pending = (
        db.query(EmailOutbox)
        .filter(EmailOutbox.status == "pending")
        .order_by(EmailOutbox.created_at)
        .limit(settings.EMAIL_WORKER_BATCH_SIZE)
        .all()
    )
    if not pending:
        return summary

    now = datetime.now(tz=UTC).replace(tzinfo=None)
    for row in pending:
        row.status = "sending"
        row.last_attempt_at = now
    db.commit()

    # ── Process each row ──────────────────────────────────────────
    for row in pending:
        try:
            outcome = _process_row(db, row)
        except Exception as exc:  # noqa: BLE001 — never let one row kill the tick
            logger.exception(
                "[EmailWorker] unexpected error processing outbox row id=%s", row.id,
            )
            outcome = ("failed", None, f"worker_exception: {type(exc).__name__}: {exc}")

        status, message_id, error = outcome
        _finalize_row(db, row, status, message_id, error)
        _write_log(db, row, status, message_id, error)
        # Count by TERMINAL state, not per-attempt outcome.  A row
        # that fails twice and succeeds on the third attempt should
        # show up as sent=1 in the tick that actually succeeded, not
        # failed=2 sent=1 across three ticks.  Mid-retry rows
        # (status flipped back to 'pending' by _finalize_row) are
        # uncounted — they'll show up in the bucket they end up in
        # eventually.
        terminal = row.status
        if terminal in ("sent", "failed", "suppressed"):
            summary[terminal] = summary.get(terminal, 0) + 1

    db.commit()
    return summary


# ── Worker loop ──────────────────────────────────────────────────────

async def email_worker_loop():
    """Background loop spawned in main.py lifespan.

    Wakes every ``EMAIL_WORKER_INTERVAL_SECONDS`` seconds and calls
    ``run_one_tick``.  Sleeps cooperatively so cancellation is
    immediate on shutdown.

    Each iteration opens its own SessionLocal — same pattern the
    other background loops follow (``_log_cleanup_loop``,
    ``_offline_sweep_loop``).  Per-tick session means a SQL error in
    one tick doesn't poison the next.
    """
    interval = max(1, settings.EMAIL_WORKER_INTERVAL_SECONDS)
    while True:
        try:
            await asyncio.sleep(interval)
        except asyncio.CancelledError:
            return

        try:
            db = SessionLocal()
            try:
                # to_thread: run_one_tick does up to EMAIL_WORKER_BATCH_SIZE
                # sequential sync Resend HTTPS calls — inline, that froze
                # the event loop 2-10s per tick whenever email was flowing
                # (worst during a node-offline storm, exactly when
                # operators are watching streams).
                summary = await asyncio.to_thread(run_one_tick, db)
            finally:
                db.close()
        except Exception:
            # The whole tick failed (probably DB connection issue).
            # Log and keep looping — we don't want one bad tick to
            # take down email forever.
            logger.exception("[EmailWorker] tick failed")
            continue

        if any(summary.values()):
            logger.info(
                "[EmailWorker] tick: sent=%d failed=%d suppressed=%d reclaimed=%d",
                summary.get("sent", 0),
                summary.get("failed", 0),
                summary.get("suppressed", 0),
                summary.get("reclaimed", 0),
            )


# ── Internals ────────────────────────────────────────────────────────

def _process_row(
    db: Session, row: EmailOutbox
) -> tuple[str, Optional[str], Optional[str]]:
    """Decide the outcome for one outbox row.

    Returns ``(status, resend_message_id, error)``.  ``status`` is one
    of ``"sent"``, ``"failed"``, or ``"suppressed"``.  Caller is
    responsible for persisting the result via ``_finalize_row``.
    """
    # Suppression check — short-circuit before the API call.  Saves a
    # round-trip and prevents accidentally re-suppressing an address
    # Resend already knows about (which dings deliverability rep).
    suppressed = (
        db.query(EmailSuppression)
        .filter(EmailSuppression.address == row.recipient_email.lower())
        .first()
    )
    if suppressed:
        return ("suppressed", None, f"address_suppressed: reason={suppressed.reason}")

    # Idempotency key derived from the row id ensures a retry of the
    # SAME outbox row doesn't double-send if the previous attempt
    # actually succeeded but the response was lost.  Including the
    # attempt count would defeat that — we want all retries of a row
    # to share one key.
    idem = f"outbox-{row.id}"

    result: EmailSendResult = send_email(
        to=row.recipient_email,
        subject=row.subject,
        body_text=row.body_text,
        body_html=row.body_html,
        kind=row.kind,
        idempotency_key=idem,
    )

    if result.skipped:
        # EMAIL_ENABLED=false — pretend we sent so the row leaves the
        # outbox, but don't record a Resend id.  Surfaced as 'sent' in
        # the log so the operator sees what would have happened.
        return ("sent", None, None)

    if result.ok:
        return ("sent", result.message_id, None)

    return ("failed", None, result.error)


def _finalize_row(
    db: Session,
    row: EmailOutbox,
    status: str,
    message_id: Optional[str],
    error: Optional[str],
) -> None:
    """Persist the outcome onto the outbox row.

    Retry logic lives here: a 'failed' row with attempts < MAX flips
    back to 'pending' for the next tick.  At MAX, it stays 'failed'
    permanently and the worker stops trying.
    """
    row.attempts = (row.attempts or 0) + 1

    if status == "sent":
        row.status = "sent"
        row.sent_at = datetime.now(tz=UTC).replace(tzinfo=None)
        row.resend_message_id = message_id
        row.error = None
    elif status == "suppressed":
        row.status = "suppressed"
        row.error = error
    else:
        # Failed: decide whether to retry or give up.
        if row.attempts >= settings.EMAIL_MAX_ATTEMPTS:
            row.status = "failed"
            row.error = error
            logger.warning(
                "[EmailWorker] giving up on outbox id=%s after %d attempts: %s",
                row.id, row.attempts, error,
            )
        else:
            # Back to pending — next tick picks it up.
            row.status = "pending"
            row.error = error


def _write_log(
    db: Session,
    row: EmailOutbox,
    status: str,
    message_id: Optional[str],
    error: Optional[str],
) -> None:
    """Append an EmailLog row.  Logs the outcome we settled on, not
    the transient retry state — so a row that eventually succeeds
    after 2 failures shows up as one 'sent' log, not three rows.

    We only write the log on terminal states ('sent', 'suppressed',
    or 'failed' at max attempts).  Mid-retry failures stay invisible
    in the log to keep the audit trail readable.
    """
    if status == "failed" and row.status == "pending":
        # Mid-retry, not terminal — skip the log row to keep noise down.
        return

    try:
        log_row = EmailLog(
            org_id=row.org_id,
            recipient_email=row.recipient_email,
            kind=row.kind,
            status=status,
            resend_message_id=message_id,
            error=error,
        )
        db.add(log_row)
    except Exception:
        # Audit-write failure must never block the worker.
        logger.exception(
            "[EmailWorker] failed to write EmailLog for outbox id=%s", row.id,
        )
