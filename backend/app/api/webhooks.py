import logging
from datetime import UTC, datetime

from fastapi import APIRouter, Depends, HTTPException, Request, status
from sqlalchemy.exc import IntegrityError
from sqlalchemy.orm import Session
from svix.webhooks import Webhook, WebhookVerificationError

from app.core.clerk import clerk
from app.core.config import settings
from app.core.database import get_db
from app.core.limiter import limiter
from app.core.plans import enforce_camera_cap
from app.models.models import (
    CameraNode,
    EmailOutbox,
    EmailSuppression,
    ProcessedWebhook,
    Setting,
)

logger = logging.getLogger(__name__)

router = APIRouter(prefix="/api/webhooks", tags=["webhooks"])

# Member limits per plan — must match Clerk Dashboard plan keys and
# PLAN_LIMITS.max_seats in app.core.plans. Source of truth is plans.py; this
# dict mirrors it here because the webhook handler calls into Clerk's SDK
# which wants the integer directly.
PLAN_MEMBER_LIMITS = {
    "free_org": 2,
    "pro": 10,
    "pro_plus": 20,
}

# Paid plan slugs. Seeing a subscription.updated with one of these means the
# payment card is active (Clerk wouldn't mark the subscription live otherwise),
# so we can clear any past-due flag we were holding. Kept local to this module
# rather than imported from plans.py to keep webhook semantics self-contained.
PAID_PLAN_SLUGS_WEBHOOK = frozenset({"pro", "pro_plus"})


def set_org_member_limit(org_id: str, limit: int):
    """Update the Clerk org's max allowed memberships."""
    try:
        clerk.organizations.update(organization_id=org_id, max_allowed_memberships=limit)
        logger.info("Set org %s member limit to %d", org_id, limit)
    except Exception:
        logger.error("Failed to set member limit for org %s", org_id, exc_info=True)


def get_active_plan_slug(items: list) -> str:
    """Extract the entitled plan slug from subscription items.

    The first ``active`` item wins.  A ``canceled`` item whose
    ``period_end`` is still in the future ALSO counts: Clerk fires the
    cancellation immediately when the payer clicks cancel, but they
    retain plan features until the period ends — and a
    ``subscription.updated`` snapshot taken after a cancel click
    contains only that canceled-but-paid-through item.  Without this
    rule, that snapshot downgraded paying customers on the spot.
    """
    from app.core.plans import _item_period_end_utc

    entitled_canceled = None
    now = datetime.now(tz=UTC)
    for item in items:
        plan = item.get("plan", {})
        slug = plan.get("slug")
        if not slug:
            continue
        if item.get("status") == "active":
            return slug
        if item.get("status") == "canceled" and entitled_canceled is None:
            period_end = _item_period_end_utc(item)
            if period_end is not None and period_end > now:
                entitled_canceled = slug
    return entitled_canceled or "free_org"


@router.post("/clerk")
@limiter.limit("120/minute")
async def clerk_webhook(request: Request, db: Session = Depends(get_db)):
    payload = await request.body()
    headers = dict(request.headers)

    if not settings.CLERK_WEBHOOK_SECRET:
        logger.error("CLERK_WEBHOOK_SECRET not set — cannot verify webhook signatures")
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "Webhook processing unavailable")

    try:
        wh = Webhook(settings.CLERK_WEBHOOK_SECRET)
        event = wh.verify(payload, headers)
    except WebhookVerificationError:
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "Invalid signature") from None

    event_type = event.get("type")
    data = event.get("data", {})

    # ── Idempotency check ──────────────────────────────────────────
    # Svix retries the same message (same svix-id) on any non-2xx or
    # network failure.  Without this guard, a transient hiccup in our
    # handler causes Clerk to redeliver and we re-run every side
    # effect.  Most ops are upserts so it's mostly benign, but
    # enforce_camera_cap is a read-then-write that can fire duplicate
    # transition notifications, and a future non-idempotent handler
    # added without this guard in mind would silently double-execute.
    #
    # Strategy: process-then-mark.  If the handler raises midway, the
    # row isn't recorded and Svix retries — operations are designed
    # to be safe to re-run, so the eventual consistency wins.  Only
    # an already-recorded msg short-circuits.
    #
    # Read the id from BOTH header conventions.  `wh.verify()` above
    # accepts either the `svix-*` headers or the Standard-Webhooks
    # `webhook-*` set (and raises if a complete set is absent from both),
    # so post-verify exactly one namespace is populated.  Clerk sends
    # `svix-*` today, but if it ever migrates to `webhook-*` the signature
    # would still verify while a `svix-id`-only read went None — that
    # would silently disable idempotency (the business-write commit at
    # the end of this handler is unconditional, so no state would be
    # lost — re-delivery would just re-run the idempotent handlers).
    svix_msg_id = headers.get("svix-id") or headers.get("webhook-id")
    if svix_msg_id:
        already = (
            db.query(ProcessedWebhook)
            .filter_by(svix_msg_id=svix_msg_id)
            .first()
        )
        if already:
            logger.info(
                "Webhook %s already processed (event=%s) — skipping",
                svix_msg_id, already.event_type or "?",
            )
            return {"status": "duplicate", "svix_id": svix_msg_id}

    logger.info("Webhook received: %s", event_type)

    # ── Subscription lifecycle ──────────────────────────────────────
    if event_type in ("subscription.created", "subscription.updated", "subscription.active"):
        org_id = data.get("payer", {}).get("organization_id")
        if org_id:
            from app.core.plans import invalidate_effective_plan_cache
            plan_slug = get_active_plan_slug(data.get("items", []))
            # Distinguish "an item is genuinely ACTIVE" from "a canceled
            # item is still paid-through": both yield a paid slug, but
            # only the former means a real (re-)subscription.
            has_active_item = any(
                i.get("status") == "active" and (i.get("plan") or {}).get("slug")
                for i in data.get("items", [])
            )
            limit = PLAN_MEMBER_LIMITS.get(plan_slug, PLAN_MEMBER_LIMITS["free_org"])
            set_org_member_limit(org_id, limit)
            # Persist plan in DB so API-key-authenticated endpoints can look it up
            Setting.set(db, org_id, "org_plan", plan_slug, commit=False)
            # If the subscription is now on a paid plan, clear any lingering
            # past-due flag. Clerk only emits subscription.active/updated once
            # the payment has actually gone through, so seeing this event with
            # a paid plan means the card is good again — the org should get
            # their paid caps back immediately, not after the next
            # paymentAttempt.updated trickles in. Without this clear, an org
            # that upgrades *during* the grace window stays capped at free
            # because effective_plan_for_caps still sees past_due=true.
            if plan_slug in PAID_PLAN_SLUGS_WEBHOOK:
                Setting.set(db, org_id, "payment_past_due", "false", commit=False)
                Setting.set(db, org_id, "payment_past_due_at", "", commit=False)
                if has_active_item:
                    # Re-subscribe / un-cancel: an ACTIVE paid item
                    # supersedes any pending scheduled cancellation.  A
                    # canceled-but-paid-through snapshot must NOT clear
                    # the flag — Clerk emits subscription.updated right
                    # alongside subscriptionItem.canceled, and erasing
                    # the marker here made it unreliable for any future
                    # "cancels at period end" banner.
                    Setting.set(db, org_id, "plan_cancel_pending", "", commit=False)
            # Invalidate AFTER the writes (and just before the flush the
            # cap re-evaluation reads) so a concurrent reader can't
            # re-prime the cache with the old plan in the gap.
            invalidate_effective_plan_cache(org_id)
            # Re-evaluate camera cap — a plan change (up or down) may flip
            # rows in either direction. Flushing the Setting first ensures
            # `resolve_org_plan` inside enforce_camera_cap reads the new value.
            db.flush()
            result = enforce_camera_cap(db, org_id)
            if result["changed"]:
                logger.info(
                    "Org %s plan change: disabled=%d enabled=%d",
                    org_id, len(result["disabled"]), len(result["enabled"]),
                )
            logger.info("Org %s subscription active on plan '%s'", org_id, plan_slug)

    # ── Subscription item activated (upgrade landed) ────────────────
    # Clerk's authoritative "this plan's payment went through" signal is
    # the ITEM-level event; if an upgrade is delivered only via
    # subscriptionItem.* the org's plan self-heals through
    # resolve_org_plan, but the Clerk member limit never moved — a
    # paying org stayed capped at 2 seats until some subscription.*
    # event happened to fire.
    elif event_type == "subscriptionItem.active":
        org_id = data.get("payer", {}).get("organization_id")
        item_slug = (data.get("plan") or {}).get("slug")
        if org_id and item_slug in PLAN_MEMBER_LIMITS:
            from app.core.plans import invalidate_effective_plan_cache
            set_org_member_limit(org_id, PLAN_MEMBER_LIMITS[item_slug])
            Setting.set(db, org_id, "org_plan", item_slug, commit=False)
            Setting.set(db, org_id, "plan_cancel_pending", "", commit=False)
            if item_slug in PAID_PLAN_SLUGS_WEBHOOK:
                Setting.set(db, org_id, "payment_past_due", "false", commit=False)
                Setting.set(db, org_id, "payment_past_due_at", "", commit=False)
            # After the writes — see the subscription.* branch.
            invalidate_effective_plan_cache(org_id)
            db.flush()
            result = enforce_camera_cap(db, org_id)
            if result["changed"]:
                logger.info(
                    "Org %s item-activated plan change: disabled=%d enabled=%d",
                    org_id, len(result["disabled"]), len(result["enabled"]),
                )
            logger.info("Org %s subscription item active on plan '%s'", org_id, item_slug)

    # ── Payment failure ─────────────────────────────────────────────
    elif event_type in ("subscription.pastDue", "subscriptionItem.pastDue"):
        org_id = data.get("payer", {}).get("organization_id")
        if org_id:
            # Record past-due timestamp for grace period tracking.
            # Clerk will retry payment via Stripe dunning. We keep current
            # plan access during the grace period but flag the org.
            #
            # Only stamp the timestamp when the org is ENTERING past-due.
            # Clerk re-emits pastDue per dunning retry while the card
            # keeps failing; overwriting the anchor on each one restarted
            # the 7-day grace clock every cycle — an org with a dead card
            # could ride paid caps indefinitely.
            from app.core.plans import invalidate_effective_plan_cache
            already_past_due = (
                Setting.get(db, org_id, "payment_past_due", "false") == "true"
            )
            Setting.set(db, org_id, "payment_past_due", "true", commit=False)
            # After the write — see the subscription.* branch.
            invalidate_effective_plan_cache(org_id)
            if not already_past_due:
                # Clerk billing payloads use snake_case epoch-MILLISECOND
                # ints for *_at fields; normalize to ISO so
                # effective_plan_for_caps' fromisoformat parse works.
                # (Read camelCase too in case the shape ever shifted.)
                raw_at = data.get("past_due_at") or data.get("pastDueAt")
                past_due_at = datetime.now(tz=UTC).isoformat()
                if raw_at is not None:
                    try:
                        val = float(raw_at)
                        if val > 1e12:  # epoch ms
                            val /= 1000.0
                        past_due_at = datetime.fromtimestamp(val, tz=UTC).isoformat()
                    except (TypeError, ValueError):
                        # Already a string timestamp — keep as-is.
                        past_due_at = str(raw_at)
                Setting.set(db, org_id, "payment_past_due_at", past_due_at, commit=False)
            logger.warning("Org %s subscription is past due — payment failed", org_id)

    # ── Payment attempt result ──────────────────────────────────────
    elif event_type == "paymentAttempt.updated":
        org_id = data.get("payer", {}).get("organization_id")
        payment_status = data.get("status")
        if org_id and payment_status == "paid":
            # Payment succeeded — clear past-due flag and also the
            # timestamp so a future past-due event starts a fresh grace
            # window rather than counting from whenever the old one began.
            Setting.set(db, org_id, "payment_past_due", "false", commit=False)
            Setting.set(db, org_id, "payment_past_due_at", "", commit=False)
            # This branch previously never invalidated — a 30s-stale
            # "free_org" effective plan could outlive the payment fix.
            from app.core.plans import invalidate_effective_plan_cache
            invalidate_effective_plan_cache(org_id)
            # Re-run enforcement so any cameras that got suspended when
            # the grace window expired come back online immediately.
            # effective_plan_for_caps now returns the nominal plan again.
            db.flush()
            result = enforce_camera_cap(db, org_id)
            if result["changed"]:
                logger.info(
                    "Org %s payment restored: re-enabled %d camera(s)",
                    org_id, len(result["enabled"]),
                )
            logger.info("Org %s payment succeeded — past-due cleared", org_id)
        elif org_id and payment_status == "failed":
            logger.warning("Org %s payment attempt failed", org_id)

    # ── Cancellation (scheduled — payer keeps features until period end) ──
    elif event_type == "subscriptionItem.canceled":
        org_id = data.get("payer", {}).get("organization_id")
        if org_id:
            # Clerk fires this the moment the payer clicks cancel, but
            # per Clerk's billing semantics they RETAIN plan features
            # until the current period ends (the .ended event).  An
            # earlier version downgraded to free right here — revoking
            # a paid-through month on day 1, and turning a scheduled
            # pro_plus→pro downgrade into a drop to FREE.  Record the
            # pending cancel for UI/banners and leave entitlements alone.
            Setting.set(db, org_id, "plan_cancel_pending", "true", commit=False)
            logger.info(
                "Org %s cancellation scheduled — plan retained until period end",
                org_id,
            )

    # ── Period actually ended — resolve what the org is entitled to NOW ──
    elif event_type == "subscriptionItem.ended":
        org_id = data.get("payer", {}).get("organization_id")
        if org_id:
            import asyncio as _asyncio

            from app.core.plans import (
                fetch_live_plan_slug,
                invalidate_effective_plan_cache,
            )

            # One item ending doesn't necessarily mean "free": a
            # scheduled pro_plus→pro downgrade ends the pro_plus item
            # while a pro item becomes active.  Ask Clerk what the org
            # is entitled to right now instead of assuming free.
            # (to_thread: the SDK call is sync + network-bound; this
            # handler runs on the event loop.)
            live_slug = await _asyncio.to_thread(fetch_live_plan_slug, org_id)
            if live_slug is None:
                # Clerk lookup failed.  Choose the revenue-safe direction
                # (free) — the hourly plan reconciler restores a paid slug
                # within the hour if this was a scheduled downgrade.
                live_slug = "free_org"
                logger.warning(
                    "Org %s item ended but live plan lookup failed — "
                    "defaulting to free until the reconciler verifies",
                    org_id,
                )
            limit = PLAN_MEMBER_LIMITS.get(live_slug, PLAN_MEMBER_LIMITS["free_org"])
            set_org_member_limit(org_id, limit)
            Setting.set(db, org_id, "org_plan", live_slug, commit=False)
            Setting.set(db, org_id, "plan_cancel_pending", "", commit=False)
            Setting.set(db, org_id, "payment_past_due", "false", commit=False)
            # After the writes — see the subscription.* branch.
            invalidate_effective_plan_cache(org_id)
            # Suspend over-cap cameras for the new (possibly lower) tier.
            # Rows are preserved (not deleted) so a re-subscribe immediately
            # re-enables them without any reconfiguration.
            db.flush()
            result = enforce_camera_cap(db, org_id)
            if result["changed"]:
                logger.info(
                    "Org %s period end: disabled %d over-cap camera(s)",
                    org_id, len(result["disabled"]),
                )
            logger.info(
                "Org %s subscription period ended — now on plan '%s'",
                org_id, live_slug,
            )

    # ── Free trial ending soon ──────────────────────────────────────
    elif event_type == "subscriptionItem.freeTrialEnding":
        org_id = data.get("payer", {}).get("organization_id")
        if org_id:
            logger.info("Org %s free trial ending in 3 days", org_id)

    # ── Membership lifecycle (security audit) ──────────────────────
    # Three sibling events fire for the same org's user list churn.
    # Each emits an admin notification so a "did someone just add
    # themselves to my org?" question is answered within seconds
    # rather than the next time someone reads the audit log.  The
    # actor (who DID this — Clerk dashboard admin, the user
    # themselves accepting an invite, etc.) isn't always present in
    # the payload, so the body describes the result rather than the
    # cause; admins can correlate via timing if needed.
    elif event_type == "organizationMembership.created":
        org_data = data.get("organization") or {}
        user_data = data.get("public_user_data") or {}
        org_id = org_data.get("id")
        if org_id:
            try:
                from app.api.notifications import create_notification
                identifier = user_data.get("identifier") or user_data.get("user_id") or "unknown user"
                role = (data.get("role") or "").replace("org:", "") or "member"
                create_notification(
                    org_id=org_id,
                    kind="member_added",
                    title=f"Member added: {identifier}",
                    body=(
                        f"{identifier} was just added to your organization "
                        f"with the {role} role.  If this was via an invite "
                        f"you sent, no action needed.  If you don't recognize "
                        f"this user, audit the org's member list and remove "
                        f"any unexpected accounts."
                    ),
                    severity="warning" if role == "admin" else "info",
                    audience="admin",
                    link="/settings",
                    meta={
                        "user_id": user_data.get("user_id"),
                        "identifier": identifier,
                        "role": role,
                    },
                    db=db,
                )
            except Exception:
                logger.exception(
                    "[ClerkWebhook] member_added notification failed for org=%s",
                    org_id,
                )

    elif event_type == "organizationMembership.updated":
        org_data = data.get("organization") or {}
        user_data = data.get("public_user_data") or {}
        org_id = org_data.get("id")
        if org_id:
            try:
                from app.api.notifications import create_notification
                identifier = user_data.get("identifier") or user_data.get("user_id") or "unknown user"
                role = (data.get("role") or "").replace("org:", "") or "member"
                create_notification(
                    org_id=org_id,
                    kind="member_role_changed",
                    title=f"Member role changed: {identifier}",
                    body=(
                        f"{identifier}'s role in your organization is now "
                        f"{role}.  Role changes — especially promotions to "
                        f"admin — are security-relevant.  If you didn't "
                        f"authorize this change, audit your org's member "
                        f"list immediately."
                    ),
                    # Role escalations to admin are always warning-worthy;
                    # demotions / member-tier changes are informational.
                    severity="warning" if role == "admin" else "info",
                    audience="admin",
                    link="/settings",
                    meta={
                        "user_id": user_data.get("user_id"),
                        "identifier": identifier,
                        "new_role": role,
                    },
                    db=db,
                )
            except Exception:
                logger.exception(
                    "[ClerkWebhook] member_role_changed notification failed for org=%s",
                    org_id,
                )

    elif event_type == "organizationMembership.deleted":
        org_data = data.get("organization") or {}
        user_data = data.get("public_user_data") or {}
        org_id = org_data.get("id")
        if org_id:
            try:
                from app.api.notifications import create_notification
                identifier = user_data.get("identifier") or user_data.get("user_id") or "unknown user"
                create_notification(
                    org_id=org_id,
                    kind="member_removed",
                    title=f"Member removed: {identifier}",
                    body=(
                        f"{identifier} was just removed from your "
                        f"organization.  No further access from this user.  "
                        f"If you didn't expect this removal, audit the org's "
                        f"recent admin activity."
                    ),
                    severity="info",
                    audience="admin",
                    link="/settings",
                    meta={
                        "user_id": user_data.get("user_id"),
                        "identifier": identifier,
                    },
                    db=db,
                )
            except Exception:
                logger.exception(
                    "[ClerkWebhook] member_removed notification failed for org=%s",
                    org_id,
                )

    # ── Organization created ──────────────────────────────────────
    # First-touch welcome — fires once when a user creates an org via
    # Clerk's CreateOrganization modal (on signup or later).  The
    # creator is automatically the org's first admin, so the
    # ``audience="admin"`` recipient resolution lands the email on
    # them.  Wrapped in try/except — a recipient-lookup race or a
    # template-render bug must NOT cause Svix to retry the webhook
    # forever (the org IS created either way; we'd just keep
    # double-emailing on each retry).
    elif event_type == "organization.created":
        org_id = data.get("id")
        org_name = data.get("name") or "your organization"
        if org_id:
            try:
                from app.api.notifications import create_notification
                create_notification(
                    org_id=org_id,
                    kind="welcome",
                    title=f"Welcome to Sentinel, {org_name}",
                    body=(
                        "Your Sentinel workspace is ready.  "
                        "Three steps to your first live feed: install "
                        "CloudNode on the host where your cameras live, "
                        "wait ~30 seconds for it to register, then add "
                        "your first camera from Settings → Cameras.  "
                        "Full docs at /docs."
                    ),
                    severity="info",
                    audience="admin",
                    link="/dashboard",
                    meta={
                        "org_name": org_name,
                        "created_by": data.get("created_by"),
                    },
                    db=db,
                )
            except Exception:
                logger.exception(
                    "[ClerkWebhook] welcome notification failed for org=%s",
                    org_id,
                )

    # ── Organization deleted ───────────────────────────────────────
    elif event_type == "organization.deleted":
        org_id = data.get("id")
        if org_id:
            # Clean up in-memory caches FIRST so the segment-cache
            # entries don't outlive the camera rows.  Then route
            # through the shared GDPR helper for the actual DB
            # cascade — same path the customer-facing
            # /api/gdpr/delete-organization endpoint uses, so a
            # Clerk-initiated org deletion and an in-app one
            # produce identical end-states.
            #
            # Previously this handler only cleared 7 tables
            # (CameraNode→Camera, CameraGroup, McpApiKey,
            # McpActivityLog, StreamAccessLog, AuditLog, Setting),
            # leaving motion events, notifications, incidents,
            # email outbox/logs, monthly usage, and user-notification
            # state to silently outlive the org.  Article 17
            # violation; fixed by routing through delete_org_data.
            from app.api.hls import cleanup_camera_cache
            from app.core.gdpr import delete_org_data

            nodes = db.query(CameraNode).filter_by(org_id=org_id).all()
            camera_count = 0
            for node in nodes:
                for camera in (node.cameras or []):
                    cleanup_camera_cache(camera.camera_id)
                    camera_count += 1

            counts = delete_org_data(db, org_id)
            db.commit()

            logger.info(
                "Org %s deleted — cleaned %d nodes, %d cameras + %s",
                org_id, len(nodes), camera_count, counts,
            )

    # Persist the handler's business writes FIRST, in their own commit.
    # Every branch above stages via Setting.set(..., commit=False) /
    # ORM mutations and leaves the commit to us — if THIS commit fails
    # (lock timeout, disk full), we must raise so Svix sees a 5xx and
    # retries.  It must NOT share a try/except with the dedup-marker
    # insert below: an earlier version committed both together inside
    # the marker's blanket except, which converted any commit failure
    # into a silent rollback + 200 — the plan change was dropped and
    # Svix never retried.
    db.commit()

    # Mark this msg id as processed so Svix retries short-circuit.
    # Done at the end so a handler that raises midway doesn't record
    # itself as done — Svix retries, idempotent ops re-run, eventual
    # consistency.  Only the dedup-unique-constraint race is benign;
    # any other failure propagates (500 → Svix retry → idempotent
    # re-run, since the marker was never recorded).
    if svix_msg_id:
        try:
            db.add(ProcessedWebhook(
                svix_msg_id=svix_msg_id, event_type=event_type or "",
            ))
            db.commit()
        except IntegrityError:
            # Race: another worker recorded the same id between our
            # check and our insert.  Unique-constraint failure is
            # benign — both runs produce the same final state and the
            # response below still tells Svix we're done.
            db.rollback()
            logger.info(
                "Webhook %s dedup insert raced with concurrent worker — ignoring",
                svix_msg_id,
            )

    return {"received": True}


# ── Resend webhook ─────────────────────────────────────────────────
# Resend signs webhooks via Svix, same library Clerk uses, so we
# verify and dedupe with the identical pattern as ``/api/webhooks/clerk``.
#
# Events we handle:
#   - email.bounced     → insert EmailSuppression so we stop sending
#   - email.complained  → insert EmailSuppression (marked spam by user)
#   - email.delivered   → optional outbox-row update (informational)
#
# Other event types (opened, clicked, scheduled, etc.) are accepted
# (200 OK) but not acted on for v1.  The 200 keeps Resend from
# disabling our endpoint for "unhandled events."

# Reasons we'll record on EmailSuppression rows.  ``email.bounced``
# is anything Resend's SMTP-level retries gave up on (hard bounces);
# ``email.complained`` is the user clicking "spam" in their client.
# Both are signals to stop sending — re-sending after either dings
# our deliverability reputation across ALL recipients.
_RESEND_SUPPRESSION_EVENTS = {
    "email.bounced": "bounce",
    "email.complained": "complaint",
}


@router.post("/resend")
@limiter.limit("600/minute")
async def resend_webhook(request: Request, db: Session = Depends(get_db)):
    """Receive Resend delivery events (bounce, complaint, etc.).

    Mirrors the Clerk webhook pattern exactly: HMAC verification via
    Svix, idempotency via ``ProcessedWebhook``, dispatch by event type.
    Reuses the existing ``ProcessedWebhook`` table because Svix message
    IDs are UUIDs — collision between Clerk and Resend is statistically
    impossible, and even if one occurred the unique constraint would
    fail safely.

    The 600/min rate limit is generous because Resend bursts events
    when a delivery campaign completes.  Real volume is far below this.
    """
    payload = await request.body()
    headers = dict(request.headers)

    if not settings.RESEND_WEBHOOK_SECRET:
        # Without the secret we can't verify signatures, so refusing
        # is the only safe thing.  An attacker who knows the URL but
        # not the secret would otherwise be able to forge bounce
        # events to suppress legitimate users.
        logger.error(
            "RESEND_WEBHOOK_SECRET not set — cannot verify Resend signatures"
        )
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "Webhook processing unavailable")

    try:
        wh = Webhook(settings.RESEND_WEBHOOK_SECRET)
        event = wh.verify(payload, headers)
    except WebhookVerificationError:
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "Invalid signature") from None

    # Resend's event payload shape:
    #   {"type": "email.bounced", "data": {"email_id": "...", "to": ["..."], ...}, "created_at": "..."}
    event_type = event.get("type") or ""
    data = event.get("data") or {}

    # ── Idempotency check (mirrors Clerk handler) ──────────────────
    # Read both header conventions — see the Clerk handler for the full
    # rationale (svix.verify accepts svix-* or webhook-*; reading only
    # svix-id would silently break dedup under the webhook-* variant).
    svix_msg_id = headers.get("svix-id") or headers.get("webhook-id")
    if svix_msg_id:
        already = (
            db.query(ProcessedWebhook)
            .filter_by(svix_msg_id=svix_msg_id)
            .first()
        )
        if already:
            logger.info(
                "Resend webhook %s already processed (event=%s) — skipping",
                svix_msg_id, already.event_type or "?",
            )
            return {"status": "duplicate", "svix_id": svix_msg_id}

    logger.info("Resend webhook received: %s", event_type)

    # ── Dispatch ────────────────────────────────────────────────────
    if event_type in _RESEND_SUPPRESSION_EVENTS:
        reason = _RESEND_SUPPRESSION_EVENTS[event_type]
        addresses = _extract_addresses(data)
        for addr in addresses:
            _insert_suppression(db, addr, reason=reason, source="resend_webhook")

        # Mark the originating outbox row 'suppressed' if we can find
        # it via the email_id Resend sends.  Updating after-the-fact
        # is informational — the suppression-list check on the next
        # send is what actually stops further attempts.
        email_id = data.get("email_id")
        if email_id:
            try:
                row = (
                    db.query(EmailOutbox)
                    .filter(EmailOutbox.resend_message_id == email_id)
                    .first()
                )
                if row and row.status == "sent":
                    row.status = "suppressed"
                    row.error = f"webhook_event:{event_type}:reason={reason}"
                    db.commit()
            except Exception:
                logger.exception(
                    "[ResendWebhook] failed to mark outbox row suppressed for email_id=%s",
                    email_id,
                )
                db.rollback()

    elif event_type == "email.delivered":
        # Informational — we already marked the row 'sent' when the
        # API call returned.  Nothing to do, but logged so an operator
        # can correlate "sent at T+0" with "delivered at T+5s" if a
        # support ticket comes in about latency.
        email_id = data.get("email_id")
        if email_id:
            logger.info(
                "[ResendWebhook] delivered confirmation for email_id=%s",
                email_id,
            )

    # Mark this message id as processed so Resend retries short-circuit.
    if svix_msg_id:
        try:
            db.add(ProcessedWebhook(
                svix_msg_id=svix_msg_id, event_type=event_type or "",
            ))
            db.commit()
        except Exception:
            db.rollback()
            logger.info(
                "Resend webhook %s dedup insert raced — ignoring",
                svix_msg_id,
            )

    return {"received": True}


def _extract_addresses(data: dict) -> list[str]:
    """Pull recipient addresses out of a Resend event payload.

    Resend sometimes sends ``to`` as a list, sometimes as a string,
    depending on the event type and SDK version.  Handle both, plus
    the edge case where it's missing entirely (we get nothing useful
    so we don't suppress anyone — better than suppressing the wrong
    address)."""
    to = data.get("to")
    if isinstance(to, list):
        return [a for a in to if isinstance(a, str) and "@" in a]
    if isinstance(to, str) and "@" in to:
        return [to]
    return []


def _insert_suppression(
    db: Session, address: str, *, reason: str, source: str
) -> None:
    """Insert into EmailSuppression, swallowing duplicate-key errors.

    Address is lower-cased to match the worker's case-insensitive
    suppression check (test_email_worker.py covers this).  Race
    between two Resend retries delivering the same bounce gets
    handled by the unique constraint — we treat the rollback as
    benign (the address is already suppressed; mission accomplished).
    """
    addr = (address or "").strip().lower()
    if not addr or "@" not in addr:
        return
    try:
        db.add(EmailSuppression(address=addr, reason=reason, source=source))
        db.commit()
        logger.info(
            "[ResendWebhook] suppressed address=%s reason=%s source=%s",
            _redact_addr(addr), reason, source,
        )
    except Exception:
        db.rollback()
        # Either an existing row (benign) or a real error.  Log at
        # debug to keep the steady-state webhook noise down.
        logger.debug(
            "[ResendWebhook] suppression insert failed (likely duplicate) "
            "for address=%s reason=%s",
            _redact_addr(addr), reason,
        )


def _redact_addr(addr: str) -> str:
    """Same redaction shape as app/core/email.py — keep PII out of logs."""
    if not addr or "@" not in addr:
        return "***"
    local, _, domain = addr.partition("@")
    return f"{local[0]}***@{domain}" if local else f"***@{domain}"
