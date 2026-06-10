"""End-to-end tests for ``app.main.run_log_cleanup`` — the actual deletion
path the nightly cleanup loop runs.

Why this exists alongside ``test_log_cleanup_union.py``:
  - ``test_log_cleanup_union`` pins the SQLAlchemy 2.x ``union(a, b, c, ...)``
    function form so the production Sentry bug (OPENSENTRY-COMMAND-1) can't
    silently come back.
  - This file exercises the *whole* function: per-org plan resolution → cutoff
    computation → DELETE on each of the five log tables → commit → returned
    summary. The Sentry bug shipped because the cleanup-loop body was wrapped
    in ``except Exception: logger.exception(...)`` and ran nightly with no
    test coverage. Test the rarely-run paths.

The cleanup loop itself stays a thin scheduler around this function (see
``_log_cleanup_loop`` in app/main.py) — that's the same shape as
``run_offline_sweep`` / ``_offline_sweep_loop``, which has a full test
suite in ``test_notifications.py``.
"""

from datetime import UTC, datetime, timedelta, timezone

from app.core.database import SessionLocal
from app.main import run_log_cleanup
from app.models.models import (
    AuditLog,
    EmailLog,
    EmailOutbox,
    McpActivityLog,
    MotionEvent,
    Notification,
    Setting,
    StreamAccessLog,
)


def _utcnow_naive() -> datetime:
    """Match the timestamp pattern the cleanup loop uses."""
    return datetime.now(tz=UTC).replace(tzinfo=None)


def _seed_logs_at(db, *, org_id: str, when: datetime) -> None:
    """Insert one row in each of the six per-org log tables for
    ``org_id`` at ``when``. Using all of them every time keeps each
    test exercising the whole UNION + DELETE matrix rather than a
    single column."""
    db.add(StreamAccessLog(
        org_id=org_id, user_id="u1", camera_id="cam_1", node_id="node_1",
        ip_address="127.0.0.1", user_agent="t", accessed_at=when,
    ))
    db.add(McpActivityLog(
        org_id=org_id, tool_name="list_cameras", key_name="k",
        status="ok", duration_ms=1, timestamp=when,
    ))
    db.add(AuditLog(
        org_id=org_id, event="evt", user_id="u1",
        ip_address="127.0.0.1", details="{}", timestamp=when,
    ))
    db.add(MotionEvent(
        org_id=org_id, camera_id="cam_1", node_id="node_1",
        score=80, segment_seq=1, timestamp=when,
    ))
    db.add(Notification(
        org_id=org_id, kind="motion", audience="all",
        title="t", body="b", severity="info", created_at=when,
    ))
    db.add(EmailLog(
        org_id=org_id, recipient_email="x@y.test", kind="camera_offline",
        status="sent", timestamp=when,
    ))


# ── Tests ────────────────────────────────────────────────────────────


def test_run_log_cleanup_deletes_rows_past_free_tier_retention():
    """Free-tier org: rows older than 30 days are deleted; rows newer than
    30 days survive. Confirms the per-tier cutoff actually fires DELETE
    statements and not just the UNION query.
    """
    db = SessionLocal()
    try:
        now = _utcnow_naive()
        old = now - timedelta(days=45)   # past 30-day Free retention
        recent = now - timedelta(days=5)  # within retention

        _seed_logs_at(db, org_id="org_free", when=old)
        _seed_logs_at(db, org_id="org_free", when=recent)
        db.commit()

        # No org_plan setting → resolve_org_plan falls through to Clerk,
        # the fake Clerk key fails, the except returns "free_org" — which
        # is exactly the production behaviour for a free-tier org. Free
        # tier retention is 30 days (PLAN_LIMITS["free_org"]).
        summary = run_log_cleanup(db)

        # Six per-org tables × one old row each = 6 deletions; 6
        # recent rows survive.  No EmailOutbox seeded so that
        # cleanup contributes 0 here (covered separately below).
        assert summary["orgs_processed"] == 1
        assert summary["total_deleted"] == 6
        assert summary["totals"] == {
            "stream": 1, "mcp": 1, "audit": 1, "motion": 1, "notif": 1,
            "email_log": 1, "email_outbox": 0, "processed_webhooks": 0,
        }

        # Spot-check that the recent rows weren't touched.
        assert db.query(StreamAccessLog).filter_by(org_id="org_free").count() == 1
        assert db.query(McpActivityLog).filter_by(org_id="org_free").count() == 1
        assert db.query(AuditLog).filter_by(org_id="org_free").count() == 1
        assert db.query(MotionEvent).filter_by(org_id="org_free").count() == 1
        assert db.query(Notification).filter_by(org_id="org_free").count() == 1
        assert db.query(EmailLog).filter_by(org_id="org_free").count() == 1
    finally:
        db.close()


def test_run_log_cleanup_respects_pro_plus_longer_retention():
    """Pro Plus retention is 365 days — rows that a Free org would lose
    must still be kept for a Pro Plus org. Pins the per-tier divergence
    so a future PR that hard-codes ``LOG_RETENTION_DAYS`` everywhere
    fails the test.
    """
    db = SessionLocal()
    try:
        now = _utcnow_naive()
        # 60 days back: past Free's 30d cutoff, well within Pro Plus's 365d.
        sixty_days_ago = now - timedelta(days=60)

        # Seeding org_plan="pro_plus" is a recognised paid slug, so
        # resolve_org_plan short-circuits on the cached Setting and never
        # tries to call Clerk — the test stays deterministic regardless
        # of throttle state from earlier tests in the same session.
        Setting.set(db, "org_pp", "org_plan", "pro_plus")
        _seed_logs_at(db, org_id="org_pp", when=sixty_days_ago)
        db.commit()

        summary = run_log_cleanup(db)

        assert summary["orgs_processed"] == 1
        assert summary["total_deleted"] == 0, (
            "60-day-old rows must survive on Pro Plus (365d retention)"
        )

        # All five rows still present.
        assert db.query(StreamAccessLog).filter_by(org_id="org_pp").count() == 1
        assert db.query(McpActivityLog).filter_by(org_id="org_pp").count() == 1
        assert db.query(AuditLog).filter_by(org_id="org_pp").count() == 1
        assert db.query(MotionEvent).filter_by(org_id="org_pp").count() == 1
        assert db.query(Notification).filter_by(org_id="org_pp").count() == 1
        assert db.query(EmailLog).filter_by(org_id="org_pp").count() == 1
    finally:
        db.close()


def test_run_log_cleanup_isolates_orgs_by_plan():
    """Two orgs, same row age, different tiers: Free org loses the row,
    Pro org keeps it. Confirms the per-org loop applies the right cutoff
    to the right org_id rather than using one global cutoff.
    """
    db = SessionLocal()
    try:
        now = _utcnow_naive()
        # 45 days back: past Free's 30d, within Pro's 90d.
        forty_five_days_ago = now - timedelta(days=45)

        Setting.set(db, "org_pro", "org_plan", "pro")
        _seed_logs_at(db, org_id="org_pro", when=forty_five_days_ago)
        _seed_logs_at(db, org_id="org_free", when=forty_five_days_ago)
        db.commit()

        summary = run_log_cleanup(db)

        assert summary["orgs_processed"] == 2
        # Only org_free's 6 per-org rows are past their 30d cutoff.
        assert summary["total_deleted"] == 6

        # org_pro untouched; org_free fully purged.
        assert db.query(StreamAccessLog).filter_by(org_id="org_pro").count() == 1
        assert db.query(StreamAccessLog).filter_by(org_id="org_free").count() == 0
        assert db.query(Notification).filter_by(org_id="org_pro").count() == 1
        assert db.query(Notification).filter_by(org_id="org_free").count() == 0
        assert db.query(EmailLog).filter_by(org_id="org_pro").count() == 1
        assert db.query(EmailLog).filter_by(org_id="org_free").count() == 0
    finally:
        db.close()


def test_run_log_cleanup_handles_empty_database():
    """Fresh deployment: no log rows yet. The cleanup must not crash —
    without this guard, a brand-new node's first nightly tick would
    fail silently (or, with the original chained-union bug, fail loudly
    inside the loop's try/except)."""
    db = SessionLocal()
    try:
        summary = run_log_cleanup(db)
        assert summary == {
            "orgs_processed": 0,
            "totals": {
                "stream": 0, "mcp": 0, "audit": 0, "motion": 0, "notif": 0,
                "email_log": 0, "email_outbox": 0, "processed_webhooks": 0,
            },
            "total_deleted": 0,
        }
    finally:
        db.close()


def test_run_log_cleanup_returns_summary_shape():
    """The summary dict is part of the contract: ``_log_cleanup_loop``
    reads ``total_deleted``, ``orgs_processed``, and the ``totals``
    sub-dict to format its log line. A test pinning the shape guards
    against accidental key renames that would break logging silently
    (no test in test_log_cleanup_union covers this — that file pins
    the SQL surface only)."""
    db = SessionLocal()
    try:
        # Seed enough variety for every key in `totals` to be exercised.
        now = _utcnow_naive()
        old = now - timedelta(days=45)
        _seed_logs_at(db, org_id="org_shape", when=old)
        db.commit()

        summary = run_log_cleanup(db)

        assert set(summary.keys()) == {"orgs_processed", "totals", "total_deleted"}
        assert set(summary["totals"].keys()) == {
            "stream", "mcp", "audit", "motion", "notif",
            "email_log", "email_outbox", "processed_webhooks",
        }
        assert isinstance(summary["orgs_processed"], int)
        assert isinstance(summary["total_deleted"], int)
        assert summary["total_deleted"] == sum(summary["totals"].values())
    finally:
        db.close()


# ── EmailOutbox cleanup (cross-org, terminal-state only) ────────────

def test_run_log_cleanup_deletes_old_terminal_outbox_rows():
    """EmailOutbox rows in 'sent', 'failed', or 'suppressed' status
    older than 7 days are deleted regardless of org-tier — the outbox
    is operationally a queue, not a per-org log, so it gets a fixed
    short window.  Long-term audit history lives in EmailLog (which
    uses the per-org tiered retention)."""
    db = SessionLocal()
    try:
        now = _utcnow_naive()
        old = now - timedelta(days=10)
        recent = now - timedelta(days=2)

        # Seed terminal-state old rows across multiple orgs to verify
        # cross-org scope.
        for org in ("org_a", "org_b"):
            for status in ("sent", "failed", "suppressed"):
                row = EmailOutbox(
                    org_id=org, recipient_email="x@y.test",
                    subject="x", body_text="t", body_html="<p>t</p>",
                    kind="camera_offline", status=status,
                )
                db.add(row)
        db.commit()
        # Backdate the created_at column post-insert (the model's
        # default fires on add; explicit assignment doesn't override
        # it cleanly otherwise).
        db.query(EmailOutbox).update({EmailOutbox.created_at: old})
        db.commit()

        # Plus a recent row that should NOT be deleted.
        recent_row = EmailOutbox(
            org_id="org_a", recipient_email="x@y.test",
            subject="x", body_text="t", body_html="<p>t</p>",
            kind="camera_offline", status="sent",
        )
        db.add(recent_row)
        db.commit()
        db.query(EmailOutbox).filter(EmailOutbox.id == recent_row.id).update(
            {EmailOutbox.created_at: recent}
        )
        db.commit()

        summary = run_log_cleanup(db)

        # 6 old terminal rows deleted (3 statuses × 2 orgs).
        assert summary["totals"]["email_outbox"] == 6
        # Recent row survives.
        assert db.query(EmailOutbox).count() == 1


    finally:
        db.close()


def test_run_log_cleanup_never_deletes_pending_or_sending_outbox_rows():
    """Even very old 'pending' / 'sending' rows must survive the
    cleanup — deleting them would silently lose an email mid-retry.
    Worth pinning explicitly because the obvious "by-age" filter
    would lose them."""
    db = SessionLocal()
    try:
        now = _utcnow_naive()
        ancient = now - timedelta(days=365)  # way past any cutoff

        for status in ("pending", "sending"):
            row = EmailOutbox(
                org_id="org_x", recipient_email="x@y.test",
                subject="x", body_text="t", body_html="<p>t</p>",
                kind="camera_offline", status=status,
            )
            db.add(row)
        db.commit()
        db.query(EmailOutbox).update({EmailOutbox.created_at: ancient})
        db.commit()

        summary = run_log_cleanup(db)

        assert summary["totals"]["email_outbox"] == 0
        # Both rows still present.
        assert db.query(EmailOutbox).filter_by(status="pending").count() == 1
        assert db.query(EmailOutbox).filter_by(status="sending").count() == 1
    finally:
        db.close()
