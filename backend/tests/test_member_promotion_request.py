"""
Tests for the member-initiated admin-promotion request endpoint.

What's pinned here:

  1. A non-admin (viewer) calling POST /api/notifications/request-admin-promotion
     produces a notification + email queued for the org's admins.
  2. An admin calling the endpoint gets a friendly 400 (no point
     emitting "X requested admin access" to themselves).
  3. The rate limit (3/hour) fires on the 4th request from the same
     org bucket — prevents spam.
  4. The notification carries the requester's identity in meta so
     the email template + future UI can show who asked.
"""

from __future__ import annotations

import pytest

from app.api import notifications as notifications_mod
from app.models.models import EmailOutbox, Notification

# ── Fixtures ────────────────────────────────────────────────────────


@pytest.fixture
def stub_recipients(monkeypatch):
    """Stub Clerk's recipient lookup so the email-enqueue path doesn't
    need a real Clerk API.  Returns one admin address."""
    monkeypatch.setattr(
        notifications_mod, "get_recipient_emails",
        lambda org_id, audience: ["admin@example.com"],
    )


@pytest.fixture
def email_enabled(monkeypatch):
    """Flip the global email kill-switch on."""
    monkeypatch.setattr(notifications_mod.settings, "EMAIL_ENABLED", True)


# ── Tests ───────────────────────────────────────────────────────────


def test_viewer_can_request_promotion(viewer_client, db, stub_recipients, email_enabled):
    """The happy path: a member without admin access clicks the button,
    the backend writes an inbox notification + queues an email for
    every admin in the org."""
    resp = viewer_client.post("/api/notifications/request-admin-promotion")
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body.get("success") is True
    assert "request has been sent" in body.get("message", "").lower()

    # Inbox row written, kind matches the new notification type.
    notifs = db.query(Notification).filter_by(
        org_id="org_test123", kind="member_promotion_requested",
    ).all()
    assert len(notifs) == 1
    notif = notifs[0]
    # Audience is admin so non-admin members don't see "X is asking
    # for admin access" (that would be bizarre noise for non-admins).
    assert notif.audience == "admin"
    # Title carries the requester so admins see who asked at a glance.
    assert "viewer@test.com" in notif.title or "testviewer" in notif.title


def test_promotion_request_enqueues_email_for_admins(
    viewer_client, db, stub_recipients, email_enabled,
):
    """Email side-channel: admins get an email so they don't have to
    be looking at the dashboard to notice the request."""
    viewer_client.post("/api/notifications/request-admin-promotion")

    outbox = db.query(EmailOutbox).filter_by(
        org_id="org_test123", kind="member_promotion_requested",
    ).all()
    assert len(outbox) == 1
    row = outbox[0]
    assert row.recipient_email == "admin@example.com"
    assert "Admin access requested" in row.subject
    # Body explains what admin access actually grants — the receiver
    # should be able to make a decision from the email alone.
    assert "CameraNode" in row.body_text
    assert "MCP" in row.body_text


def test_admin_promotion_request_returns_400(admin_client):
    """An admin clicking the button (e.g. dev tools, button still
    rendered by accident) gets a friendly 400, not a notification.
    Otherwise the admin would see 'admin@test.com requested admin
    access' from themselves, which is confusing nonsense."""
    resp = admin_client.post("/api/notifications/request-admin-promotion")
    assert resp.status_code == 400
    body = resp.json()
    # API error envelope — match either shape (legacy detail vs new error).
    detail = body.get("detail") or body.get("message") or ""
    assert "already an admin" in detail.lower()


def test_promotion_request_rate_limited(viewer_client, stub_recipients, email_enabled):
    """3 requests/hour cap.  4th request from the same org bucket
    must come back 429.  Prevents a spammy member from blasting
    admins with notifications."""
    # Three consecutive requests succeed.
    for i in range(3):
        r = viewer_client.post("/api/notifications/request-admin-promotion")
        assert r.status_code == 200, f"req #{i + 1}: {r.status_code} {r.text}"

    # Fourth request hits the slowapi cap.
    overflow = viewer_client.post("/api/notifications/request-admin-promotion")
    assert overflow.status_code == 429
    # Same custom envelope as every other rate-limited endpoint.
    body = overflow.json()
    assert body.get("error") == "rate_limit_exceeded"


def test_promotion_request_persists_requester_meta(
    viewer_client, db, stub_recipients, email_enabled,
):
    """The notification's meta payload carries the requester's
    user_id + email so future UI ("who asked?") and the email
    template can render the requester's identity precisely.
    Pin the meta shape so a refactor doesn't quietly break the
    template's `notification.meta.requester_email` access."""
    import json

    viewer_client.post("/api/notifications/request-admin-promotion")

    notif = db.query(Notification).filter_by(
        org_id="org_test123", kind="member_promotion_requested",
    ).first()
    assert notif is not None
    meta = json.loads(notif.meta_json)
    assert meta.get("requester_user_id") == "user_viewer456"
    assert meta.get("requester_email") == "viewer@test.com"
    # Label is the human-readable form for display — falls back through
    # email/user_id if needed.
    assert meta.get("requester_label") in (
        "viewer@test.com", "testviewer", "user_viewer456",
    )
