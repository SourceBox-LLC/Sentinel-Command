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

import hmac
import logging
from datetime import UTC, datetime
from typing import Optional

from fastapi import APIRouter, Depends, Header, HTTPException, Query, Request
from pydantic import BaseModel, Field
from sqlalchemy.orm import Session

from app.core.audit import write_audit
from app.core.auth import AuthUser, require_admin, require_view
from app.core.config import settings
from app.core.database import get_db
from app.core.plans import effective_plan_for_caps, get_plan_display_name
from app.core.sentinel_dispatch import (
    SENTINEL_PLANS,
    cap_for_plan,
    dispatch_manual_run,
    runs_used_this_month,
)
from app.models.models import Incident, SentinelConfig, SentinelRun, Setting

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


def _has_sentinel_access(db: Session, org_id: str) -> bool:
    """True iff the org's effective plan is in the Sentinel-eligible
    set (Pro or Pro Plus today).  Free / past-due-too-long orgs
    return False and are gated out of write endpoints / dispatch /
    the agent MCP path."""
    return effective_plan_for_caps(db, org_id) in SENTINEL_PLANS


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
async def require_sentinel_agent(
    x_sentinel_agent_key: Optional[str] = Header(None, alias="X-Sentinel-Agent-Key"),
) -> None:
    """Verify the inbound request carries the shared SENTINEL_AGENT_KEY
    secret.  Used only for service-to-service callbacks from the
    Sentinel agent into Command Center (run-completion + pending-run
    polling).

    Org scope is established by the run row's org_id, not by this
    auth — the agent is org-agnostic at the auth layer.  Each run
    record is org-scoped server-side, so a leaked agent key can only
    update runs that already exist (it can't fabricate a run for a
    different org).

    Hard-rejects every request when the key isn't configured (empty
    string), which is the desired behaviour in environments where
    the agent isn't deployed.
    """
    if not settings.SENTINEL_AGENT_KEY:
        raise HTTPException(401, "agent auth not configured")
    # Constant-time compare so a timing side-channel can't reveal
    # prefix matches against the configured secret.  Empty header
    # short-circuits before the compare.
    #
    # Compare BYTES, not str: ``hmac.compare_digest(str, str)`` raises
    # TypeError when either side contains non-ASCII, and Starlette
    # decodes header values as latin-1 — so an unauthenticated probe
    # with any byte >0x7F in the header produced an unhandled 500 on
    # all three agent endpoints instead of a clean 401.  latin-1 can
    # encode every such header value back losslessly.
    if not x_sentinel_agent_key or not hmac.compare_digest(
        x_sentinel_agent_key.encode("latin-1", "replace"),
        settings.SENTINEL_AGENT_KEY.encode("utf-8"),
    ):
        raise HTTPException(401, "invalid agent key")


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
    has_access = plan in SENTINEL_PLANS
    return {
        "config": cfg.to_dict(),
        "plan_gated": not has_access,
        # Minimum tier that gets ANY Sentinel access; the UI uses this
        # for the "upgrade to Pro" CTA on the locked banner.
        "plan_required": "pro",
        "plan_current": get_plan_display_name(plan),
        # Cap for the org's CURRENT plan — drives the run-budget UI.
        # 0 when the org isn't on a Sentinel-eligible plan.
        "monthly_cap": cap_for_plan(plan),
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
    if not _has_sentinel_access(db, user.org_id):
        raise HTTPException(
            status_code=402,
            detail={"error": "plan_required", "plan": "pro"},
        )

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
@router.get("/runs/pending", dependencies=[Depends(require_sentinel_agent)])
async def list_pending_runs(
    limit: int = Query(20, ge=1, le=100),
    db: Session = Depends(get_db),
):
    """Polling endpoint for the Sentinel agent to discover work.

    Returns up to `limit` pending runs across all orgs, oldest-first
    (FIFO).  The agent is responsible for calling /start on each one
    it picks up so others don't race for the same row.

    Slice 3 may swap this for a webhook delivery model — both flows
    are agent-side concerns; the run record contract stays the same.
    """
    rows = (
        db.query(SentinelRun)
        .filter(SentinelRun.outcome == "pending")
        .order_by(SentinelRun.triggered_at.asc())
        .limit(limit)
        .all()
    )
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
    if not _has_sentinel_access(db, user.org_id):
        raise HTTPException(
            status_code=402,
            detail={"error": "plan_required", "plan": "pro"},
        )

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
    severity: Optional[str] = None  # low | medium | high (only when outcome=incident)
    incident_id: Optional[int] = None
    summary: str = Field("", max_length=8000)
    tool_call_count: int = 0
    tool_trace: Optional[list[dict]] = None


_VALID_TERMINAL_OUTCOMES = {"incident", "no_action", "error"}


@router.post("/runs/{run_id}/complete", dependencies=[Depends(require_sentinel_agent)])
async def post_run_complete(
    run_id: str,
    body: RunCompleteBody,
    db: Session = Depends(get_db),
):
    """Agent → Command Center callback to mark a pending/running run
    as completed.

    Idempotency rules:

      - Same outcome retried (incident → incident, etc.): no-op,
        return existing row.  Lets the agent safely re-POST a
        completion if the original ack was lost.
      - error → incident / no_action: ALLOWED.  The wall-clock
        timeout cleanup wrapper in process_with_timeout proactively
        marks an in-flight run as `error` when the 540 s budget is
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
@router.post("/runs/{run_id}/start", dependencies=[Depends(require_sentinel_agent)])
async def post_run_start(
    run_id: str,
    db: Session = Depends(get_db),
):
    """Agent claims a pending run and transitions it to running.
    Optional — the agent may skip this and jump straight to /complete
    if it doesn't need a separate "I'm working on it" signal.
    """
    row = db.query(SentinelRun).filter_by(id=run_id).first()
    if row is None:
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
