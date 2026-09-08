"""
Sentinel API — config + run history for the autonomous security agent.

Slice 1 of the Sentinel rollout: this module covers persistence only.
The agent itself isn't wired up yet — `sentinel_runs` rows will only
start appearing once slice 3 ships.  See plans/ for the 7-slice
roadmap.

Endpoints:
  - GET   /api/sentinel/config       fetch (creates default row on first call)
  - PATCH /api/sentinel/config       partial update (PRO PLUS only)
  - GET   /api/sentinel/runs         paginated run history + small stats
  - GET   /api/sentinel/runs/{id}    single run detail with tool trace

Plan gating:
  - GET endpoints return 200 with `plan_gated: true` for non-Pro-Plus
    orgs so the read-only UI can render.
  - PATCH returns 402 for non-Pro-Plus orgs (write requires plan).

Pattern notes:
  - PATCH semantics with `exclude_unset=True` mirror the email-prefs
    pattern at notifications.py:1080-1108 — partial updates are the
    norm, frontend toggles fire one at a time, no stale-clobber.
  - Plan resolution via `effective_plan_for_caps()` — JWT claims can
    be stale; this respects past-due grace.
  - Audit row written on every PATCH so admin actions are traceable
    (matches email-prefs at notifications.py:1110).
"""

import hashlib
import hmac
import logging
import secrets
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Optional

from fastapi import APIRouter, Depends, Header, HTTPException, Query, Request
from pydantic import BaseModel, Field
from sqlalchemy.exc import IntegrityError
from sqlalchemy.orm import Session

from app.core.audit import audit_label, write_audit
from app.core.auth import AuthUser, require_active_billing, require_admin, require_view
from app.core.config import settings
from app.core.database import get_db
from app.core.license_client import sentinel_blocked_by_license
from app.core.limiter import limiter
from app.core.plans import effective_plan_for_caps, get_plan_display_name
from app.core.sentinel_dispatch import (
    SENTINEL_PLANS,
    cap_for_plan,
    dispatch_manual_run,
    runs_used_this_month,
)
from app.models.models import (
    Incident,
    SentinelAgentKey,
    SentinelConfig,
    SentinelRun,
    Setting,
)

logger = logging.getLogger(__name__)
router = APIRouter(prefix="/api/sentinel", tags=["sentinel"])


# ── PATCH body model ────────────────────────────────────────────────
class SentinelConfigPatch(BaseModel):
    enabled: Optional[bool] = None
    motion_enabled: Optional[bool] = None
    incident_opened_enabled: Optional[bool] = None
    motion_cooldown_min: Optional[int] = Field(None, ge=1, le=60)
    schedule_mode: Optional[str] = None  # validated below
    schedule_start: Optional[str] = None  # HH:MM
    schedule_end: Optional[str] = None  # HH:MM
    active_days: Optional[list[str]] = None
    camera_scope: Optional[dict] = None


_VALID_SCHEDULE_MODES = {"always", "scheduled", "off"}
_VALID_DAY_KEYS = {"mon", "tue", "wed", "thu", "fri", "sat", "sun"}


def _resolve_sentinel_access(db: Session, org_id: str) -> tuple[bool, dict]:
    """Single computation of both "is Sentinel access granted" and, if
    not, why — a 402 caller uses `denial_detail` directly rather than
    re-deriving it from a second, independent plan/license lookup a few
    DB round-trips later, which could otherwise disagree with the
    first check if state changed in between (e.g. the background
    license-reconcile loop committing mid-request).

    Free / past-due-too-long orgs get `plan_required`. Self-hosted
    orgs are unconditionally "self_host" (Sentinel-eligible by plan
    alone, since self-host has no billing at all) — the additional
    `sentinel_blocked_by_license` check is what actually requires a
    paid license for self-host, surfaced as `license_required` instead
    of the "upgrade to Pro" CTA that doesn't apply there. No-op for
    hosted Clerk orgs either way.

    Returns `(has_access, denial_detail)`; `denial_detail` is only
    meaningful when `has_access` is False.
    """
    plan = effective_plan_for_caps(db, org_id)
    if plan not in SENTINEL_PLANS:
        return False, {"error": "plan_required", "plan": "pro"}
    if sentinel_blocked_by_license(plan, db):
        return False, {"error": "license_required"}
    return True, {}


def _ensure_config_row(db: Session, org_id: str) -> SentinelConfig:
    """Get-or-create the per-org Sentinel config row.

    Lazy-create on first GET is fine here — the unique index on
    `org_id` means a concurrent INSERT race resolves cleanly via
    IntegrityError (we then re-query and return the row that won).
    """
    cfg = db.query(SentinelConfig).filter_by(org_id=org_id).first()
    if cfg is not None:
        return cfg
    cfg = SentinelConfig(org_id=org_id)
    db.add(cfg)
    try:
        db.commit()
        db.refresh(cfg)
    except Exception:  # IntegrityError on race — re-fetch the winner
        db.rollback()
        cfg = db.query(SentinelConfig).filter_by(org_id=org_id).first()
        if cfg is None:
            raise
    return cfg


def _validate_hhmm(value: str, field_name: str) -> None:
    """Reject anything that isn't `HH:MM` 0–23:0–59."""
    if not value or len(value) != 5 or value[2] != ":":
        raise HTTPException(400, f"{field_name} must be HH:MM")
    try:
        h = int(value[:2])
        m = int(value[3:])
    except ValueError as exc:
        raise HTTPException(400, f"{field_name} must be HH:MM") from exc
    if not (0 <= h <= 23 and 0 <= m <= 59):
        raise HTTPException(400, f"{field_name} out of range")


# ── Service-to-service auth (Sentinel agent → Command Center) ───────
# Agent posts run completions back via this header.  Defined BEFORE
# any route uses it via Depends() so module-load order works out.

@dataclass(frozen=True)
class AgentPrincipal:
    """Who an authenticated agent request is acting as.

    ``org_id is None`` means the first-party multi-tenant agent, which
    is allowed to see and act across every org. A scoped principal
    carries exactly one org and must never be able to widen that.
    Endpoints branch on ``scoped`` rather than on ``org_id is None`` so
    the intent is explicit at each call site.
    """

    org_id: Optional[str]
    scoped: bool
    key_id: Optional[int]


async def require_sentinel_agent(
    x_sentinel_agent_key: Optional[str] = Header(None, alias="X-Sentinel-Agent-Key"),
    db: Session = Depends(get_db),
) -> "AgentPrincipal":
    """Verify the inbound request carries the shared SENTINEL_AGENT_KEY
    secret.  Used only for service-to-service callbacks from the
    Sentinel agent into Command Center (run-completion + pending-run
    polling).

    Org scope is established by the run row's org_id, not by this
    auth — the agent is org-agnostic at the auth layer.  Each run
    record is org-scoped server-side, so a leaked agent key can only
    update runs that already exist (it can't fabricate a run for a
    different org).

    An unset SENTINEL_AGENT_KEY disables only the shared first-party
    path — it must NOT disable scoped per-org keys.  A self-hosted
    Command Center never sets that env var (there is no first-party
    agent to authenticate) and yet is exactly the deployment that wants
    to issue scoped keys for its own agent.  Rejecting up front here
    made the whole scoped path unreachable on those installs.
    """
    # Compare BYTES, not str: ``hmac.compare_digest(str, str)`` raises
    # TypeError when either side contains non-ASCII, and Starlette
    # decodes header values as latin-1 — so an unauthenticated probe
    # with any byte >0x7F in the header produced an unhandled 500 on
    # all three agent endpoints instead of a clean 401.  latin-1 can
    # encode every such header value back losslessly.
    if not x_sentinel_agent_key:
        raise HTTPException(401, "invalid agent key")

    presented = x_sentinel_agent_key.encode("latin-1", "replace")

    # 1. The first-party shared key: SourceBox's own multi-tenant agent.
    #    Org-agnostic by design — it drains every org's queue.  Guarded
    #    on the setting being non-empty so an unset key can never match
    #    an empty-ish header; constant-time compare so a timing
    #    side-channel can't reveal prefix matches against the secret.
    if settings.SENTINEL_AGENT_KEY and hmac.compare_digest(
        presented, settings.SENTINEL_AGENT_KEY.encode("utf-8")
    ):
        return AgentPrincipal(org_id=None, scoped=False, key_id=None)

    # 2. A per-org scoped key belonging to a customer-hosted agent.
    #    org_id comes FROM THE ROW — never from a header the caller
    #    controls. That is the entire point of this path: the holder of
    #    a scoped key must not be able to name an org it doesn't own.
    key_hash = hashlib.sha256(presented).hexdigest()
    row = (
        db.query(SentinelAgentKey)
        .filter(
            SentinelAgentKey.key_hash == key_hash,
            SentinelAgentKey.revoked.is_(False),
        )
        .first()
    )
    if row is None:
        # Same message and status as a bad shared key: don't tell an
        # attacker which of the two key types they got wrong.
        raise HTTPException(401, "invalid agent key")

    # Best-effort last-seen. Never let this fail the request — an agent
    # being unable to work because a bookkeeping write failed would be a
    # worse outcome than a slightly stale timestamp.
    try:
        row.last_used_at = datetime.now(tz=UTC).replace(tzinfo=None)
        db.commit()
    except Exception:
        db.rollback()
        logger.warning("sentinel: could not stamp last_used_at for agent key %s", row.id)

    return AgentPrincipal(org_id=row.org_id, scoped=True, key_id=row.id)


# ── GET /api/sentinel/config ────────────────────────────────────────
@router.get("/config")
async def get_config(
    user: AuthUser = Depends(require_view),
    db: Session = Depends(get_db),
):
    """Return the org's Sentinel config (creating defaults on first call).

    Always returns 200 — orgs without Sentinel access (free /
    past-due-too-long) get the same payload with `plan_gated: true`
    so the frontend can render a read-only view with an upgrade
    banner.  `monthly_cap` reflects the org's plan-specific cap
    (100 for Pro, 500 for Pro Plus, 0 for ineligible plans).
    """
    cfg = _ensure_config_row(db, user.org_id)
    plan = effective_plan_for_caps(db, user.org_id)
    # _resolve_sentinel_access (not a raw `plan in SENTINEL_PLANS` check)
    # so a self-hosted install without a valid license shows the same
    # gated state here that dispatch/PATCH/manual-run already enforce —
    # this endpoint used to only check plan membership, which meant an
    # unlicensed self-host org saw plan_gated: false and a full cap
    # here while every write path correctly blocked it underneath.
    has_access, denial_detail = _resolve_sentinel_access(db, user.org_id)
    return {
        "config": cfg.to_dict(),
        "plan_gated": not has_access,
        # "plan_required" or "license_required" — lets a future
        # frontend distinguish "upgrade to Pro" from "self-hosted,
        # needs a Sentinel license" instead of always showing the
        # Clerk-upgrade CTA. None when access is granted.
        "plan_gated_reason": denial_detail.get("error") if not has_access else None,
        # Minimum tier that gets ANY Sentinel access; the UI uses this
        # for the "upgrade to Pro" CTA on the locked banner.
        "plan_required": "pro",
        "plan_current": get_plan_display_name(plan),
        # Cap for the org's CURRENT plan — drives the run-budget UI.
        # 0 when the org isn't on a Sentinel-eligible plan OR (self-host)
        # doesn't have a valid license, even though the plan itself
        # ("self_host") nominally carries a non-zero cap.
        "monthly_cap": cap_for_plan(plan) if has_access else 0,
    }


# ── PATCH /api/sentinel/config ──────────────────────────────────────
@router.patch("/config")
async def patch_config(
    request: Request,
    patch: SentinelConfigPatch,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Apply a partial update to the org's Sentinel config.

    Only fields present in the request body are touched — partial
    updates are the norm (frontend toggles fire one at a time).
    Returns the full config so the frontend doesn't need a follow-up
    GET to reflect the new state.
    """
    has_access, denial_detail = _resolve_sentinel_access(db, user.org_id)
    if not has_access:
        raise HTTPException(status_code=402, detail=denial_detail)

    cfg = _ensure_config_row(db, user.org_id)
    changes: list[str] = []

    body = patch.model_dump(exclude_unset=True)
    for field, value in body.items():
        if value is None:
            continue

        # Field-level validation for the constrained values.
        if field == "schedule_mode":
            if value not in _VALID_SCHEDULE_MODES:
                raise HTTPException(400, f"invalid schedule_mode: {value!r}")
            cfg.schedule_mode = value
        elif field == "schedule_start":
            _validate_hhmm(value, "schedule_start")
            cfg.schedule_start = value
        elif field == "schedule_end":
            _validate_hhmm(value, "schedule_end")
            cfg.schedule_end = value
        elif field == "active_days":
            if not isinstance(value, list):
                raise HTTPException(400, "active_days must be a list")
            cleaned = [d for d in value if d in _VALID_DAY_KEYS]
            cfg.set_active_days(cleaned)
        elif field == "camera_scope":
            if not isinstance(value, dict):
                raise HTTPException(400, "camera_scope must be an object")
            cfg.set_camera_scope(value)
        else:
            # Boolean / int columns — set directly
            setattr(cfg, field, value)

        changes.append(f"{field}={value}")

    if changes:
        cfg.updated_at = datetime.now(tz=UTC).replace(tzinfo=None)
        db.commit()
        db.refresh(cfg)
        write_audit(
            db,
            org_id=user.org_id,
            event="sentinel_config_updated",
            user_id=user.user_id,
            username=user.email or user.username,
            details={"changes": changes},
            request=request,
        )

    return {"config": cfg.to_dict()}


# ── GET /api/sentinel/runs ──────────────────────────────────────────
@router.get("/runs")
async def list_runs(
    limit: int = Query(50, ge=1, le=200),
    offset: int = Query(0, ge=0),
    trigger: Optional[str] = Query(None, description="filter: motion|incident_opened|manual|scheduled"),
    since: Optional[str] = Query(None, description="ISO datetime — runs >= this"),
    user: AuthUser = Depends(require_view),
    db: Session = Depends(get_db),
):
    """List Sentinel runs for the user's org with offset+limit pagination
    and small inline stats (runs_today, total).

    No SSE for live updates yet — slice 4 will add a stream endpoint
    when the agent service starts producing rows.
    """
    base = db.query(SentinelRun).filter_by(org_id=user.org_id)

    q = base
    if trigger:
        q = q.filter_by(trigger_type=trigger)
    if since:
        try:
            since_dt = datetime.fromisoformat(since.replace("Z", "+00:00"))
            # If the client sent a tz-aware timestamp, convert to UTC
            # FIRST and then strip tz to match the naive UTC datetimes
            # stored in the column.  Previously the tz was just dropped,
            # so e.g. '2026-05-07T15:00-05:00' was queried as 15:00 UTC
            # instead of 20:00 UTC — off by the offset.
            if since_dt.tzinfo is not None:
                since_dt = since_dt.astimezone(UTC).replace(tzinfo=None)
            q = q.filter(SentinelRun.triggered_at >= since_dt)
        except ValueError as exc:
            raise HTTPException(400, "invalid `since` — expected ISO datetime") from exc

    total = q.count()
    rows = (
        q.order_by(SentinelRun.triggered_at.desc())
        .offset(offset)
        .limit(limit)
        .all()
    )

    # "Today" = midnight in the org's configured timezone, converted
    # back to UTC for comparison against the naive UTC `triggered_at`
    # column.  Previously used UTC midnight regardless of the org's
    # tz, which made "runs today" show the wrong window for any
    # non-UTC org (an EU user at 06:00 local would miss the six
    # hours of runs that landed between 23:00 UTC and 05:00 UTC
    # before the UTC day rolled).
    from zoneinfo import ZoneInfo, ZoneInfoNotFoundError
    tz_name = Setting.get(db, user.org_id, "timezone", "UTC") or "UTC"
    try:
        org_tz = ZoneInfo(tz_name)
    except (ZoneInfoNotFoundError, ValueError):
        org_tz = ZoneInfo("UTC")
    now_local = datetime.now(tz=org_tz)
    today_start = (
        now_local.replace(hour=0, minute=0, second=0, microsecond=0)
        .astimezone(UTC)
        .replace(tzinfo=None)
    )
    runs_today = (
        base.filter(SentinelRun.triggered_at >= today_start).count()
    )

    incident_count = base.filter(SentinelRun.outcome == "incident").count()
    pending_count = base.filter(SentinelRun.outcome.in_(("pending", "running"))).count()
    runs_month = runs_used_this_month(db, user.org_id)
    cap = cap_for_plan(effective_plan_for_caps(db, user.org_id))

    return {
        "runs": [r.to_dict(include_trace=False) for r in rows],
        "total": total,
        "stats": {
            "runs_today": runs_today,
            "runs_total": base.count(),
            "runs_this_month": runs_month,
            "incidents_filed": incident_count,
            "pending": pending_count,
            # Plan-aware monthly cap.  Pro = 100, Pro Plus = 500,
            # ineligible = 0 (read-only UI).  Frontend reads this
            # directly rather than hardcoding the value.
            "monthly_cap": cap,
            "remaining_this_month": max(0, cap - runs_month),
        },
    }


# ── GET /api/sentinel/runs/pending (agent → CC) ─────────────────────
# REGISTERED BEFORE /runs/{run_id} so the literal "pending" path
# wins over the parameterised one (FastAPI matches in registration
# order; otherwise GET /runs/pending would 404 with run_id=pending).
@router.get("/runs/pending")
async def list_pending_runs(
    limit: int = Query(20, ge=1, le=100),
    db: Session = Depends(get_db),
    agent: AgentPrincipal = Depends(require_sentinel_agent),
):
    """Polling endpoint for the Sentinel agent to discover work.

    Returns up to `limit` pending runs across all orgs, oldest-first
    (FIFO).  The agent is responsible for calling /start on each one
    it picks up so others don't race for the same row.

    Slice 3 may swap this for a webhook delivery model — both flows
    are agent-side concerns; the run record contract stays the same.
    """
    q = db.query(SentinelRun).filter(SentinelRun.outcome == "pending")

    # A scoped key sees ONLY its own org's queue. Without this filter a
    # customer running their own agent would receive other customers'
    # pending runs — including their org ids and incident context — and
    # the leak would be silent, because the agent would simply process
    # what it was handed.
    if agent.scoped:
        q = q.filter(SentinelRun.org_id == agent.org_id)

    rows = q.order_by(SentinelRun.triggered_at.asc()).limit(limit).all()
    return {
        "runs": [
            {
                **r.to_dict(include_trace=False),
                # Agent needs the org_id to know which MCP key to use
                # — surfaced explicitly because to_dict() doesn't
                # include it (UI doesn't need it).
                "org_id": r.org_id,
            }
            for r in rows
        ],
    }


# ── GET /api/sentinel/runs/{run_id} ─────────────────────────────────
@router.get("/runs/{run_id}")
async def get_run(
    run_id: str,
    user: AuthUser = Depends(require_view),
    db: Session = Depends(get_db),
):
    """Return a single run with full tool trace (for the drawer)."""
    row = (
        db.query(SentinelRun)
        .filter_by(org_id=user.org_id, id=run_id)
        .first()
    )
    if row is None:
        raise HTTPException(404, "run not found")
    return row.to_dict(include_trace=True)


# ── POST /api/sentinel/runs/manual ──────────────────────────────────
class ManualRunBody(BaseModel):
    prompt: str = Field("", max_length=2000)
    camera_id: Optional[str] = None


@router.post("/runs/manual")
async def post_manual_run(
    body: ManualRunBody,
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Operator-initiated agent run.  Creates a pending sentinel_runs
    row that the agent picks up.

    Pro or Pro Plus.  Per-plan cap-enforced.  Schedule + scope are
    deliberately NOT enforced — the operator clicked "Run now" to
    override them.
    """
    has_access, denial_detail = _resolve_sentinel_access(db, user.org_id)
    if not has_access:
        raise HTTPException(status_code=402, detail=denial_detail)

    try:
        run = dispatch_manual_run(
            db,
            org_id=user.org_id,
            prompt=body.prompt,
            camera_id=body.camera_id,
        )
    except ValueError as exc:
        if str(exc) == "monthly_cap_reached":
            cap = cap_for_plan(effective_plan_for_caps(db, user.org_id))
            raise HTTPException(
                status_code=429,
                detail={
                    "error": "monthly_cap_reached",
                    "cap": cap,
                    "used": runs_used_this_month(db, user.org_id),
                },
            ) from exc
        if str(exc) == "plan_not_eligible":
            raise HTTPException(
                status_code=402,
                detail={"error": "plan_required", "plan": "pro"},
            ) from exc
        if str(exc) == "license_required":
            # Defense-in-depth only in practice: the upfront
            # _resolve_sentinel_access check above already catches this
            # for the one real caller of dispatch_manual_run. Mapped
            # anyway so it can't fall through to an unhandled 500 if
            # that ever changes.
            raise HTTPException(
                status_code=402,
                detail={"error": "license_required"},
            ) from exc
        if str(exc) == "dispatch_globally_disabled":
            # Operator paused the agent fleet-wide (kill-switch or the
            # global monthly ceiling). Distinct from a per-org cap.
            raise HTTPException(
                status_code=503,
                detail={"error": "sentinel_dispatch_disabled"},
            ) from exc
        raise

    write_audit(
        db,
        org_id=user.org_id,
        event="sentinel_manual_run",
        user_id=user.user_id,
        username=user.email or user.username,
        details={
            "run_id": run.id,
            "camera_id": body.camera_id or None,
            "prompt_len": len(body.prompt or ""),
        },
        request=request,
    )
    return run.to_dict(include_trace=False)


# ── POST /api/sentinel/runs/{id}/complete (agent → CC) ──────────────
class RunCompleteBody(BaseModel):
    outcome: str  # incident | no_action | error
    severity: Optional[str] = None  # low | medium | high | critical (only when outcome=incident)
    incident_id: Optional[int] = None
    summary: str = Field("", max_length=8000)
    tool_call_count: int = 0
    tool_trace: Optional[list[dict]] = None


_VALID_TERMINAL_OUTCOMES = {"incident", "no_action", "error"}


@router.post("/runs/{run_id}/complete")
async def post_run_complete(
    run_id: str,
    body: RunCompleteBody,
    db: Session = Depends(get_db),
    agent: AgentPrincipal = Depends(require_sentinel_agent),
):
    """Agent → Command Center callback to mark a pending/running run
    as completed.

    Idempotency rules:

      - Same outcome retried (incident → incident, etc.): no-op,
        return existing row.  Lets the agent safely re-POST a
        completion if the original ack was lost.
      - error → incident / no_action: ALLOWED.  The wall-clock
        timeout cleanup wrapper in process_with_timeout proactively
        marks an in-flight run as `error` when the 270 s budget is
        hit; if the agent later finishes successfully (e.g. a future
        retry path, or the upcoming CC-side stranded-run reaper),
        we want the real outcome to land instead of being trapped
        behind a defensive-error stamp.
      - incident / no_action → error: refused (treated as no-op).
        Once the agent has reported a real outcome we don't let it
        get downgraded.
    """
    if body.outcome not in _VALID_TERMINAL_OUTCOMES:
        raise HTTPException(400, f"invalid outcome: {body.outcome!r}")
    # "critical" included: the MCP create_incident enum (and the agent's
    # own prompt) allow it — rejecting it here 400'd exactly the
    # highest-urgency completions, downgrading those runs to error.
    if body.outcome == "incident" and body.severity not in (
        "low", "medium", "high", "critical",
    ):
        raise HTTPException(400, "severity required for outcome=incident")

    row = db.query(SentinelRun).filter_by(id=run_id).first()
    if row is None:
        raise HTTPException(404, "run not found")
    # A scoped key may only touch its own org's runs. Without this a
    # customer-hosted agent could start or complete another customer's
    # run — and /complete writes an incident, so that is a write into
    # someone else's data, not just a read. 404 rather than 403 so a
    # scoped caller cannot probe which run ids exist.
    if agent.scoped and row.org_id != agent.org_id:
        raise HTTPException(404, "run not found")

    if row.is_terminal:
        # Allow a one-way upgrade error → real outcome; otherwise
        # short-circuit as a same-outcome retry no-op.
        is_error_to_real_upgrade = (
            row.outcome == "error" and body.outcome in ("incident", "no_action")
        )
        if not is_error_to_real_upgrade:
            return row.to_dict(include_trace=True)

    # Cross-check that the agent isn't pointing the run at an incident
    # outside the run's org.  The agent is trusted infrastructure (single
    # shared key) but a leaked key would let the holder write
    # `incident_id=<some other org's id>` into a run row, which would
    # surface as a wrong/foreign deep-link in that org's run drawer.
    # Defence-in-depth: only accept incident IDs that belong to row.org_id.
    if body.outcome == "incident" and body.incident_id is not None:
        owned = (
            db.query(Incident.id)
            .filter_by(id=body.incident_id, org_id=row.org_id)
            .first()
        )
        if owned is None:
            raise HTTPException(
                400,
                "incident_id does not belong to this run's org",
            )

    now = datetime.now(tz=UTC).replace(tzinfo=None)
    row.outcome = body.outcome
    row.severity = body.severity if body.outcome == "incident" else None
    row.incident_id = body.incident_id if body.outcome == "incident" else None
    row.summary = (body.summary or "")[:8000]
    row.tool_call_count = max(0, int(body.tool_call_count or 0))
    if body.tool_trace is not None:
        row.set_tool_trace(body.tool_trace)
    if row.started_at is None:
        # Agent went straight to terminal without an explicit start
        # signal — best-effort backfill.
        row.started_at = now
    row.completed_at = now

    db.commit()
    db.refresh(row)
    logger.info(
        "sentinel: run completed id=%s org=%s outcome=%s severity=%s",
        row.id, row.org_id, row.outcome, row.severity,
    )
    return row.to_dict(include_trace=True)


# ── POST /api/sentinel/runs/{id}/start (agent → CC) ─────────────────
@router.post("/runs/{run_id}/start")
async def post_run_start(
    run_id: str,
    db: Session = Depends(get_db),
    agent: AgentPrincipal = Depends(require_sentinel_agent),
):
    """Agent claims a pending run and transitions it to running.
    Optional — the agent may skip this and jump straight to /complete
    if it doesn't need a separate "I'm working on it" signal.
    """
    row = db.query(SentinelRun).filter_by(id=run_id).first()
    if row is None:
        raise HTTPException(404, "run not found")
    # A scoped key may only touch its own org's runs. Without this a
    # customer-hosted agent could start or complete another customer's
    # run — and /complete writes an incident, so that is a write into
    # someone else's data, not just a read. 404 rather than 403 so a
    # scoped caller cannot probe which run ids exist.
    if agent.scoped and row.org_id != agent.org_id:
        raise HTTPException(404, "run not found")
    if row.outcome != "pending":
        # Already past pending — accept idempotently, but tell the
        # caller it did NOT win the claim.  Without this flag, two
        # overlapping wakeup drains both got an indistinguishable 200
        # and both ran the full (expensive) agent loop → duplicate
        # incidents + double LLM spend.
        result = row.to_dict(include_trace=False)
        result["claimed"] = False
        return result
    row.outcome = "running"
    row.started_at = datetime.now(tz=UTC).replace(tzinfo=None)
    db.commit()
    db.refresh(row)
    result = row.to_dict(include_trace=False)
    result["claimed"] = True
    return result


# /runs/pending lives above (registered BEFORE /runs/{run_id} due to
# FastAPI's in-order route matching).


# ── Agent key management (issuance) ─────────────────────────────────
#
# Mints the per-org credential a customer needs to run the Sentinel
# agent on their own hardware.  The shared SENTINEL_AGENT_KEY cannot be
# handed out: it drains every org's queue and can act as any org via
# X-Agent-Org-Override.  A key minted here is bound to one org by its
# database row, and both auth paths derive org_id from that row.

AGENT_KEY_PREFIX = "osa_"


def _generate_agent_key() -> str:
    """``osa_`` + 32 hex chars, matching osc_ (MCP) and osi_ (integration).

    **ASCII is load-bearing, not incidental.** The two auth paths hash
    different byte encodings of the same string: this module hashes
    ``latin-1`` bytes (see require_sentinel_agent — Starlette decodes
    headers as latin-1), while app/mcp/server.py hashes UTF-8. Those
    agree only while the key is ASCII. token_hex is [0-9a-f], so it is
    safe; swapping in a "friendlier" alphabet with any non-ASCII
    character would mint keys that authenticate on one path and 401 on
    the other, which is a miserable bug to diagnose.
    """
    return AGENT_KEY_PREFIX + secrets.token_hex(16)


class AgentKeyCreateBody(BaseModel):
    name: str = Field("Self-hosted agent", max_length=100)


@router.post("/agent-keys")
@limiter.limit("10/hour")
async def create_agent_key(
    request: Request,
    body: AgentKeyCreateBody,
    user: AuthUser = Depends(require_active_billing),
    db: Session = Depends(get_db),
):
    """Mint a per-org agent key.  Returns the plaintext exactly once.

    ``require_active_billing`` rather than ``require_admin``: this
    provisions a credential that spends money (every run the agent
    completes burns a monthly-cap slot and real LLM cost), which is
    precisely the "past-due orgs can read but not provision" case that
    dependency exists for.  Revocation deliberately stays on plain
    ``require_admin`` — see below.
    """
    # Plan/licence gate, same as patch_config and post_manual_run.  This
    # is a UX gate, not the security boundary: app/mcp/server.py
    # re-checks plan and licence on every tool call, because a plan can
    # change long after a key is minted.  Failing here means a free org
    # finds out now, with an upgrade CTA, instead of at 3am via an
    # opaque 401 from an agent they already configured.
    has_access, denial_detail = _resolve_sentinel_access(db, user.org_id)
    if not has_access:
        raise HTTPException(status_code=402, detail=denial_detail)

    # key_hash is UNIQUE. A 128-bit collision is not going to happen,
    # but an unhandled IntegrityError here would be a 500 with a dirty
    # session, so absorb it and try once more rather than leaving the
    # only failure path in this endpoint uncovered.
    for attempt in (1, 2):
        raw_key = _generate_agent_key()
        row = SentinelAgentKey(
            org_id=user.org_id,
            key_hash=hashlib.sha256(raw_key.encode()).hexdigest(),
            key_last4=raw_key[-4:],
            name=body.name,
            # The user id, NOT audit_label(user): this column is
            # String(100) and an email can overflow it. The human-
            # readable actor is durable in the audit row instead.
            created_by=user.user_id,
        )
        db.add(row)
        try:
            db.commit()
            break
        except IntegrityError:
            db.rollback()
            if attempt == 2:
                raise
    db.refresh(row)

    write_audit(
        db,
        org_id=user.org_id,
        event="sentinel_agent_key_created",
        user_id=user.user_id,
        username=audit_label(user),
        details={"key_id": row.id, "name": body.name, "key_last4": row.key_last4},
        request=request,
    )

    # Security-audit signal to admins.  Names the actor so a recipient
    # who IS the actor recognises their own action rather than
    # suspecting a compromise.
    try:
        from app.api.notifications import create_notification
        actor = audit_label(user) or user.user_id or "unknown user"
        create_notification(
            org_id=user.org_id,
            kind="sentinel_agent_key_created",
            title=f"New Sentinel agent key created: {body.name}",
            body=(
                f"{actor} just created a Sentinel agent key "
                f"\"{body.name}\".  Anyone holding it can run the Sentinel "
                f"agent against this organization's cameras.  If this was "
                f"you, no action needed.  If not, revoke it from the MCP "
                f"settings page immediately."
            ),
            severity="warning",
            audience="admin",
            link="/mcp",
            meta={
                "key_id": row.id,
                "key_name": body.name,
                "actor_user_id": user.user_id,
            },
            db=db,
        )
    except Exception:
        # The audit row is already committed; losing the inbox notice is
        # annoying, not a security regression.  Never fail the mint.
        logger.exception(
            "[SentinelAgentKeys] notification emit failed for key_id=%s", row.id,
        )

    return {
        "id": row.id,
        "name": row.name,
        # Only time this value exists outside the caller's machine.
        "key": raw_key,
        "key_last4": row.key_last4,
        "created_at": row.created_at.isoformat() if row.created_at else None,
        "warning": "Save this key now. You won't be able to see it again.",
    }


@router.get("/agent-keys")
async def list_agent_keys(
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """List this org's live agent keys.

    Deliberately NOT plan-gated, unlike minting: an org that downgrades
    must still be able to see and revoke credentials it already issued.
    Gating this would strand live keys with no UI to kill them.
    """
    rows = (
        db.query(SentinelAgentKey)
        .filter_by(org_id=user.org_id, revoked=False)
        .order_by(SentinelAgentKey.created_at.desc())
        .all()
    )
    return [r.to_dict() for r in rows]


@router.delete("/agent-keys/{key_id}")
@limiter.limit("30/hour")
async def revoke_agent_key(
    key_id: int,
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Revoke an agent key.  Takes effect immediately.

    ``org_id`` in the filter below is the security control, not a
    convenience — without it any admin could revoke any org's key.  404
    rather than 403 on a miss so a caller cannot probe which key ids
    exist elsewhere.

    Soft revoke, matching McpApiKey: keeps ``last_used_at`` as the
    forensic answer to "when did this leaked credential last act?", and
    keeps the unique ``key_hash`` permanently burned.
    """
    row = (
        db.query(SentinelAgentKey)
        .filter_by(id=key_id, org_id=user.org_id)
        .first()
    )
    if row is None:
        raise HTTPException(404, "agent key not found")

    row.revoked = True
    db.commit()

    write_audit(
        db,
        org_id=user.org_id,
        event="sentinel_agent_key_revoked",
        user_id=user.user_id,
        username=audit_label(user),
        details={"key_id": row.id, "name": row.name},
        request=request,
    )

    try:
        from app.api.notifications import create_notification
        actor = audit_label(user) or user.user_id or "unknown user"
        create_notification(
            org_id=user.org_id,
            kind="sentinel_agent_key_revoked",
            title=f"Sentinel agent key revoked: {row.name}",
            body=(
                f"{actor} revoked the Sentinel agent key \"{row.name}\".  "
                f"Any agent still using it will start failing immediately."
            ),
            severity="info",
            audience="admin",
            link="/admin/audit-log",
            meta={"key_id": row.id, "key_name": row.name, "actor_user_id": user.user_id},
            db=db,
        )
    except Exception:
        logger.exception(
            "[SentinelAgentKeys] revoke notification failed for key_id=%s", row.id,
        )

    return {"success": True, "revoked": key_id}
