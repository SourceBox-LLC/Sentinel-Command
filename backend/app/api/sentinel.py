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

import logging
from datetime import UTC, datetime
from typing import Optional

from fastapi import APIRouter, Depends, HTTPException, Query, Request
from pydantic import BaseModel, Field
from sqlalchemy.orm import Session

from app.core.audit import write_audit
from app.core.auth import AuthUser, require_admin, require_view
from app.core.database import get_db
from app.core.plans import effective_plan_for_caps, get_plan_display_name
from app.models.models import SentinelConfig, SentinelRun

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


def _is_pro_plus(db: Session, org_id: str) -> bool:
    return effective_plan_for_caps(db, org_id) == "pro_plus"


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


# ── GET /api/sentinel/config ────────────────────────────────────────
@router.get("/config")
async def get_config(
    user: AuthUser = Depends(require_view),
    db: Session = Depends(get_db),
):
    """Return the org's Sentinel config (creating defaults on first call).

    Always returns 200 — non-Pro-Plus orgs get the same payload with
    `plan_gated: true` so the frontend can render a read-only view
    with a clear upgrade banner.
    """
    cfg = _ensure_config_row(db, user.org_id)
    plan = effective_plan_for_caps(db, user.org_id)
    return {
        "config": cfg.to_dict(),
        "plan_gated": plan != "pro_plus",
        "plan_required": "pro_plus",
        "plan_current": get_plan_display_name(plan),
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
    if not _is_pro_plus(db, user.org_id):
        raise HTTPException(
            status_code=402,
            detail={"error": "plan_required", "plan": "pro_plus"},
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
            details=", ".join(changes),
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
            # Strip tz to match the naive datetimes stored in the column.
            if since_dt.tzinfo is not None:
                since_dt = since_dt.replace(tzinfo=None)
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

    # Small inline stats — avoids a separate /usage endpoint while
    # the cap framework isn't built yet (slice 5).
    today_start = datetime.now(tz=UTC).replace(
        hour=0, minute=0, second=0, microsecond=0, tzinfo=None
    )
    runs_today = (
        base.filter(SentinelRun.triggered_at >= today_start).count()
    )

    incident_count = base.filter(SentinelRun.outcome == "incident").count()

    return {
        "runs": [r.to_dict(include_trace=False) for r in rows],
        "total": total,
        "stats": {
            "runs_today": runs_today,
            "runs_total": base.count(),
            "incidents_filed": incident_count,
        },
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
