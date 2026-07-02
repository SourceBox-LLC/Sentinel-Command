"""
Motion Events API — query motion detection events reported by CameraNodes,
plus SSE streaming for real-time motion alerts in the dashboard.
"""

import asyncio
import json
import logging
from datetime import UTC, datetime, timedelta
from typing import Optional

from fastapi import APIRouter, Depends, HTTPException, Query, Request
from fastapi.responses import StreamingResponse
from sqlalchemy.orm import Session

from app.core.auth import AuthUser, require_view
from app.core.database import get_db
from app.core.limiter import limiter
from app.models.models import MotionEvent

logger = logging.getLogger(__name__)

router = APIRouter(prefix="/api/motion", tags=["motion"])


# ── Motion Event Broadcaster ────────────────────────────────────────
# Lightweight pub/sub that forwards motion events to SSE subscribers.
# Called by ws.py after persisting each motion event to the DB.

# Per-org SSE subscriber cap. Tiered — the route handler looks up the
# caller's plan and passes the per-tier cap (defined in
# ``app.core.plans.PLAN_LIMITS[plan]["max_sse_subscribers"]``) into
# ``subscribe``. This fallback only matters for code paths that haven't
# yet been wired to pass the cap through (e.g. future admin tooling); the
# public SSE routes always pass the tier cap. A single authenticated
# member otherwise could open thousands of long-lived streams from a
# scripted loop and exhaust server memory.
MAX_SSE_SUBSCRIBERS_PER_ORG = 100  # fallback — Pro Plus default


class MotionBroadcaster:
    """Push motion events to dashboard SSE connections, scoped by org."""

    def __init__(self):
        # {org_id: [asyncio.Queue, ...]}
        self._subscribers: dict[str, list[asyncio.Queue]] = {}

    def notify(self, org_id: str, event_data: dict):
        """Broadcast a motion event to all SSE subscribers for an org."""
        queues = self._subscribers.get(org_id, [])
        dead = []
        for q in queues:
            try:
                q.put_nowait(event_data)
            except asyncio.QueueFull:
                dead.append(q)
        if dead:
            for q in dead:
                try:
                    self._subscribers[org_id].remove(q)
                except (ValueError, KeyError):
                    pass

    def subscribe(self, org_id: str, cap: int = MAX_SSE_SUBSCRIBERS_PER_ORG) -> Optional[asyncio.Queue]:
        """Add a new SSE subscription for an org.

        ``cap`` is the per-tier subscriber cap the caller looked up (see
        PLAN_LIMITS). Returns the queue on success, or ``None`` when this
        org is already at the cap (the route handler turns that into a 429).
        """
        existing = self._subscribers.setdefault(org_id, [])
        if len(existing) >= cap:
            logger.warning(
                "[Motion] SSE cap hit for org %s (%d/%d) — rejecting",
                org_id, len(existing), cap,
            )
            return None
        q: asyncio.Queue = asyncio.Queue(maxsize=100)
        existing.append(q)
        logger.info("[Motion] SSE subscriber added for org %s (%d/%d)",
                    org_id, len(existing), cap)
        return q

    def unsubscribe(self, org_id: str, q: asyncio.Queue):
        if org_id in self._subscribers:
            try:
                self._subscribers[org_id].remove(q)
            except ValueError:
                pass


# Singleton — imported by ws.py to broadcast events.
motion_broadcaster = MotionBroadcaster()

# Second, independent pool for the Home Assistant integration SSE
# (app/api/integration.py). The motion publisher in ws.py fans out to BOTH,
# so integration subscribers get the same org-wide events — but a persistent
# HA connection lives in its own pool and never consumes a dashboard SSE
# subscriber slot (the dashboard cap and the integration cap are separate).
integration_motion_broadcaster = MotionBroadcaster()


@router.get("/events")
async def list_motion_events(
    camera_id: Optional[str] = None,
    hours: int = Query(default=24, ge=1, le=168),
    # ge=1: SQLite treats LIMIT -1 as "no limit".
    limit: int = Query(default=100, ge=1, le=500),
    # OFFSET is O(n) — cap so no one can force SQLite to skip billions.
    offset: int = Query(default=0, ge=0, le=1_000_000),
    user: AuthUser = Depends(require_view),
    db: Session = Depends(get_db),
):
    """List recent motion events, optionally filtered by camera."""
    since = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(hours=hours)

    query = db.query(MotionEvent).filter(
        MotionEvent.org_id == user.org_id,
        MotionEvent.timestamp >= since,
    )

    if camera_id:
        query = query.filter(MotionEvent.camera_id == camera_id)

    total = query.count()
    events = (
        query.order_by(MotionEvent.timestamp.desc())
        .offset(offset)
        .limit(limit)
        .all()
    )

    return {
        "total": total,
        "limit": limit,
        "offset": offset,
        "hours": hours,
        "events": [e.to_dict() for e in events],
    }


@router.get("/events/stats")
async def motion_stats(
    hours: int = Query(default=24, le=168),
    user: AuthUser = Depends(require_view),
    db: Session = Depends(get_db),
):
    """Aggregate motion stats per camera over the given time window."""
    from sqlalchemy import func

    since = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(hours=hours)

    rows = (
        db.query(
            MotionEvent.camera_id,
            func.count(MotionEvent.id).label("count"),
            func.max(MotionEvent.score).label("peak_score"),
            func.max(MotionEvent.timestamp).label("latest"),
        )
        .filter(
            MotionEvent.org_id == user.org_id,
            MotionEvent.timestamp >= since,
        )
        .group_by(MotionEvent.camera_id)
        .all()
    )

    return {
        "hours": hours,
        "cameras": [
            {
                "camera_id": r.camera_id,
                "event_count": r.count,
                "peak_score": r.peak_score,
                "latest": r.latest.isoformat() if r.latest else None,
            }
            for r in rows
        ],
    }


# ── SSE Stream ──────────────────────────────────────────────────────

@router.get("/events/stream")
@limiter.limit("60/minute")
async def stream_motion_events(
    request: Request,
    user: AuthUser = Depends(require_view),
):
    """
    SSE endpoint — streams motion detection events in real-time.
    Used by the dashboard to show instant motion notifications.

    Rate-limited to 60 connect attempts per minute per org.  See the
    notifications-stream endpoint for the full rationale — same
    threat model (connect-flood burning JWT-verify CPU even when the
    per-org subscriber cap rejects the new subscriber).
    """
    from app.core.plans import get_plan_limits
    org_id = user.org_id
    cap = get_plan_limits(user.plan).get("max_sse_subscribers", MAX_SSE_SUBSCRIBERS_PER_ORG)
    queue = motion_broadcaster.subscribe(org_id, cap)
    if queue is None:
        raise HTTPException(
            status_code=429,
            detail=(
                f"Too many open motion streams for this org (cap: {cap} on "
                f"your current plan). Close unused dashboard tabs and retry, "
                f"or upgrade for a higher cap."
            ),
        )

    async def event_generator():
        try:
            yield f"data: {json.dumps({'type': 'connected', 'org_id': org_id})}\n\n"

            while True:
                try:
                    event = await asyncio.wait_for(queue.get(), timeout=25.0)
                    yield f"data: {json.dumps(event)}\n\n"
                except TimeoutError:
                    # Keepalive to prevent connection drop
                    yield ": keepalive\n\n"
        except asyncio.CancelledError:
            pass
        finally:
            motion_broadcaster.unsubscribe(org_id, queue)

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
        },
    )
