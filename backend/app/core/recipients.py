"""
Email recipient lookup — "who do we send this to?"

The Notification model carries an ``audience`` field (``"all"`` or
``"admin"``).  This module turns that audience + an org_id into a
list of email addresses we should send to, by calling Clerk's
``organization_memberships.list`` and filtering by role.

There is no local User table — Clerk is the source of truth.  We
cache results in-process for 5 minutes so a flap event ("camera
offline / online / offline / online") doesn't burn one Clerk API
call per emit.

Suppression-list filtering happens in the worker, not here.  This
module's job ends at "here are the addresses according to Clerk."
"""

from __future__ import annotations

import logging
import time
from threading import Lock
from typing import Optional

from app.core.clerk import clerk

logger = logging.getLogger(__name__)


# ── In-process TTL cache ─────────────────────────────────────────────
# Single dict keyed on (org_id, audience) → (expires_at_monotonic, addrs).
# 5 minutes covers the worst-case flap window without serving stale
# membership long enough to matter — a member removed from an org
# stops getting emails within 5 minutes of the change, which is fine
# for security-alert latency.
#
# Tests can call ``_clear_cache`` to reset between runs.

_CACHE_TTL_SECONDS = 300

_cache: dict[tuple[str, str], tuple[float, list[str]]] = {}
_cache_lock = Lock()


def _clear_cache() -> None:
    """Reset the cache.  Used by tests; not exposed in public API."""
    with _cache_lock:
        _cache.clear()


# ── Public API ───────────────────────────────────────────────────────

# Clerk role strings.  These are Clerk's defaults for org membership;
# customers haven't been able to override them since the org-roles GA
# in late 2024.  If we ever flip on Clerk's custom-roles feature this
# constant is the one place to broaden the set.
_ADMIN_ROLES = frozenset({"org:admin", "admin"})


def get_recipient_emails(org_id: str, audience: str) -> list[str]:
    """Return email addresses that should receive a notification.

    Parameters
    ----------
    org_id
        The Clerk organization id (``"org_..."``).
    audience
        ``"all"`` returns every member of the org; ``"admin"`` returns
        only admins.  Unknown values fall through to ``"all"`` so a
        future audience type doesn't silently drop emails on the floor.

    Returns
    -------
    A deduplicated list of email addresses.  Empty list on any
    failure (missing org, Clerk outage, malformed responses).  Empty
    list is the safe default — beats erroring out and stopping the
    worker tick.
    """
    audience = audience if audience in ("all", "admin") else "all"
    cache_key = (org_id, audience)

    # Cache check — racy but the read-then-write is benign.  Worst
    # case two concurrent callers both hit Clerk on the first miss
    # and one of the writes wins.
    with _cache_lock:
        cached = _cache.get(cache_key)
    if cached is not None:
        expires_at, addrs = cached
        if expires_at > time.monotonic():
            return list(addrs)  # defensive copy so callers can't mutate cache

    addrs = _fetch_from_clerk(org_id, audience)

    if addrs is None:
        # Fetch FAILED (Clerk outage / network error) — distinct from a
        # genuinely empty membership list.  Do NOT cache the failure:
        # the enqueue path writes zero outbox rows when recipients are
        # empty, so caching [] for the TTL would convert one transient
        # Clerk hiccup into 5 minutes of silently dropped alert emails
        # with no retry (the outbox IS the retry mechanism, and it was
        # never reached).  Return empty for THIS call only; the next
        # notification re-attempts the lookup immediately.
        return []

    with _cache_lock:
        _cache[cache_key] = (time.monotonic() + _CACHE_TTL_SECONDS, list(addrs))

    return list(addrs)


def invalidate_org(org_id: str) -> None:
    """Drop cached entries for an org.

    Hook this from the Clerk webhook for ``organizationMembership.created``
    / ``.deleted`` so a member added or removed sees the change in
    under 5 minutes (immediately, in fact).  Until that wiring lands
    the natural TTL handles staleness.
    """
    with _cache_lock:
        for key in [k for k in _cache if k[0] == org_id]:
            _cache.pop(key, None)


# ── Internals ────────────────────────────────────────────────────────

def _fetch_from_clerk(org_id: str, audience: str) -> list[str] | None:
    """Call Clerk to list memberships and extract email addresses.

    Returns ``None`` on a FAILED fetch (Clerk outage, network error) so
    the caller can skip caching it, vs ``[]`` for an org that genuinely
    has no matching members (cacheable).  Errors are logged, never
    raised — a Clerk outage must degrade to "no email this time", not
    crash the notification path.  Because failures aren't cached, the
    very next notification retries the lookup immediately.

    Pagination: we cap at 100 members per org (Clerk's max page size)
    because a single org is unlikely to exceed that in our target
    market.  If we later land an enterprise customer with 200 admins
    on one org we'll need to walk pages — for now, an info-level log
    flags when we hit the cap so the limitation is visible.
    """
    try:
        result = clerk.organization_memberships.list(
            organization_id=org_id,
            limit=100,
        )
    except Exception:
        logger.exception(
            "[Recipients] Clerk list failed for org=%s audience=%s",
            org_id, audience,
        )
        return None

    members = getattr(result, "data", None) or []
    if not members:
        logger.info(
            "[Recipients] org=%s has zero memberships per Clerk", org_id,
        )
        return []

    if len(members) >= 100:
        logger.info(
            "[Recipients] org=%s hit 100-member page cap — additional "
            "members will not receive emails until pagination ships",
            org_id,
        )

    addrs: list[str] = []
    seen: set[str] = set()
    for m in members:
        if audience == "admin":
            role = getattr(m, "role", "") or ""
            if role not in _ADMIN_ROLES:
                continue
        addr = _extract_email(m)
        if not addr:
            continue
        # Lower-case dedup so "Alice@Example.com" and "alice@example.com"
        # don't both get emailed.
        canonical = addr.lower()
        if canonical in seen:
            continue
        seen.add(canonical)
        addrs.append(addr)

    return addrs


def _extract_email(membership) -> Optional[str]:
    """Pull an email address out of a Clerk OrganizationMembership.

    The ``public_user_data.identifier`` field is the user's primary
    identifier — for email-auth users (the default for SaaS apps,
    including ours) this is the email.  Non-email auth flows would
    leave a username here, which we reject below.
    """
    public = getattr(membership, "public_user_data", None)
    if public is None:
        return None
    identifier = getattr(public, "identifier", None)
    if not identifier or "@" not in identifier:
        # Non-email identifier (username, phone, etc.) — skip.
        return None
    return identifier
