"""Clerk webhook integration tests.

These cover the code-level plumbing of subscription lifecycle events
(created / updated / canceled / past-due) into the `Setting(org_plan)`
cache that the rest of the backend reads for feature gating.

A silent regression here means a paying customer never actually gets
the paid tier — so we sign real svix payloads and POST them at the
endpoint, rather than mocking `Webhook.verify()`. If the signature
verification path breaks, these tests break with it.
"""

import json
from datetime import UTC, datetime, timezone
from unittest.mock import patch

import pytest
from svix.webhooks import Webhook

from app.core.config import settings
from app.models.models import Setting

TEST_ORG_ID = "org_test123"
# Fixed test secret so every test signs with the same key. Format matches
# Clerk's whsec_<base64> convention; the body is a throwaway 32-byte key.
TEST_WEBHOOK_SECRET = "whsec_dGVzdHNlY3JldHRlc3RzZWNyZXR0ZXN0c2VjcmV0MTI="


def _signed_post(client, event_type: str, data: dict, *, secret: str = TEST_WEBHOOK_SECRET):
    """POST a signed Clerk-style webhook to /api/webhooks/clerk."""
    payload = json.dumps({"type": event_type, "data": data})
    msg_id = f"msg_{event_type.replace('.', '_')}_test"
    ts = datetime.now(tz=UTC)

    sig = Webhook(secret).sign(msg_id, ts, payload)

    headers = {
        "svix-id": msg_id,
        "svix-timestamp": str(int(ts.timestamp())),
        "svix-signature": sig,
        "content-type": "application/json",
    }
    return client.post("/api/webhooks/clerk", content=payload, headers=headers)


@pytest.fixture
def webhook_client(unauthenticated_client, monkeypatch):
    """Client with CLERK_WEBHOOK_SECRET configured + Clerk SDK stubbed.

    `clerk.organizations.update` is called from `set_org_member_limit`.
    Without a real Clerk API it raises, and while the handler catches
    it, the stack trace pollutes test output. Stubbing keeps the logs
    quiet and makes failures easier to read.
    """
    monkeypatch.setattr(settings, "CLERK_WEBHOOK_SECRET", TEST_WEBHOOK_SECRET)
    with patch("app.api.webhooks.clerk.organizations.update"):
        yield unauthenticated_client


# ─── Subscription lifecycle ─────────────────────────────────────────

def test_subscription_created_sets_org_plan_to_pro(webhook_client, db):
    """The most important billing test in the repo: when Clerk fires
    `subscription.created` with an active pro item, the handler persists
    `org_plan=pro`. A regression here silently keeps every paying
    customer on free_org."""
    resp = _signed_post(webhook_client, "subscription.created", {
        "payer": {"organization_id": TEST_ORG_ID},
        "items": [{"status": "active", "plan": {"slug": "pro"}}],
    })
    assert resp.status_code == 200
    assert resp.json() == {"received": True}
    assert Setting.get(db, TEST_ORG_ID, "org_plan") == "pro"


def test_subscription_updated_flips_plan_pro_to_pro_plus(webhook_client, db):
    """Upgrade path: the new slug overwrites the cached one."""
    Setting.set(db, TEST_ORG_ID, "org_plan", "pro")

    resp = _signed_post(webhook_client, "subscription.updated", {
        "payer": {"organization_id": TEST_ORG_ID},
        "items": [{"status": "active", "plan": {"slug": "pro_plus"}}],
    })
    assert resp.status_code == 200
    assert Setting.get(db, TEST_ORG_ID, "org_plan") == "pro_plus"


def test_subscription_active_also_sets_plan(webhook_client, db):
    """`subscription.active` is treated the same as created/updated —
    some Clerk configurations fire this event instead of the other two."""
    resp = _signed_post(webhook_client, "subscription.active", {
        "payer": {"organization_id": TEST_ORG_ID},
        "items": [{"status": "active", "plan": {"slug": "pro"}}],
    })
    assert resp.status_code == 200
    assert Setting.get(db, TEST_ORG_ID, "org_plan") == "pro"


def test_subscription_item_canceled_keeps_plan_until_period_end(webhook_client, db):
    """Cancellation is SCHEDULED, not immediate: per Clerk semantics the
    payer retains plan features until the period ends.  The handler must
    only record the pending cancel — downgrading here revoked a paid
    month on day 1 (and turned scheduled pro_plus→pro downgrades into a
    drop to free)."""
    Setting.set(db, TEST_ORG_ID, "org_plan", "pro")

    resp = _signed_post(webhook_client, "subscriptionItem.canceled", {
        "payer": {"organization_id": TEST_ORG_ID},
    })
    assert resp.status_code == 200
    # Plan untouched; cancel recorded for UI.
    assert Setting.get(db, TEST_ORG_ID, "org_plan") == "pro"
    assert Setting.get(db, TEST_ORG_ID, "plan_cancel_pending") == "true"


def test_subscription_item_ended_applies_live_entitlement(webhook_client, db, monkeypatch):
    """`subscriptionItem.ended` (period actually over) re-resolves the
    org's CURRENT entitlement from Clerk — free after a full cancel, but
    the next tier down after a scheduled downgrade."""
    import app.api.webhooks  # noqa: F401 — ensure module import for patch target
    from app.core import plans as plans_mod

    Setting.set(db, TEST_ORG_ID, "org_plan", "pro_plus")
    monkeypatch.setattr(plans_mod, "fetch_live_plan_slug", lambda org_id: "free_org")

    resp = _signed_post(webhook_client, "subscriptionItem.ended", {
        "payer": {"organization_id": TEST_ORG_ID},
    })
    assert resp.status_code == 200
    assert Setting.get(db, TEST_ORG_ID, "org_plan") == "free_org"
    assert Setting.get(db, TEST_ORG_ID, "plan_cancel_pending") == ""


def test_subscription_item_ended_scheduled_downgrade_lands_on_pro(
    webhook_client, db, monkeypatch
):
    """pro_plus item ends while a pro item is active → org lands on pro,
    NOT free."""
    from app.core import plans as plans_mod

    Setting.set(db, TEST_ORG_ID, "org_plan", "pro_plus")
    monkeypatch.setattr(plans_mod, "fetch_live_plan_slug", lambda org_id: "pro")

    resp = _signed_post(webhook_client, "subscriptionItem.ended", {
        "payer": {"organization_id": TEST_ORG_ID},
    })
    assert resp.status_code == 200
    assert Setting.get(db, TEST_ORG_ID, "org_plan") == "pro"


def test_updated_snapshot_with_canceled_but_paid_through_item_keeps_plan(
    webhook_client, db
):
    """A subscription.updated snapshot taken right after a cancel click
    contains only a canceled item with a future period_end — the org is
    still entitled until then, so the plan must NOT drop to free."""
    import time as _time

    Setting.set(db, TEST_ORG_ID, "org_plan", "pro")
    future_ms = int((_time.time() + 14 * 86400) * 1000)

    resp = _signed_post(webhook_client, "subscription.updated", {
        "payer": {"organization_id": TEST_ORG_ID},
        "items": [{
            "status": "canceled",
            "period_end": future_ms,
            "plan": {"slug": "pro"},
        }],
    })
    assert resp.status_code == 200
    assert Setting.get(db, TEST_ORG_ID, "org_plan") == "pro"


# ─── Payment states ────────────────────────────────────────────────

def test_past_due_sets_flag(webhook_client, db):
    """`subscription.pastDue` sets `payment_past_due=true`. The plan
    stays intact during the grace period so the user isn't demoted the
    moment a card declines; `require_active_billing` handles blocking
    writes separately."""
    resp = _signed_post(webhook_client, "subscription.pastDue", {
        "payer": {"organization_id": TEST_ORG_ID},
    })
    assert resp.status_code == 200
    assert Setting.get(db, TEST_ORG_ID, "payment_past_due") == "true"


def test_subscription_item_past_due_also_sets_flag(webhook_client, db):
    """The item-level `subscriptionItem.pastDue` is handled the same."""
    resp = _signed_post(webhook_client, "subscriptionItem.pastDue", {
        "payer": {"organization_id": TEST_ORG_ID},
    })
    assert resp.status_code == 200
    assert Setting.get(db, TEST_ORG_ID, "payment_past_due") == "true"


def test_payment_paid_clears_past_due(webhook_client, db):
    """`paymentAttempt.updated` with status=paid clears the flag so the
    org can write again without manual intervention."""
    Setting.set(db, TEST_ORG_ID, "payment_past_due", "true")

    resp = _signed_post(webhook_client, "paymentAttempt.updated", {
        "payer": {"organization_id": TEST_ORG_ID},
        "status": "paid",
    })
    assert resp.status_code == 200
    assert Setting.get(db, TEST_ORG_ID, "payment_past_due") == "false"


def test_payment_failed_does_not_clear_past_due(webhook_client, db):
    """A failed retry must NOT clear the flag."""
    Setting.set(db, TEST_ORG_ID, "payment_past_due", "true")

    resp = _signed_post(webhook_client, "paymentAttempt.updated", {
        "payer": {"organization_id": TEST_ORG_ID},
        "status": "failed",
    })
    assert resp.status_code == 200
    assert Setting.get(db, TEST_ORG_ID, "payment_past_due") == "true"


# ─── Idempotency ───────────────────────────────────────────────────

def test_duplicate_svix_id_is_no_op(webhook_client, db):
    """Svix retries the same message (same svix-id) on any non-2xx /
    network failure.  Without idempotency the handler would re-run all
    side effects on each retry — re-set member limits, re-fire
    enforce_camera_cap.  This test posts the same payload twice and
    asserts the second call returns ``status: duplicate`` without
    re-processing.

    This is the audit-flagged "money-flavored" failure mode: a Clerk
    retry of subscription.updated could double-count plan changes."""
    from app.models.models import ProcessedWebhook

    # First delivery — processes normally.
    payload = json.dumps({
        "type": "subscription.created",
        "data": {
            "payer": {"organization_id": TEST_ORG_ID},
            "items": [{"status": "active", "plan": {"slug": "pro"}}],
        },
    })
    msg_id = "msg_dedup_test_42"
    ts = datetime.now(tz=UTC)
    sig = Webhook(TEST_WEBHOOK_SECRET).sign(msg_id, ts, payload)
    headers = {
        "svix-id": msg_id,
        "svix-timestamp": str(int(ts.timestamp())),
        "svix-signature": sig,
        "content-type": "application/json",
    }

    first = webhook_client.post("/api/webhooks/clerk", content=payload, headers=headers)
    assert first.status_code == 200
    assert first.json() == {"received": True}
    assert Setting.get(db, TEST_ORG_ID, "org_plan") == "pro"
    assert (
        db.query(ProcessedWebhook).filter_by(svix_msg_id=msg_id).count() == 1
    )

    # Now flip the persisted plan so we can detect re-processing.  If
    # the handler runs again on the second delivery, it would overwrite
    # this back to "pro".
    Setting.set(db, TEST_ORG_ID, "org_plan", "pro_plus")

    # Second delivery — same msg_id, same payload.
    second = webhook_client.post("/api/webhooks/clerk", content=payload, headers=headers)
    assert second.status_code == 200
    assert second.json()["status"] == "duplicate"
    # Plan stayed at pro_plus — second handler did NOT re-run.
    assert Setting.get(db, TEST_ORG_ID, "org_plan") == "pro_plus"
    # Still exactly one row.
    assert (
        db.query(ProcessedWebhook).filter_by(svix_msg_id=msg_id).count() == 1
    )


def test_standard_webhooks_header_variant_is_processed_and_deduped(webhook_client, db):
    """Svix's verify() accepts the Standard-Webhooks ``webhook-*`` headers
    in addition to ``svix-*``, and the ecosystem is migrating that way.
    Under that variant the handler must STILL record the ProcessedWebhook
    row and short-circuit a redelivery.

    This is the regression guard for a money-flavored bug: the id was
    read only from ``svix-id``, so under ``webhook-*`` headers (which
    still pass signature verification) ``svix_msg_id`` went None. That
    disabled idempotency AND skipped the gated commit — the sole commit
    that persists ``enforce_camera_cap``'s ``disabled_by_plan`` flips — so
    an org that upgraded or cancelled would get its plan Setting written
    (``Setting.set`` self-commits) while its cameras were left in the
    wrong state.  The ProcessedWebhook row's existence below proves that
    gated commit executed under the variant.
    """
    from app.models.models import ProcessedWebhook

    payload = json.dumps({
        "type": "subscription.created",
        "data": {
            "payer": {"organization_id": TEST_ORG_ID},
            "items": [{"status": "active", "plan": {"slug": "pro"}}],
        },
    })
    msg_id = "msg_stdwebhooks_variant_7"
    ts = datetime.now(tz=UTC)
    # The signature is over {id}.{timestamp}.{payload} regardless of which
    # header NAMES carry them, so signing normally then sending the values
    # under webhook-* headers verifies fine.
    sig = Webhook(TEST_WEBHOOK_SECRET).sign(msg_id, ts, payload)
    headers = {
        "webhook-id": msg_id,
        "webhook-timestamp": str(int(ts.timestamp())),
        "webhook-signature": sig,
        "content-type": "application/json",
    }

    first = webhook_client.post("/api/webhooks/clerk", content=payload, headers=headers)
    assert first.status_code == 200
    assert first.json() == {"received": True}
    assert Setting.get(db, TEST_ORG_ID, "org_plan") == "pro"
    # Dedup row recorded under the webhook-id → svix_msg_id was populated
    # from the webhook-* namespace and the gated commit ran.
    assert db.query(ProcessedWebhook).filter_by(svix_msg_id=msg_id).count() == 1

    # Redelivery with the same webhook-id short-circuits as a duplicate.
    second = webhook_client.post("/api/webhooks/clerk", content=payload, headers=headers)
    assert second.status_code == 200
    assert second.json()["status"] == "duplicate"
    assert db.query(ProcessedWebhook).filter_by(svix_msg_id=msg_id).count() == 1


# ─── Security ──────────────────────────────────────────────────────

def test_invalid_signature_rejected(webhook_client, db):
    """A payload signed with the wrong secret is rejected with 400 and
    produces no side effect."""
    resp = _signed_post(
        webhook_client,
        "subscription.created",
        {
            "payer": {"organization_id": TEST_ORG_ID},
            "items": [{"status": "active", "plan": {"slug": "pro"}}],
        },
        secret="whsec_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    )
    assert resp.status_code == 400
    assert Setting.get(db, TEST_ORG_ID, "org_plan") is None


def test_missing_webhook_secret_fails_closed(unauthenticated_client, monkeypatch, db):
    """Without `CLERK_WEBHOOK_SECRET` configured the endpoint MUST fail
    closed — never silently accept unsigned payloads. A silent-accept
    regression would give anyone on the internet write access to the
    billing cache."""
    monkeypatch.setattr(settings, "CLERK_WEBHOOK_SECRET", "")

    resp = unauthenticated_client.post(
        "/api/webhooks/clerk",
        json={
            "type": "subscription.created",
            "data": {
                "payer": {"organization_id": TEST_ORG_ID},
                "items": [{"status": "active", "plan": {"slug": "pro"}}],
            },
        },
    )
    assert resp.status_code == 400
    assert Setting.get(db, TEST_ORG_ID, "org_plan") is None


# ─── Edge cases ────────────────────────────────────────────────────

def test_unknown_plan_slug_stored_verbatim(webhook_client, db):
    """Clerk dashboards configured with non-matching slugs ('Pro' with
    a capital P, 'pro-v2', etc.) are the #1 production misconfig for
    this path. We store whatever slug Clerk sent so an operator can
    see it in the DB and spot the mismatch — better than silently
    dropping to free_org and leaving no trace."""
    resp = _signed_post(webhook_client, "subscription.created", {
        "payer": {"organization_id": TEST_ORG_ID},
        "items": [{"status": "active", "plan": {"slug": "unknown_slug"}}],
    })
    assert resp.status_code == 200
    assert Setting.get(db, TEST_ORG_ID, "org_plan") == "unknown_slug"


def test_subscription_with_no_active_items_resolves_to_free(webhook_client, db):
    """All-paused subscriptions resolve to free_org via
    `get_active_plan_slug`'s fallback."""
    resp = _signed_post(webhook_client, "subscription.updated", {
        "payer": {"organization_id": TEST_ORG_ID},
        "items": [{"status": "paused", "plan": {"slug": "pro"}}],
    })
    assert resp.status_code == 200
    assert Setting.get(db, TEST_ORG_ID, "org_plan") == "free_org"


def test_webhook_without_org_id_is_noop(webhook_client, db):
    """Malformed payloads missing `payer.organization_id` are ignored
    gracefully (200) rather than crashing the endpoint — Clerk retries
    on 5xx, so a crash would compound the problem."""
    resp = _signed_post(webhook_client, "subscription.created", {
        "items": [{"status": "active", "plan": {"slug": "pro"}}],
    })
    assert resp.status_code == 200
    # Nothing written
    assert Setting.get(db, TEST_ORG_ID, "org_plan") is None


def test_org_deleted_purges_settings(webhook_client, db):
    """`organization.deleted` wipes all org-scoped data. We verify the
    Setting deletion here; the broader cascade (nodes/cameras/keys) is
    covered by the integration test in test_security.py when org data
    is scoped. If THIS fails, we leak org data after account deletion."""
    Setting.set(db, TEST_ORG_ID, "org_plan", "pro")
    Setting.set(db, TEST_ORG_ID, "payment_past_due", "false")

    resp = _signed_post(webhook_client, "organization.deleted", {
        "id": TEST_ORG_ID,
    })
    assert resp.status_code == 200
    assert Setting.get(db, TEST_ORG_ID, "org_plan") is None
    assert Setting.get(db, TEST_ORG_ID, "payment_past_due") is None


# ─── Membership lifecycle (security audit notifications) ────────────
# These pin the 2026-05-04 wiring: Clerk membership webhooks fire
# admin notifications so a "did someone just add themselves to my
# org?" question is answered within seconds.

def test_membership_created_emits_member_added_notification(webhook_client, db):
    """`organizationMembership.created` → one member_added
    notification with the actor's identifier in the body."""
    from app.models.models import Notification

    resp = _signed_post(webhook_client, "organizationMembership.created", {
        "id": "orgmem_xyz",
        "organization": {"id": TEST_ORG_ID, "name": "Test Org"},
        "public_user_data": {
            "user_id": "user_new",
            "identifier": "newbie@example.com",
        },
        "role": "org:member",
    })

    assert resp.status_code == 200
    notifs = (
        db.query(Notification)
        .filter_by(kind="member_added", org_id=TEST_ORG_ID)
        .all()
    )
    assert len(notifs) == 1
    n = notifs[0]
    assert n.audience == "admin"
    # Member role → info severity (admin role would be warning).
    assert n.severity == "info"
    assert "newbie@example.com" in n.title
    assert "newbie@example.com" in n.body


def test_membership_created_admin_role_is_warning_severity(webhook_client, db):
    """A new member added directly as ADMIN is more security-relevant
    than a new member at lower role — bumped to warning severity so
    inbox UI treatment / email subject prefix can reflect it."""
    from app.models.models import Notification

    _signed_post(webhook_client, "organizationMembership.created", {
        "id": "orgmem_admin",
        "organization": {"id": TEST_ORG_ID, "name": "Test Org"},
        "public_user_data": {
            "user_id": "user_new_admin",
            "identifier": "admin@example.com",
        },
        "role": "org:admin",
    })

    notif = (
        db.query(Notification)
        .filter_by(kind="member_added", org_id=TEST_ORG_ID)
        .first()
    )
    assert notif is not None
    assert notif.severity == "warning"


def test_membership_updated_emits_role_changed_notification(webhook_client, db):
    """`organizationMembership.updated` → one member_role_changed
    notification with the new role in the body."""
    from app.models.models import Notification

    resp = _signed_post(webhook_client, "organizationMembership.updated", {
        "id": "orgmem_promo",
        "organization": {"id": TEST_ORG_ID, "name": "Test Org"},
        "public_user_data": {
            "user_id": "user_promoted",
            "identifier": "promoted@example.com",
        },
        "role": "org:admin",
    })

    assert resp.status_code == 200
    notif = (
        db.query(Notification)
        .filter_by(kind="member_role_changed", org_id=TEST_ORG_ID)
        .first()
    )
    assert notif is not None
    assert notif.audience == "admin"
    assert notif.severity == "warning"  # promotion to admin
    assert "promoted@example.com" in notif.body
    import json as _json
    meta = _json.loads(notif.meta_json)
    assert meta["new_role"] == "admin"


def test_membership_deleted_emits_member_removed_notification(webhook_client, db):
    """`organizationMembership.deleted` → one member_removed
    notification."""
    from app.models.models import Notification

    resp = _signed_post(webhook_client, "organizationMembership.deleted", {
        "id": "orgmem_gone",
        "organization": {"id": TEST_ORG_ID, "name": "Test Org"},
        "public_user_data": {
            "user_id": "user_gone",
            "identifier": "exit@example.com",
        },
    })

    assert resp.status_code == 200
    notif = (
        db.query(Notification)
        .filter_by(kind="member_removed", org_id=TEST_ORG_ID)
        .first()
    )
    assert notif is not None
    assert notif.audience == "admin"
    assert notif.severity == "info"
    assert "exit@example.com" in notif.body


def test_membership_event_without_org_id_is_noop(webhook_client, db):
    """A malformed payload missing the organization.id field must
    not crash the handler — just return 200 and skip."""
    from app.models.models import Notification

    resp = _signed_post(webhook_client, "organizationMembership.created", {
        "id": "orgmem_orphan",
        # No "organization" field at all.
        "public_user_data": {"identifier": "ghost@example.com"},
        "role": "org:member",
    })

    assert resp.status_code == 200
    assert (
        db.query(Notification)
        .filter_by(kind="member_added")
        .count()
    ) == 0


def test_membership_notification_failure_does_not_break_webhook(webhook_client, db, monkeypatch):
    """If create_notification raises during a membership event,
    the webhook MUST still return 200 — Clerk retries on non-2xx
    and we don't want a notification fault to cause webhook
    backpressure that blocks unrelated events."""
    from app.api import webhooks as webhooks_mod

    def boom(*args, **kwargs):
        raise RuntimeError("notification system down")
    # Patch the function reference inside the webhooks module's
    # namespace at the level the handler imports it (lazy import
    # inside the handler — patch via the source module).
    monkeypatch.setattr(
        "app.api.notifications.create_notification", boom,
    )

    resp = _signed_post(webhook_client, "organizationMembership.created", {
        "id": "orgmem_resilience",
        "organization": {"id": TEST_ORG_ID, "name": "Test Org"},
        "public_user_data": {"identifier": "test@example.com"},
        "role": "org:member",
    })

    assert resp.status_code == 200
