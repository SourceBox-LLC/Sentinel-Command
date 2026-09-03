"""Client for the separate Sentinel License Service — gates Sentinel AI
access for self-hosted installs (AUTH_PROVIDER=local). See
Sentinel-License-Service/README.md (sibling repo) for the service
itself, and this repo's plan doc for the full design.

This module owns both sides of the check-in: the outbound HTTP call
(``check_in_with_license_service``) and the cached read
(``is_sentinel_licensed``) that the actual Sentinel gate consults. The
cache lives in the same ``Setting`` KV table Clerk's billing webhook
already writes into for hosted orgs — no new table needed.

Fail-open / fail-closed semantics (four states):
  1. ``SENTINEL_LICENSE_KEY`` never configured -> hard fail-closed,
     always, no grace.
  2. Last check-in reachable and said ``valid: true`` -> full access.
  3. Most recent check-in was unreachable (network/5xx — NOT the
     service explicitly answering) -> fail open for
     ``SENTINEL_LICENSE_GRACE_HOURS`` from the last time it WAS
     reachable and said yes, then fail closed.
  4. Most recent check-in was reachable and said ``valid: false``
     (revoked/expired/suspended/unknown) -> fail closed immediately,
     no grace at all.

The key distinction driving 3 vs. 4 is whether the *current* attempt
reached the service, tracked separately from what the last successful
answer was — see ``_LAST_CHECK_REACHABLE`` below. Without that split,
a stale "yes" from days ago could be mistaken for a fresh "no", or vice
versa, once check-ins start failing.
"""

from __future__ import annotations

import logging
import secrets
from datetime import UTC, datetime, timedelta

import httpx
from sqlalchemy.orm import Session

from app.core.config import settings

logger = logging.getLogger(__name__)

# Distinct from and much shorter than PAYMENT_GRACE_DAYS (7 days) —
# that grace exists for a slow billing-dunning cycle measured in days;
# a validation-service *outage* is an infra-availability question
# measured in hours. Long enough to absorb a redeploy or a weekend
# incident without punishing a paying customer; short enough that a
# permanently broken/abandoned setup doesn't grant free access forever.
SENTINEL_LICENSE_GRACE_HOURS = 72

# Log the grace-mode warning at most this often once grace is active,
# so an extended outage doesn't spam a warning on every 15-minute tick.
_GRACE_WARNING_REPEAT_HOURS = 12

_CHECKIN_TIMEOUT_SECONDS = 10.0

# Setting keys (all under settings.LOCAL_ORG_ID — self-host has exactly
# one org, so there's no per-org fan-out to worry about).
_LICENSE_VALID = "sentinel_license_valid"
_LAST_CHECK_AT = "sentinel_license_last_check_at"
_LAST_CHECK_REACHABLE = "sentinel_license_last_check_reachable"
_LAST_OK_AT = "sentinel_license_last_ok_at"
_LAST_GRACE_WARNING_AT = "sentinel_license_last_grace_warning_at"
_INSTALL_ID = "sentinel_install_id"
# Separate opt-in entitlement on the same license (see
# app/core/sync_client.py) — a license can be Sentinel-AI-valid without
# also being sync-enabled, so this is tracked independently of
# _LICENSE_VALID rather than folded into it.
_SYNC_ENABLED = "sentinel_data_sync_enabled"


def _parse_iso_or_none(raw: str) -> datetime | None:
    """Parse a stored ISO-8601 timestamp, normalizing to tz-aware UTC.

    Every current writer in this module stores tz-aware strings (via
    ``datetime.now(tz=UTC).isoformat()``), so this never hits the naive
    branch today — but treating a naive value as UTC rather than
    letting it flow through untouched avoids a `TypeError` later at
    `datetime.now(tz=UTC) - naive_dt` (mixing aware and naive datetimes
    raises), which would otherwise turn a fail-closed gate into an
    unhandled 500 the moment any future writer, migration, or manual
    DB edit ever stores a naive timestamp here. Catches both
    `ValueError` (malformed string) and `TypeError` (non-string input)
    defensively, even though normalizing already prevents the specific
    TypeError this was written for.
    """
    try:
        parsed = datetime.fromisoformat(raw)
    except (ValueError, TypeError):
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=UTC)
    return parsed


def _get_or_create_install_id(db: Session) -> str:
    from app.models.models import Setting

    existing = Setting.get(db, settings.LOCAL_ORG_ID, _INSTALL_ID, "")
    if existing:
        return existing
    new_id = secrets.token_hex(16)
    Setting.set(db, settings.LOCAL_ORG_ID, _INSTALL_ID, new_id)
    return new_id


async def check_in_with_license_service(db: Session) -> None:
    """Attempt one check-in against the license service and persist the
    outcome into Setting. Never raises — a check-in failure is exactly
    the "unreachable" state this whole module exists to handle
    gracefully, not something callers should have to guard against.
    """
    if not settings.SENTINEL_LICENSE_KEY:
        # Nothing to check in with — is_sentinel_licensed() already
        # hard-fails closed on this exact condition (state 1).
        return

    from app.models.models import Setting

    now_iso = datetime.now(tz=UTC).isoformat()
    install_id = _get_or_create_install_id(db)
    db.commit()  # in case _get_or_create_install_id just minted a new one

    try:
        async with httpx.AsyncClient(timeout=_CHECKIN_TIMEOUT_SECONDS) as client:
            resp = await client.post(
                f"{settings.SENTINEL_LICENSE_SERVICE_URL.rstrip('/')}/v1/licenses/check-in",
                headers={"Authorization": f"Bearer {settings.SENTINEL_LICENSE_KEY}"},
                json={
                    "install_id": install_id,
                    "product": "sentinel_ai",
                    "client_version": "2.1.2",
                },
            )
        resp.raise_for_status()
        data = resp.json()
        if not isinstance(data, dict):
            raise ValueError(f"expected a JSON object, got {type(data).__name__}")
        valid = bool(data.get("valid"))
    except Exception:
        # Covers network errors, non-2xx, and a malformed/non-object
        # response body — any of these mean we can't trust the answer,
        # so treat it identically to "unreachable" (grace window
        # applies) rather than letting a parsing error escape uncaught,
        # which would skip the Setting writes below entirely and leave
        # a stale cached state in place indefinitely.
        logger.warning(
            "[SentinelLicense] check-in failed (network/5xx/malformed response) — "
            "treating as unreachable, grace window (if any) applies",
            exc_info=True,
        )
        Setting.set(db, settings.LOCAL_ORG_ID, _LAST_CHECK_AT, now_iso, commit=False)
        Setting.set(db, settings.LOCAL_ORG_ID, _LAST_CHECK_REACHABLE, "false", commit=False)
        db.commit()
        return

    Setting.set(db, settings.LOCAL_ORG_ID, _LAST_CHECK_AT, now_iso, commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, _LAST_CHECK_REACHABLE, "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, _LICENSE_VALID, "true" if valid else "false", commit=False)
    # False whenever the check-in itself came back invalid (revoked/
    # expired/suspended/not_found never populate sync_enabled in the
    # response body, so `data.get(...)` naturally reads None/False here) —
    # a license losing validity must also lose sync access immediately,
    # not keep coasting on a stale cached "true".
    Setting.set(
        db, settings.LOCAL_ORG_ID, _SYNC_ENABLED,
        "true" if valid and data.get("sync_enabled") else "false", commit=False,
    )
    if valid:
        Setting.set(db, settings.LOCAL_ORG_ID, _LAST_OK_AT, now_iso, commit=False)
    else:
        logger.info(
            "[SentinelLicense] check-in reachable, license not valid (reason=%s)",
            data.get("reason"),
        )
    db.commit()


def is_sentinel_licensed(db: Session) -> bool:
    """The read side — cheap, no network call, safe to call on every
    Sentinel-gated request. Always True for hosted (Clerk) orgs; this
    entire module is a self-host-only concern."""
    if not settings.is_local_auth():
        return True

    if not settings.SENTINEL_LICENSE_KEY:
        return False  # state 1

    from app.models.models import Setting

    reachable = Setting.get(db, settings.LOCAL_ORG_ID, _LAST_CHECK_REACHABLE, "")

    if reachable == "true":
        # states 2 & 4 — trust the most recent reachable answer
        # directly, no grace either way.
        return Setting.get(db, settings.LOCAL_ORG_ID, _LICENSE_VALID, "") == "true"

    # Never successfully reached the service, or the most recent
    # attempt failed — state 3, lean on the grace window.
    last_ok_raw = Setting.get(db, settings.LOCAL_ORG_ID, _LAST_OK_AT, "")
    if not last_ok_raw:
        return False  # never had a good check-in to grant grace from

    last_ok_at = _parse_iso_or_none(last_ok_raw)
    if last_ok_at is None:
        return False

    return datetime.now(tz=UTC) - last_ok_at <= timedelta(hours=SENTINEL_LICENSE_GRACE_HOURS)


def is_sync_enabled(db: Session) -> bool:
    """Read-side gate for the cloud data-sync tier (app/core/sync_client.py).

    Requires both: the license itself currently trusted as valid (same
    reachability/grace semantics as is_sentinel_licensed — a license
    outage grace-periods sync the same way it grace-periods Sentinel AI,
    since the risk of syncing a few extra hours past a billing hiccup is
    minor) AND the separate sync_enabled entitlement bit, since a
    license can be Sentinel-AI-valid without having bought sync.
    """
    if not settings.is_local_auth() or not settings.SENTINEL_LICENSE_KEY:
        return False

    if not is_sentinel_licensed(db):
        return False

    from app.models.models import Setting

    return Setting.get(db, settings.LOCAL_ORG_ID, _SYNC_ENABLED, "") == "true"


# The one plan slug the license gate applies to. `self_host` is
# Sentinel-eligible by plan alone (it's in SENTINEL_PLANS) but ALSO
# needs a valid license — every other plan (free_org/pro/pro_plus) is
# governed purely by the existing plan-membership check and never
# reaches this predicate's `is_sentinel_licensed` call at all.
_LICENSE_GATED_PLAN = "self_host"


def sentinel_blocked_by_license(plan: str, db: Session) -> bool:
    """True iff `plan` requires a Sentinel license and this org doesn't
    currently have a valid one.

    Single source of truth for the self-host license gate, reused at
    every Sentinel access-control call site (api/sentinel.py,
    core/sentinel_dispatch.py, mcp/server.py) — previously this exact
    condition was hand-copied at 5 call sites across 3 files with
    nothing enforcing they stayed in sync.
    """
    return plan == _LICENSE_GATED_PLAN and not is_sentinel_licensed(db)


def warn_if_in_extended_grace(db: Session) -> None:
    """Called from the background loop (not the hot read path) after an
    unreachable check-in, so an outage that eats into the grace window
    actually gets noticed. Rate-limited to once per
    _GRACE_WARNING_REPEAT_HOURS so a prolonged outage doesn't spam logs
    on every 15-minute tick.
    """
    if not settings.is_local_auth() or not settings.SENTINEL_LICENSE_KEY:
        return

    from app.models.models import Setting

    reachable = Setting.get(db, settings.LOCAL_ORG_ID, _LAST_CHECK_REACHABLE, "")
    if reachable == "true":
        return

    last_ok_raw = Setting.get(db, settings.LOCAL_ORG_ID, _LAST_OK_AT, "")
    if not last_ok_raw:
        return
    last_ok_at = _parse_iso_or_none(last_ok_raw)
    if last_ok_at is None:
        return

    age = datetime.now(tz=UTC) - last_ok_at
    if age < timedelta(hours=1):
        return  # too fresh to be worth a warning yet

    last_warning_raw = Setting.get(db, settings.LOCAL_ORG_ID, _LAST_GRACE_WARNING_AT, "")
    if last_warning_raw:
        last_warning_at = _parse_iso_or_none(last_warning_raw)
        if last_warning_at is not None and (
            datetime.now(tz=UTC) - last_warning_at < timedelta(hours=_GRACE_WARNING_REPEAT_HOURS)
        ):
            return

    remaining = timedelta(hours=SENTINEL_LICENSE_GRACE_HOURS) - age
    if remaining > timedelta(0):
        logger.warning(
            "[SentinelLicense] license service unreachable for %s — running on grace, "
            "Sentinel AI access will be pulled in %s if this isn't resolved",
            age, remaining,
        )
    else:
        logger.warning(
            "[SentinelLicense] license service unreachable for %s — grace window "
            "exhausted, Sentinel AI access is now blocked",
            age,
        )
    Setting.set(db, settings.LOCAL_ORG_ID, _LAST_GRACE_WARNING_AT, datetime.now(tz=UTC).isoformat())
