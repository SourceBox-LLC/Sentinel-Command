"""
Tests for the motion-email cooldown gate (v1.1).

Covers the per-camera anchor mechanism in
``app/api/notifications.py::_claim_motion_cooldown_or_silence`` plus
the kind-aware dispatch in ``create_notification`` that uses it.
The companion digest-loop tests live in ``test_motion_digest_loop.py``.

Key invariants pinned here:
  * The first motion event per camera per cooldown window emails
    immediately AND writes a per-camera anchor.
  * Subsequent in-window events still write inbox + SSE rows but
    skip the email outbox.
  * Anchor expiry resumes immediate emails (and overwrites the anchor).
  * Email-disabled paths NEVER write an anchor (so flipping the toggle
    on later starts a clean window).
  * Inbox-disabled (``motion_notifications=false``) suppresses the
    inbox row + SSE only — the email side-channel is an INDEPENDENT
    toggle and still emails (anchor included) when ``email_motion``
    is on.  Both toggles off → nothing anywhere.
  * Default state for ``email_motion`` is OFF (deliberate inversion
    of every other email kind — see comment block in
    ``_EMAIL_KIND_TO_SETTING``).
  * Malformed anchor values recover gracefully on the next event.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta, timezone

import pytest

from app.api import notifications as notifications_mod
from app.api.notifications import create_notification
from app.models.models import EmailOutbox, Notification, Setting

# ── Fixtures ────────────────────────────────────────────────────────


@pytest.fixture
def stub_recipients(monkeypatch):
    """Bypass Clerk lookup — return one admin email for every call."""
    class Stub:
        def __init__(self):
            self.return_value: list[str] = ["alice@org.test"]
            self.calls: list[tuple[str, str]] = []

        def __call__(self, org_id, audience):
            self.calls.append((org_id, audience))
            return list(self.return_value)

    stub = Stub()
    monkeypatch.setattr(notifications_mod, "get_recipient_emails", stub)
    return stub


def _enable_motion_email(db, monkeypatch):
    """Helper — flip both the global kill-switch and the per-org motion
    toggle ON.  ``email_motion`` defaults OFF so tests that exercise
    the email path must opt in explicitly."""
    monkeypatch.setattr(notifications_mod.settings, "EMAIL_ENABLED", True)
    Setting.set(db, "org_test123", "email_motion", "true")


def _emit_motion(db, *, camera_id="cam_front_door", title=None):
    """Helper — emit a motion notification matching the shape ws.py
    produces in production."""
    return create_notification(
        org_id="org_test123",
        kind="motion",
        title=title or f"Motion on {camera_id}",
        body="Scene change detected at 80% intensity.",
        severity="info",
        audience="all",
        link=f"/dashboard?camera={camera_id}",
        camera_id=camera_id,
        meta={"score": 80, "segment_seq": 42},
        db=db,
    )


def _anchor_key(camera_id):
    return f"motion_email_cooldown_start:{camera_id}"


# ── Default-OFF behaviour ──────────────────────────────────────────


def test_motion_default_off(db, monkeypatch, stub_recipients):
    """Fresh org with no settings touched: motion is in
    ``_EMAIL_KIND_TO_SETTING`` but defaults FALSE.  Inbox row and SSE
    fire (they default ON in ``_NOTIFICATION_KIND_TO_SETTING``), but
    no email outbox row and no anchor written.

    This is the CRITICAL safety call — opting users in by default
    risks day-one email volume on outdoor cameras and tanks Resend
    sender reputation if recipients spam-mark."""
    monkeypatch.setattr(notifications_mod.settings, "EMAIL_ENABLED", True)
    # Note: NOT calling _enable_motion_email — this test specifically
    # exercises the unconfigured-org path.

    notif = _emit_motion(db)

    assert notif is not None  # inbox row created
    assert db.query(EmailOutbox).count() == 0
    assert Setting.get(db, "org_test123", _anchor_key("cam_front_door"), "") == ""


def test_motion_kill_switch_off_skips_email_and_anchor(db, monkeypatch, stub_recipients):
    """``EMAIL_ENABLED=false`` global kill-switch — even with the
    per-org toggle on, no email and no anchor.  The anchor write must
    be gated behind the email-enabled check, NOT the inbox check."""
    Setting.set(db, "org_test123", "email_motion", "true")
    monkeypatch.setattr(notifications_mod.settings, "EMAIL_ENABLED", False)

    _emit_motion(db)

    assert db.query(EmailOutbox).count() == 0
    assert Setting.get(db, "org_test123", _anchor_key("cam_front_door"), "") == ""


# ── Cooldown gate (the core of v1.1) ───────────────────────────────


def test_motion_first_event_sends_immediate_and_writes_anchor(
    db, monkeypatch, stub_recipients
):
    """Empty anchor + email enabled → enqueue immediate email + write
    the cooldown anchor with current ISO timestamp.  This is the
    entry point of every cooldown cycle."""
    _enable_motion_email(db, monkeypatch)

    _emit_motion(db)

    # Email enqueued for the one stubbed recipient.
    rows = db.query(EmailOutbox).filter_by(kind="motion").all()
    assert len(rows) == 1
    assert rows[0].recipient_email == "alice@org.test"
    # Anchor written.
    anchor = Setting.get(db, "org_test123", _anchor_key("cam_front_door"), "")
    assert anchor, "expected an anchor timestamp string after first event"
    # Round-trips as ISO format.
    parsed = datetime.fromisoformat(anchor)
    delta = abs((datetime.now(tz=UTC).replace(tzinfo=None) - parsed).total_seconds())
    assert delta < 5, f"anchor timestamp should be ~now, drift={delta:.1f}s"


def test_motion_silenced_when_anchor_active(db, monkeypatch, stub_recipients):
    """Second motion event within the cooldown window writes inbox +
    SSE rows but skips the email outbox.  This is the volume-control
    behaviour the whole cooldown design exists for."""
    _enable_motion_email(db, monkeypatch)

    # First event seeds the anchor + immediate email.
    _emit_motion(db)
    assert db.query(EmailOutbox).count() == 1
    assert db.query(Notification).filter_by(kind="motion").count() == 1

    # Second event 1 second later — anchor still active.
    _emit_motion(db)

    # Inbox row count goes up...
    assert db.query(Notification).filter_by(kind="motion").count() == 2
    # ...but email outbox count does NOT.
    assert db.query(EmailOutbox).count() == 1


def test_motion_resumes_after_cooldown_expiry(
    db, monkeypatch, stub_recipients
):
    """Manually backdate the anchor past the cooldown window → the
    next motion event treats it as expired, fires a fresh immediate
    email, and overwrites the anchor with the new timestamp."""
    _enable_motion_email(db, monkeypatch)

    # Backdate anchor 16 minutes (default cooldown is 15).
    expired = (
        datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(minutes=16)
    ).isoformat()
    Setting.set(db, "org_test123", _anchor_key("cam_front_door"), expired)

    _emit_motion(db)

    # New email enqueued (fresh cycle).
    assert db.query(EmailOutbox).count() == 1
    # Anchor overwritten — strictly newer than the expired value.
    anchor_now = Setting.get(db, "org_test123", _anchor_key("cam_front_door"), "")
    assert anchor_now != expired
    assert datetime.fromisoformat(anchor_now) > datetime.fromisoformat(expired)


def test_motion_per_camera_independence(db, monkeypatch, stub_recipients):
    """A camera in cooldown does NOT suppress an immediate email for
    a DIFFERENT camera.  Per-camera anchor keys make this work."""
    _enable_motion_email(db, monkeypatch)

    _emit_motion(db, camera_id="cam_a")  # immediate for A, anchor for A
    _emit_motion(db, camera_id="cam_b")  # immediate for B, anchor for B

    # Both fired immediates.
    assert db.query(EmailOutbox).count() == 2
    # Both anchors present.
    assert Setting.get(db, "org_test123", _anchor_key("cam_a"), "")
    assert Setting.get(db, "org_test123", _anchor_key("cam_b"), "")

    # Now a SECOND event on A is silenced (A's anchor is fresh)...
    _emit_motion(db, camera_id="cam_a")
    assert db.query(EmailOutbox).count() == 2  # unchanged
    # ...but a second event on B is also silenced (B's anchor is fresh).
    _emit_motion(db, camera_id="cam_b")
    assert db.query(EmailOutbox).count() == 2  # unchanged


# ── Existing-gate compatibility ────────────────────────────────────


def test_motion_inbox_disabled_still_emails_when_email_enabled(
    db, monkeypatch, stub_recipients
):
    """Setting.motion_notifications=false suppresses the INBOX row (and
    the caller gets None), but the email side-channel is an independent
    toggle: with ``email_motion=true`` the email must still flow, anchor
    included.  Pins the decoupling — the Settings UI presents inbox and
    email as separate per-kind switches, so muting bell noise must not
    silently kill alert emails (an earlier early-return did exactly
    that)."""
    _enable_motion_email(db, monkeypatch)
    Setting.set(db, "org_test123", "motion_notifications", "false")

    notif = _emit_motion(db)

    assert notif is None  # inbox suppressed → caller sees None
    assert db.query(Notification).filter_by(kind="motion").count() == 0
    # Email side-channel still ran: outbox row enqueued, anchor armed.
    assert db.query(EmailOutbox).count() == 1
    assert Setting.get(db, "org_test123", _anchor_key("cam_front_door"), "") != ""


def test_motion_both_toggles_off_short_circuits_everything(
    db, monkeypatch, stub_recipients
):
    """Inbox AND email both off → nothing anywhere: no row, no outbox,
    no anchor."""
    monkeypatch.setattr(notifications_mod.settings, "EMAIL_ENABLED", True)
    Setting.set(db, "org_test123", "motion_notifications", "false")
    Setting.set(db, "org_test123", "email_motion", "false")

    notif = _emit_motion(db)

    assert notif is None
    assert db.query(Notification).filter_by(kind="motion").count() == 0
    assert db.query(EmailOutbox).count() == 0
    assert Setting.get(db, "org_test123", _anchor_key("cam_front_door"), "") == ""


def test_motion_email_disabled_skips_anchor(db, monkeypatch, stub_recipients):
    """``email_motion=false`` → no email, no anchor written either.
    Important: the anchor is part of the email-side state machine,
    not a general per-camera tracker.  If email is off, the anchor
    must stay clean so flipping email on later starts a fresh window
    rather than inheriting a phantom 'cooldown active' state."""
    monkeypatch.setattr(notifications_mod.settings, "EMAIL_ENABLED", True)
    Setting.set(db, "org_test123", "email_motion", "false")

    _emit_motion(db)

    # Inbox row still created (motion_notifications defaults True).
    assert db.query(Notification).filter_by(kind="motion").count() == 1
    # No email enqueued and no anchor written.
    assert db.query(EmailOutbox).count() == 0
    assert Setting.get(db, "org_test123", _anchor_key("cam_front_door"), "") == ""


# ── Edge cases ─────────────────────────────────────────────────────


def test_motion_malformed_anchor_treated_as_expired(
    db, monkeypatch, stub_recipients
):
    """Garbage in the anchor value (manual edit, partial write,
    encoding bug) → next motion event treats it as expired and
    overwrites cleanly.  Without this fallback, a single corrupt
    value would silence emails for that camera forever."""
    _enable_motion_email(db, monkeypatch)
    Setting.set(
        db, "org_test123", _anchor_key("cam_front_door"),
        "this is not an ISO timestamp",
    )

    _emit_motion(db)

    # Fresh email enqueued despite the garbage anchor.
    assert db.query(EmailOutbox).count() == 1
    # Anchor overwritten with a parseable value.
    fresh = Setting.get(db, "org_test123", _anchor_key("cam_front_door"), "")
    datetime.fromisoformat(fresh)  # raises if still malformed


def test_motion_cooldown_minutes_setting_respected(
    db, monkeypatch, stub_recipients
):
    """Custom ``email_motion_cooldown_minutes=1`` → events 90 seconds
    apart fire two immediate emails (because the 1-min window has
    already expired by the second event)."""
    _enable_motion_email(db, monkeypatch)
    Setting.set(db, "org_test123", "email_motion_cooldown_minutes", "1")

    # First event: writes anchor at "now".
    _emit_motion(db)
    assert db.query(EmailOutbox).count() == 1

    # Backdate the anchor 90s ago — now past the 1-min cooldown.
    backdated = (
        datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(seconds=90)
    ).isoformat()
    Setting.set(db, "org_test123", _anchor_key("cam_front_door"), backdated)

    _emit_motion(db)
    assert db.query(EmailOutbox).count() == 2


def test_motion_cooldown_minutes_malformed_falls_back(
    db, monkeypatch, stub_recipients
):
    """Garbage in ``email_motion_cooldown_minutes`` → fall back to the
    15-min default rather than crashing.  Defensive against direct-DB
    typos like ``"fifteen"`` or empty strings."""
    _enable_motion_email(db, monkeypatch)
    Setting.set(db, "org_test123", "email_motion_cooldown_minutes", "not-a-number")

    # Should still emit the immediate email with default cooldown applied.
    _emit_motion(db)
    assert db.query(EmailOutbox).count() == 1

    # Helper directly returns the fallback value.
    assert notifications_mod._motion_cooldown_minutes(db, "org_test123") == 15


# ── Helper unit-tests ───────────────────────────────────────────────


def test_anchor_key_format():
    """Anchor key format is the contract that the digest loop's LIKE
    query and split-on-colon parsing rely on.  Pin it so a future
    refactor that changes the format also has to update the loop."""
    assert (
        notifications_mod._motion_cooldown_anchor_key("cam_xyz")
        == "motion_email_cooldown_start:cam_xyz"
    )


def test_claim_motion_cooldown_returns_true_on_missing_camera_id(db):
    """Defensive — ``camera_id=None`` returns True (do email) without
    writing an anchor (which would have a malformed key).  Today
    motion events always carry a camera_id, but a future caller
    shouldn't silently break."""
    assert (
        notifications_mod._claim_motion_cooldown_or_silence(
            db, "org_test123", None,
        )
        is True
    )
    # No anchor row written for None.
    assert (
        db.query(Setting)
        .filter(Setting.key.like("motion_email_cooldown_start:%"))
        .count()
        == 0
    )
