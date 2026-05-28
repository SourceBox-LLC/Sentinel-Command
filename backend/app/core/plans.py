"""
Plan configuration and limit enforcement for Sentinel billing tiers.

Plan slugs must match the keys defined in the Clerk Dashboard:
  - free_org  (Free)
  - pro       (Pro — $12/mo, or $10/mo billed annually = $120/yr)
  - pro_plus  (Pro Plus — $29/mo, or $25/mo billed annually = $300/yr)

Historical note: an earlier ``business`` slug was renamed to ``pro_plus``
during the Clerk-side plan reorg.  The transitional alias was carried
for a while to handle in-flight JWTs and unrefreshed Setting rows; it
was removed after every known org had rolled over.  See ADR
``docs/adr/0002-viewer-hour-billing.md`` for the original tier names.
"""

import logging
import threading
import time
from datetime import UTC, datetime, timedelta

logger = logging.getLogger(__name__)

# Grace window after a failed payment before we tighten the caps.  Matches
# the industry norm (7 days) and lines up with Clerk's default Stripe dunning
# schedule — by day 7 the card has been retried 3–4 times, so an org still
# past-due at that point is unlikely to recover without action. The ToS /
# pricing page both reference this number; if you change it, update those
# too. The grace window is a *soft* cap — banners and API 402s on MCP fire
# immediately; we only rebase cameras to the free tier when the grace
# expires.
PAYMENT_GRACE_DAYS = 7

PLAN_LIMITS = {
    # Hardware caps are sized as ABUSE RAILS rather than product differentiators.
    # Almost no legitimate customer will hit these — the binding constraint for
    # upgrade decisions is `max_viewer_hours_per_month` below, because that is
    # what actually drives our egress cost. If a legitimate customer needs more
    # than the Pro Plus cap, contact us and we'll bump it manually.
    #
    # Usage caps (viewer-hours, MCP calls) are the real tier axis. See the
    # `viewer_hours` aggregator in app.api.hls and the daily cap in
    # app.mcp.server for enforcement.
    "free_org": {
        "max_cameras": 5,
        "max_nodes": 2,
        "max_seats": 2,
        "max_viewer_hours_per_month": 30,
        "max_sse_subscribers": 10,
        "log_retention_days": 30,
    },
    "pro": {
        "max_cameras": 25,
        "max_nodes": 10,
        "max_seats": 10,
        "max_viewer_hours_per_month": 300,
        "max_sse_subscribers": 30,
        "log_retention_days": 90,
    },
    "pro_plus": {
        "max_cameras": 200,
        "max_nodes": 999,  # effectively unlimited
        "max_seats": 20,
        "max_viewer_hours_per_month": 1500,
        "max_sse_subscribers": 100,
        "log_retention_days": 365,
    },
}

# Slugs we trust without re-checking against Clerk.
PAID_PLAN_SLUGS = frozenset({"pro", "pro_plus"})

# Min seconds between consecutive live Clerk lookups for the same org.
# Prevents non-paid callers from generating excessive Clerk API traffic
# when they hit the MCP gate repeatedly.
_RESOLVE_THROTTLE_SECONDS = 60.0
_last_resolve_at: dict[str, float] = {}

# `_last_resolve_at` is read+written from both the event loop (async
# handlers calling resolve_org_plan via Depends) and MCP worker threads.
# A lock keeps the throttle read/write atomic AND makes the prune sweep
# below safe to iterate without racing an insert ("dict changed size
# during iteration").  Held only for the dict ops — never across the
# Clerk network call.
_resolve_lock = threading.Lock()
# An entry older than the throttle window is inert (the throttle check
# already treats it as expired), so it can be dropped.  Without this
# sweep, _last_resolve_at accumulates one entry per org that ever hit
# the live-lookup path forever — a slow leak dominated by free-tier orgs
# hammering MCP, which is exactly the throttle's target population.
_RESOLVE_PRUNE_INTERVAL = 600.0  # 10 min
_last_resolve_prune_at: float = 0.0


def _prune_resolve_cache(now: float) -> None:
    """Drop `_last_resolve_at` entries older than the throttle window.

    Caller must hold `_resolve_lock`.  Time-gated to at most once per
    `_RESOLVE_PRUNE_INTERVAL` so it's a single comparison on the common
    path and an O(orgs) walk only every ~10 min.
    """
    global _last_resolve_prune_at
    if now - _last_resolve_prune_at < _RESOLVE_PRUNE_INTERVAL:
        return
    _last_resolve_prune_at = now
    cutoff = now - _RESOLVE_THROTTLE_SECONDS
    stale = [org for org, ts in _last_resolve_at.items() if ts < cutoff]
    for org in stale:
        _last_resolve_at.pop(org, None)


def get_plan_limits(plan: str) -> dict:
    """Return the limits dict for a plan slug. Falls back to free tier."""
    return PLAN_LIMITS.get(plan, PLAN_LIMITS["free_org"])


def resolve_org_plan(db, org_id: str) -> str:
    """Return the current plan slug for an org, with a Clerk fallback.

    Read order:
      1. Cached `Setting(org_plan)` — populated by the Clerk webhook
         handler in app/api/webhooks.py. If the value is a recognized
         paid plan, return immediately (fast path, no API call).
      2. Live `clerk.organizations.get_billing_subscription()` — fixes
         orgs whose subscription webhook never fired (e.g. they
         upgraded before the handler shipped, delivery failed, or the
         dashboard plan name produced a slug that didn't match a key
         in PLAN_LIMITS). The fresh slug is written back to the
         Setting so future calls hit the fast path.

    Live lookups for the same org are throttled to once per 60 seconds
    so a free-tier caller hammering MCP can't drive Clerk API spend.
    """
    from app.core.clerk import clerk
    from app.models.models import Setting

    cached = Setting.get(db, org_id, "org_plan", "")
    if cached in PAID_PLAN_SLUGS:
        return cached

    # Throttle live re-checks per org.  Lock only the dict read/modify +
    # the periodic prune — NOT the Clerk call below (which must not
    # serialize across orgs).
    now = time.monotonic()
    with _resolve_lock:
        _prune_resolve_cache(now)
        if now - _last_resolve_at.get(org_id, 0.0) < _RESOLVE_THROTTLE_SECONDS:
            return cached or "free_org"
        _last_resolve_at[org_id] = now

    try:
        sub = clerk.organizations.get_billing_subscription(organization_id=org_id)
    except Exception:
        logger.warning(
            "Live Clerk plan lookup failed for org %s — returning cached value %r",
            org_id, cached, exc_info=True,
        )
        return cached or "free_org"

    # Mirror the webhook handler's logic: take the first active item's plan slug.
    live_slug = "free_org"
    for item in (sub.subscription_items or []):
        status = getattr(item, "status", None)
        plan = getattr(item, "plan", None)
        if status == "active" and plan and getattr(plan, "slug", None):
            live_slug = plan.slug
            break

    if live_slug != cached:
        Setting.set(db, org_id, "org_plan", live_slug)
        try:
            db.commit()
            logger.info(
                "Resolved org %s plan from Clerk: cached=%r → live=%r",
                org_id, cached, live_slug,
            )
        except Exception:
            db.rollback()
            logger.exception("Failed to persist resolved plan for org %s", org_id)

    return live_slug


def get_plan_limits_for_org(db, org_id: str) -> dict:
    """Look up an org's plan from the database and return its limits.

    Used by endpoints that authenticate via API key (e.g. node registration)
    where JWT claims are not available.  Falls back to a live Clerk lookup
    if the cached Setting is stale or missing — see ``resolve_org_plan``.
    """
    plan = resolve_org_plan(db, org_id)
    limits = get_plan_limits(plan)
    # Attach the plan slug so callers can show a display name
    return {**limits, "_plan": plan}


def get_plan_display_name(plan: str) -> str:
    """Human-readable plan name."""
    names = {
        "free_org": "Free",
        "pro": "Pro",
        "pro_plus": "Pro Plus",
    }
    return names.get(plan, "Free")


def effective_plan_for_caps(db, org_id: str) -> str:
    """Return the plan slug to use for *cap enforcement*, accounting for
    the past-due grace period.

    Semantics:
      - Nominal plan (what Clerk says the org pays for) comes from
        `resolve_org_plan` — fast-path Setting read, falls back to Clerk.
      - If the org is currently past-due AND `payment_past_due_at` is older
        than ``PAYMENT_GRACE_DAYS``, return ``"free_org"`` so the enforcement
        tightens camera caps as if the subscription had already been
        cancelled. The row itself isn't touched — a successful payment
        re-enables everything immediately by flipping `payment_past_due`
        back to "false" and re-running `enforce_camera_cap`.

    Affects runtime cap enforcement (camera cap via ``enforce_camera_cap``,
    monthly viewer-hours via ``app.api.hls.get_hls_segment``).
    ``require_active_billing`` still gates MCP creation *immediately* on
    past-due (no grace) because issuing fresh credentials to an org with a
    failing card is a different risk than letting their existing cameras
    keep streaming for a week.

    Use this everywhere a runtime cap is checked.  Do NOT use it for the
    TUI status-bar badge — operators want to see their *paid* plan there,
    not a silent downgrade during a brief card failure.
    """
    from app.models.models import Setting

    nominal = resolve_org_plan(db, org_id)

    past_due = Setting.get(db, org_id, "payment_past_due", "false") == "true"
    if not past_due:
        return nominal

    past_due_at = Setting.get(db, org_id, "payment_past_due_at", "")
    if not past_due_at:
        # Flag is set but no timestamp — conservative: assume grace hasn't
        # expired yet, since we can't tell how long it's been past-due.
        # Operator-visible banner still fires; MCP still blocked.
        return nominal

    # Timestamps from Clerk come in ISO 8601 (with or without a Z suffix).
    # If we can't parse it, don't tighten — surfacing a bug loudly is
    # better than silently suspending cameras on a parse error.
    try:
        dt = datetime.fromisoformat(past_due_at.replace("Z", "+00:00"))
    except ValueError:
        logger.warning(
            "effective_plan_for_caps: unparseable payment_past_due_at %r for org %s — "
            "keeping nominal plan",
            past_due_at, org_id,
        )
        return nominal

    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=UTC)

    age = datetime.now(tz=UTC) - dt
    if age > timedelta(days=PAYMENT_GRACE_DAYS):
        logger.info(
            "effective_plan_for_caps: org %s past-due for %s (> %d days) — "
            "tightening caps to free tier",
            org_id, age, PAYMENT_GRACE_DAYS,
        )
        return "free_org"

    return nominal


def enforce_camera_cap(db, org_id: str) -> dict:
    """Enforce the org's current camera cap by flipping ``Camera.disabled_by_plan``.

    Keeps the oldest ``max_cameras`` cameras (by ``created_at`` ascending — the
    ones the org has had the longest) enabled, flags the rest as
    ``disabled_by_plan=True``. On upgrade (cap raised above current count) all
    flags are cleared. Idempotent: safe to call on every registration and
    subscription webhook with no state change when nothing needs to flip.

    Why oldest-first:
      - Deterministic, no user input required.
      - Preserves long-running cameras that almost certainly have history /
        recordings the operator cares about.
      - Newer cameras the operator just plugged in are easier to replace
        (you remember setting them up this week) than a year-old camera.

    The flag is consulted by ``POST /push-segment`` which rejects uploads
    with HTTP 402 + ``plan_limit_hit`` body when set. Enforcement is *only*
    at upload time — we don't delete rows, so on upgrade the disabled
    cameras light back up immediately with all their metadata intact.

    Returns a dict:
      {
          "plan": "free",                 # wire slug
          "max_cameras": 2,
          "disabled": ["cam_03", ...],    # camera_ids newly or still disabled
          "enabled": ["cam_01", ...],     # camera_ids newly or still enabled
          "changed": True,                # whether any row flipped
      }

    The caller commits.
    """
    from app.models.models import Camera  # local import — plans.py is
    # depended on by many modules, keep the import graph flat.

    # Use the *effective* plan — after PAYMENT_GRACE_DAYS past-due, this
    # returns "free_org" even if the nominal plan is Pro/Pro Plus, so the
    # cap tightens automatically without requiring a cancellation webhook.
    plan_slug = effective_plan_for_caps(db, org_id)
    limits = get_plan_limits(plan_slug)
    cap = int(limits["max_cameras"])

    # Ordered by created_at ASC; None last (shouldn't happen in practice
    # since `created_at` has a default, but be defensive).
    cameras = (
        db.query(Camera)
        .filter_by(org_id=org_id)
        .order_by(Camera.created_at.asc().nulls_last(), Camera.id.asc())
        .all()
    )

    keep_ids = {c.camera_id for c in cameras[:cap]}
    disable_ids = [c.camera_id for c in cameras[cap:]]

    changed = False
    enabled: list[str] = []
    disabled: list[str] = []
    for cam in cameras:
        should_disable = cam.camera_id not in keep_ids
        if bool(cam.disabled_by_plan) != should_disable:
            cam.disabled_by_plan = should_disable
            changed = True
        (disabled if should_disable else enabled).append(cam.camera_id)

    if changed:
        logger.info(
            "enforce_camera_cap: org=%s plan=%s cap=%d enabled=%d disabled=%d",
            org_id, plan_slug, cap, len(enabled), len(disabled),
        )

    return {
        "plan": wire_plan_slug(plan_slug),
        "max_cameras": cap,
        "enabled": enabled,
        "disabled": disable_ids,
        "changed": changed,
    }


def wire_plan_slug(plan: str) -> str:
    """Canonical plan string for the CloudNode wire protocol.

    Strips the internal ``_org`` suffix so the node renders a clean pill
    badge (``[ FREE ]`` rather than ``[ FREE_ORG ]``). Unknown slugs pass
    through untouched so a future tier like ``enterprise`` shows up in
    the node UI before we ship a node update.

    The CloudNode treats this field as advisory — enforcement still lives
    here in the backend — so a stale / unexpected value doesn't affect
    access, only the label in the status bar.
    """
    plan = (plan or "").strip().lower()
    if not plan:
        return "free"
    if plan.endswith("_org"):
        return plan[: -len("_org")]
    return plan
