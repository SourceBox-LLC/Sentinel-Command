"""
Tests for the first-touch welcome email fired on Clerk's
``organization.created`` webhook.

What's pinned here:

  1. The webhook handler routes ``organization.created`` to
     ``create_notification(kind="welcome", ...)``, which both inserts
     a Notification row (visible in the bell-icon inbox) and enqueues
     an EmailOutbox row keyed to the org's admin.

  2. The welcome kind has its own ``email_welcome`` setting with
     default True so a fresh org always gets the welcome — but a
     test-org (or any operator who flips the setting to "false")
     gets the inbox row without the email.

  3. Templates render — subject + bodies don't blow up on the meta
     payload the webhook handler emits.

The webhook secret signing path is exercised by ``test_webhooks.py``
already; this file uses ``_signed_post`` to keep the test honest
(verification still runs).
"""

from __future__ import annotations

import json
from datetime import UTC, datetime
from unittest.mock import patch

import pytest
from svix.webhooks import Webhook

from app.api import notifications as notifications_mod
from app.core.config import settings
from app.models.models import EmailOutbox, Notification, Setting

TEST_ORG_ID = "org_test_welcome_123"
TEST_WEBHOOK_SECRET = "whsec_dGVzdHNlY3JldHRlc3RzZWNyZXR0ZXN0c2VjcmV0MTI="


# ── Fixtures ────────────────────────────────────────────────────────


@pytest.fixture
def webhook_client(unauthenticated_client, monkeypatch):
    """Webhook client with secret + Clerk SDK stubs in place.

    Same shape as test_webhooks.py — yields the unauthenticated
    TestClient with environment patched so signed payloads verify."""
    monkeypatch.setattr(settings, "CLERK_WEBHOOK_SECRET", TEST_WEBHOOK_SECRET)
    with patch("app.api.webhooks.clerk.organizations.update"):
        yield unauthenticated_client


@pytest.fixture
def stub_recipients(monkeypatch):
    """Stub Clerk's recipient lookup so the email-enqueue path doesn't
    need a real Clerk API.  Returns the org-creator's address."""
    monkeypatch.setattr(
        notifications_mod, "get_recipient_emails",
        lambda org_id, audience: ["creator@example.com"],
    )


@pytest.fixture
def email_enabled(monkeypatch):
    """Flip the global email kill-switch on for this test."""
    monkeypatch.setattr(notifications_mod.settings, "EMAIL_ENABLED", True)


def _signed_post(client, event_type: str, data: dict):
    """POST a signed Clerk webhook to /api/webhooks/clerk."""
    payload = json.dumps({"type": event_type, "data": data})
    msg_id = f"msg_{event_type.replace('.', '_')}_welcome_test"
    ts = datetime.now(tz=UTC)
    sig = Webhook(TEST_WEBHOOK_SECRET).sign(msg_id, ts, payload)
    headers = {
        "svix-id": msg_id,
        "svix-timestamp": str(int(ts.timestamp())),
        "svix-signature": sig,
        "content-type": "application/json",
    }
    return client.post("/api/webhooks/clerk", content=payload, headers=headers)


# ── Tests ───────────────────────────────────────────────────────────


def test_org_created_writes_inbox_notification(
    webhook_client, db, stub_recipients, email_enabled,
):
    """The minimum-viable path: an org.created webhook produces a
    welcome row in the notifications table.  This is the "did anything
    happen?" gap we're closing — currently the webhook fires and
    emits NOTHING."""
    resp = _signed_post(webhook_client, "organization.created", {
        "id": TEST_ORG_ID,
        "name": "Acme Surveillance",
        "created_by": "user_creator_123",
    })
    assert resp.status_code == 200

    notifs = db.query(Notification).filter_by(org_id=TEST_ORG_ID).all()
    welcomes = [n for n in notifs if n.kind == "welcome"]
    assert len(welcomes) == 1, (
        f"expected exactly one welcome notification, got {len(welcomes)}"
    )

    welcome = welcomes[0]
    # Title carries the org name so the bell-icon panel reads
    # naturally ("Welcome to Sentinel, Acme Surveillance").
    assert "Acme Surveillance" in welcome.title
    # Audience is admin-only — the creator IS an admin and we don't
    # want member-tier users (added later) to retroactively get a
    # welcome inbox item the next time they log in.
    assert welcome.audience == "admin"
    # Severity is informational — green bar, not orange/red.
    assert welcome.severity == "info"
    # Link points to the dashboard — that's the natural next click.
    assert welcome.link == "/dashboard"


def test_org_created_enqueues_welcome_email(
    webhook_client, db, stub_recipients, email_enabled,
):
    """The notification side-effect we actually want: an EmailOutbox
    row gets enqueued so the worker delivers the welcome email on
    the next tick."""
    _signed_post(webhook_client, "organization.created", {
        "id": TEST_ORG_ID,
        "name": "Acme Surveillance",
        "created_by": "user_creator_123",
    })

    outbox = db.query(EmailOutbox).filter_by(
        org_id=TEST_ORG_ID, kind="welcome",
    ).all()
    assert len(outbox) == 1, (
        f"expected one welcome email queued, got {len(outbox)}"
    )
    row = outbox[0]
    assert row.recipient_email == "creator@example.com"
    assert row.status == "pending"
    # Subject template renders without exploding on the org-name interpolation.
    assert "Sentinel" in row.subject
    # Body mentions the install one-liner — the most important
    # actionable bit of the welcome content.
    assert "install.sh" in row.body_text or "install.sh" in row.body_html


def test_org_created_skipped_when_email_disabled_globally(
    webhook_client, db, stub_recipients, monkeypatch,
):
    """Global EMAIL_ENABLED kill-switch off → inbox row still written
    (the bell icon should ALWAYS show the welcome) but no email
    enqueued.  Same pattern as every other notification kind."""
    monkeypatch.setattr(notifications_mod.settings, "EMAIL_ENABLED", False)

    _signed_post(webhook_client, "organization.created", {
        "id": TEST_ORG_ID,
        "name": "Acme Surveillance",
        "created_by": "user_creator_123",
    })

    # Inbox: one row.
    notifs = db.query(Notification).filter_by(
        org_id=TEST_ORG_ID, kind="welcome",
    ).all()
    assert len(notifs) == 1

    # Outbox: zero rows.
    outbox = db.query(EmailOutbox).filter_by(
        org_id=TEST_ORG_ID, kind="welcome",
    ).all()
    assert len(outbox) == 0


def test_org_created_skipped_when_per_org_email_welcome_false(
    webhook_client, db, stub_recipients, email_enabled,
):
    """Per-org ``email_welcome=false`` setting (operator-set; no UI today)
    suppresses the email but still writes the inbox row.  Used by the
    SourceBox-internal test orgs so we don't blast ourselves on
    every dev env spin-up."""
    Setting.set(db, TEST_ORG_ID, "email_welcome", "false")

    _signed_post(webhook_client, "organization.created", {
        "id": TEST_ORG_ID,
        "name": "Internal Test Org",
        "created_by": "user_creator_123",
    })

    # Inbox row still written — welcome_notifications setting defaults True.
    notifs = db.query(Notification).filter_by(
        org_id=TEST_ORG_ID, kind="welcome",
    ).all()
    assert len(notifs) == 1

    # Outbox row NOT written — email_welcome was explicitly muted.
    outbox = db.query(EmailOutbox).filter_by(
        org_id=TEST_ORG_ID, kind="welcome",
    ).all()
    assert len(outbox) == 0


def test_org_created_handler_failure_does_not_break_webhook(
    webhook_client, db, monkeypatch,
):
    """If create_notification raises (Clerk API down, DB hiccup, etc.),
    the webhook handler MUST still return 200.  Otherwise Svix retries
    forever, and every retry tries to re-enqueue the welcome — which
    after the first success becomes a duplicate-email source.

    Pin the try/except by forcing an exception inside the call and
    asserting the handler still returns 200."""
    def _boom(**kwargs):
        raise RuntimeError("simulated Clerk outage during welcome emit")

    # Patch create_notification at its import site INSIDE the handler.
    # The handler imports it lazily (`from app.api.notifications import
    # create_notification`) so module-level monkeypatch on the source
    # is what catches it.
    monkeypatch.setattr(notifications_mod, "create_notification", _boom)

    resp = _signed_post(webhook_client, "organization.created", {
        "id": TEST_ORG_ID,
        "name": "Acme Surveillance",
        "created_by": "user_creator_123",
    })
    # The webhook still returns 200 — Svix will not retry, the org IS
    # created, the welcome is just lost.  Acceptable failure mode.
    assert resp.status_code == 200


def test_welcome_template_contains_three_step_onboarding(
    webhook_client, db, stub_recipients, email_enabled,
):
    """Pin the actual user-facing content: the welcome email guides
    new users through (1) install CameraNode, (2) wait for register,
    (3) add first camera.  This is the whole point of the welcome —
    if a refactor strips the steps from the template, this test
    catches it before users see a blank welcome."""
    _signed_post(webhook_client, "organization.created", {
        "id": TEST_ORG_ID,
        "name": "Acme Surveillance",
        "created_by": "user_creator_123",
    })

    row = db.query(EmailOutbox).filter_by(
        org_id=TEST_ORG_ID, kind="welcome",
    ).first()
    assert row is not None

    # The three-step content is what makes welcome useful vs a generic
    # "welcome to the product" blast.  Check both bodies — text and
    # HTML versions both must explain the journey.
    for body in (row.body_text, row.body_html):
        assert "CameraNode" in body, "missing CameraNode mention"
        assert "camera" in body.lower(), "missing camera mention"
        # Step numbers OR "step" word should be present somewhere.
        assert any(marker in body for marker in ("1.", "2.", "3.", "Three")), (
            "missing onboarding step structure"
        )
