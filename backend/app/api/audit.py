from datetime import UTC, datetime, timedelta
from typing import Optional

from fastapi import APIRouter, Depends, HTTPException, Query, Request
from sqlalchemy.orm import Session

from app.core.auth import AuthUser, require_admin
from app.core.csv_export import filename_for, stream_csv_response
from app.core.database import get_db
from app.core.limiter import limiter
from app.models import StreamAccessLog

router = APIRouter(prefix="/api", tags=["audit"])


def _require_admin_feature(user: AuthUser):
    """Raise 403 if the org's plan doesn't include the admin feature."""
    if "admin" not in user.features:
        raise HTTPException(
            status_code=403,
            detail="Audit dashboard requires a Pro or Pro Plus plan. Upgrade at /pricing.",
        )


@router.get("/audit/stream-logs")
@limiter.limit("120/minute")
async def get_stream_logs(
    request: Request,
    camera_id: Optional[str] = None,
    user_id: Optional[str] = None,
    # ge=1: SQLite treats LIMIT -1 as "no limit" — without the lower
    # bound, ?limit=-1 materializes the org's whole retention window.
    limit: int = Query(default=100, ge=1, le=500),
    # Cap so an attacker can't force SQLite to skip billions of rows per
    # request (OFFSET is O(n) even with an index). 1M is well past any
    # realistic history a UI would page through.
    offset: int = Query(default=0, ge=0, le=1_000_000),
    # ``format=csv`` switches to a streaming CSV download.  Same auth,
    # same per-org rate-limit bucket; the JSON ``limit``/``offset``
    # caps are bypassed so an export can pull a meaningful window —
    # see the CSV branch below.
    format: str = Query(default="json", pattern="^(json|csv)$"),
    admin: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """
    Get stream access logs for the admin's organization.
    Only org admins can access this endpoint.
    Logs are automatically cleaned up after the retention period.

    Returns JSON by default; pass ``?format=csv`` for a streaming
    download suitable for compliance archiving.

    Rate-limited to 120 req/min per org — same as ``/api/audit-logs``.
    Each call is a multi-clause SQL query against StreamAccessLog;
    the dashboard polls only on user interaction so 2 req/sec is
    plenty of headroom.
    """
    _require_admin_feature(admin)
    query = db.query(StreamAccessLog).filter(StreamAccessLog.org_id == admin.org_id)

    if camera_id:
        query = query.filter(StreamAccessLog.camera_id == camera_id)

    if user_id:
        query = query.filter(
            StreamAccessLog.user_email.ilike(f"%{user_id}%")
            | StreamAccessLog.user_id.ilike(f"%{user_id}%")
        )

    if format == "csv":
        # Lift the row cap for CSV — auditor wants a window, not a page.
        # 50k rows × ~250 bytes/row ≈ 12 MB; constant memory via yield_per.
        csv_query = (
            query.order_by(StreamAccessLog.accessed_at.desc()).limit(50_000)
        )

        def _rows():
            for log in csv_query.yield_per(500):
                yield [
                    log.accessed_at.isoformat() if log.accessed_at else "",
                    log.camera_id or "",
                    log.node_id or "",
                    log.user_email or "",
                    log.user_id or "",
                    log.ip_address or "",
                ]

        return stream_csv_response(
            filename=filename_for("stream-access-log", admin.org_id),
            header=["accessed_at", "camera_id", "node_id", "user_email", "user_id", "ip_address"],
            rows=_rows(),
        )

    total = query.count()

    logs = (
        query.order_by(StreamAccessLog.accessed_at.desc())
        .offset(offset)
        .limit(limit)
        .all()
    )

    return {
        "total": total,
        "limit": limit,
        "offset": offset,
        "logs": [log.to_dict() for log in logs],
    }


@router.get("/audit/stream-logs/stats")
@limiter.limit("60/minute")
async def get_stream_stats(
    request: Request,
    days: int = Query(default=7, le=30),
    admin: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """
    Get stream access statistics for the admin's organization.
    Returns counts by camera, user, and day.

    Rate-limited to 60 req/min per org — this fans out into 4
    aggregating queries (count + 3 group-bys) and is more expensive
    per call than the row-list endpoint above.  Halve the budget
    accordingly.
    """
    _require_admin_feature(admin)
    from sqlalchemy import func

    since = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(days=days)

    base_query = db.query(StreamAccessLog).filter(
        StreamAccessLog.org_id == admin.org_id,
        StreamAccessLog.accessed_at >= since,
    )

    by_camera = (
        base_query.with_entities(
            StreamAccessLog.camera_id, func.count(StreamAccessLog.id).label("count")
        )
        .group_by(StreamAccessLog.camera_id)
        .order_by(func.count(StreamAccessLog.id).desc())
        .limit(10)
        .all()
    )

    by_user = (
        base_query.with_entities(
            StreamAccessLog.user_id,
            StreamAccessLog.user_email,
            func.count(StreamAccessLog.id).label("count"),
        )
        .group_by(StreamAccessLog.user_id, StreamAccessLog.user_email)
        .order_by(func.count(StreamAccessLog.id).desc())
        .limit(10)
        .all()
    )

    by_day = (
        base_query.with_entities(
            func.date(StreamAccessLog.accessed_at).label("date"),
            func.count(StreamAccessLog.id).label("count"),
        )
        .group_by(func.date(StreamAccessLog.accessed_at))
        .order_by(func.date(StreamAccessLog.accessed_at).desc())
        .all()
    )

    return {
        "days": days,
        "total_accesses": base_query.count(),
        "by_camera": [{"camera_id": c, "count": n} for c, n in by_camera],
        "by_user": [{"user_id": u, "user_email": e or "", "count": n} for u, e, n in by_user],
        "by_day": [{"date": str(d), "count": n} for d, n in by_day],
    }
