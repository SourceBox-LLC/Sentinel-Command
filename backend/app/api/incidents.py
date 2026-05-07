"""
Incident reports.

Two write paths into the same `incidents` table:
  - MCP `create_incident` / `add_observation` / ... tools (agents).  See
    `app/mcp/server.py` — agent rows land with ``created_by="mcp:<key_name>"``.
  - This module's `POST /api/incidents` (humans).  Operator-filed rows
    land with ``created_by="user:<clerk_user_id>"`` so the dashboard can
    badge AI vs human at a glance.

Reads are dashboard-only (admin) and live under `/api/incidents`.
"""

import logging
from datetime import UTC, datetime
from typing import Optional

from fastapi import APIRouter, Depends, HTTPException, Query, Request, Response
from pydantic import BaseModel, Field
from sqlalchemy.orm import Session

from app.core.auth import AuthUser, require_admin
from app.core.database import get_db
from app.core.limiter import limiter
from app.models.models import (
    INCIDENT_SEVERITIES,
    INCIDENT_STATUSES,
    Camera,
    Incident,
    IncidentEvidence,
)

logger = logging.getLogger(__name__)
router = APIRouter(prefix="/api/incidents", tags=["incidents"])


# ---------------------------------------------------------------------------
# Request/response schemas
# ---------------------------------------------------------------------------


class IncidentPatch(BaseModel):
    status: Optional[str] = Field(default=None)
    severity: Optional[str] = Field(default=None)
    summary: Optional[str] = Field(default=None)
    report: Optional[str] = Field(default=None)


class IncidentCreate(BaseModel):
    """Operator-filed incident.  Mirrors the MCP `create_incident`
    tool's input shape so the same DB row layout serves both authors."""

    title: str = Field(..., min_length=1, max_length=200)
    summary: str = Field(..., min_length=1)
    severity: str = Field(default="medium")
    camera_id: Optional[str] = Field(default=None)


# ---------------------------------------------------------------------------
# Create (human-authored)
# ---------------------------------------------------------------------------


@router.post("", status_code=201)
@limiter.limit("60/minute")
async def create_incident(
    request: Request,
    body: IncidentCreate,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """File a new incident manually.

    Mirrors the MCP `create_incident` tool's validation (title and
    summary required, severity in the allowed enum, camera_id must
    exist within the org if provided) and fires the same
    `incident_created` notification so the inbox + email channels
    don't care which author wrote the row.

    The only difference from the MCP path: ``created_by`` is stamped
    ``user:<clerk_id>`` instead of ``mcp:<key_name>``.  The dashboard
    keys off this prefix to badge AI- vs human-authored rows.
    """
    if body.severity not in INCIDENT_SEVERITIES:
        raise HTTPException(
            status_code=400, detail=f"Invalid severity: {body.severity}"
        )

    title = body.title.strip()
    summary = body.summary.strip()
    if not title:
        raise HTTPException(status_code=400, detail="title is required")
    if not summary:
        raise HTTPException(status_code=400, detail="summary is required")

    if body.camera_id:
        cam = (
            db.query(Camera)
            .filter_by(org_id=user.org_id, camera_id=body.camera_id)
            .first()
        )
        if not cam:
            raise HTTPException(
                status_code=400,
                detail=f"Camera '{body.camera_id}' not found",
            )

    incident = Incident(
        org_id=user.org_id,
        camera_id=body.camera_id,
        title=title[:200],
        summary=summary,
        severity=body.severity,
        status="open",
        created_by=f"user:{user.user_id}",
    )
    db.add(incident)
    db.commit()
    db.refresh(incident)

    # Fire inbox + email notification.  Same kind/audience the MCP
    # path uses (`mcp/server.py:1295-1306`) — we want both author
    # types to flow through identical notification preferences.
    try:
        from app.api.notifications import create_notification

        notif_severity = (
            "critical" if body.severity in ("high", "critical") else "warning"
        )
        create_notification(
            org_id=user.org_id,
            kind="incident_created",
            title=f"Incident #{incident.id}: {incident.title}",
            body=f"[{body.severity.upper()}] {incident.summary}",
            severity=notif_severity,
            audience="all",
            link=f"/incidents/{incident.id}",
            camera_id=body.camera_id,
            meta={"incident_id": incident.id, "severity": body.severity},
            db=db,
        )
    except Exception:  # noqa: BLE001
        # Notification failure must NEVER fail incident creation —
        # the row is already committed and the operator clicked
        # submit.  Log and move on; an admin can retry the notify
        # path manually if needed.
        logger.exception(
            "[create_incident] notification emit failed for incident=%s",
            incident.id,
        )

    return incident.to_dict(include_evidence=True)


# ---------------------------------------------------------------------------
# List + counts
# ---------------------------------------------------------------------------


@router.get("")
async def list_incidents(
    status: Optional[str] = Query(default=None, description="Filter by status"),
    severity: Optional[str] = Query(default=None, description="Filter by severity"),
    camera_id: Optional[str] = Query(default=None),
    limit: int = Query(default=50, ge=1, le=200),
    # OFFSET is O(n) — cap so no one can force SQLite to skip billions.
    offset: int = Query(default=0, ge=0, le=1_000_000),
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """List incident reports for the org, newest first."""
    query = db.query(Incident).filter(Incident.org_id == user.org_id)

    if status:
        if status not in INCIDENT_STATUSES:
            raise HTTPException(status_code=400, detail=f"Invalid status: {status}")
        query = query.filter(Incident.status == status)
    if severity:
        if severity not in INCIDENT_SEVERITIES:
            raise HTTPException(status_code=400, detail=f"Invalid severity: {severity}")
        query = query.filter(Incident.severity == severity)
    if camera_id:
        query = query.filter(Incident.camera_id == camera_id)

    total = query.count()
    incidents = (
        query.order_by(Incident.created_at.desc())
        .offset(offset)
        .limit(limit)
        .all()
    )

    return {
        "total": total,
        "limit": limit,
        "offset": offset,
        "incidents": [i.to_dict() for i in incidents],
    }


@router.get("/counts")
async def incident_counts(
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Quick aggregate counts for the stat bar / badges."""
    base = db.query(Incident).filter(Incident.org_id == user.org_id)
    open_count = base.filter(Incident.status == "open").count()
    critical_open = base.filter(
        Incident.status == "open", Incident.severity == "critical"
    ).count()
    high_open = base.filter(
        Incident.status == "open", Incident.severity == "high"
    ).count()
    return {
        "open": open_count,
        "open_critical": critical_open,
        "open_high": high_open,
        "total": base.count(),
    }


# ---------------------------------------------------------------------------
# Detail / update / delete
# ---------------------------------------------------------------------------


def _get_owned_incident(db: Session, org_id: str, incident_id: int) -> Incident:
    incident = (
        db.query(Incident)
        .filter(Incident.id == incident_id, Incident.org_id == org_id)
        .first()
    )
    if not incident:
        raise HTTPException(status_code=404, detail="Incident not found")
    return incident


@router.get("/{incident_id}")
async def get_incident(
    incident_id: int,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Fetch a single incident with all of its evidence."""
    incident = _get_owned_incident(db, user.org_id, incident_id)
    return incident.to_dict(include_evidence=True)


@router.patch("/{incident_id}")
@limiter.limit("120/minute")
async def update_incident(
    request: Request,
    incident_id: int,
    patch: IncidentPatch,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Acknowledge, resolve, dismiss, or otherwise edit an incident."""
    incident = _get_owned_incident(db, user.org_id, incident_id)

    if patch.status is not None:
        if patch.status not in INCIDENT_STATUSES:
            raise HTTPException(status_code=400, detail=f"Invalid status: {patch.status}")
        # Mark resolution metadata when transitioning to a terminal state
        if patch.status in ("resolved", "dismissed") and incident.status not in (
            "resolved",
            "dismissed",
        ):
            incident.resolved_at = datetime.now(tz=UTC).replace(tzinfo=None)
            incident.resolved_by = f"user:{user.user_id}"
        elif patch.status == "open":
            # Re-opening clears the resolution
            incident.resolved_at = None
            incident.resolved_by = None
        incident.status = patch.status

    if patch.severity is not None:
        if patch.severity not in INCIDENT_SEVERITIES:
            raise HTTPException(status_code=400, detail=f"Invalid severity: {patch.severity}")
        incident.severity = patch.severity

    if patch.summary is not None:
        incident.summary = patch.summary
    if patch.report is not None:
        incident.report = patch.report

    db.commit()
    db.refresh(incident)
    return incident.to_dict(include_evidence=True)


@router.delete("/{incident_id}")
@limiter.limit("60/minute")
async def delete_incident(
    request: Request,
    incident_id: int,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Delete an incident and all of its evidence (cascades)."""
    incident = _get_owned_incident(db, user.org_id, incident_id)
    db.delete(incident)
    db.commit()
    return {"deleted": incident_id}


# ---------------------------------------------------------------------------
# Evidence — fetch a snapshot blob
# ---------------------------------------------------------------------------


@router.get("/{incident_id}/evidence/{evidence_id}")
@limiter.limit("120/minute")
async def get_incident_evidence(
    request: Request,
    incident_id: int,
    evidence_id: int,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Stream a snapshot or clip blob for an evidence item.
    Returns 404 if the incident doesn't belong to the caller's org or
    if the evidence row has no binary payload.

    Rate-limited to 120 req/min per org.  This endpoint serves
    arbitrary-size video blobs from the DB and bypasses the
    viewer-hour cap that gates the live HLS endpoints — a per-org
    cap here keeps it from becoming a poor man's bandwidth tap.
    """
    # Org check via the parent incident
    _get_owned_incident(db, user.org_id, incident_id)

    evidence = (
        db.query(IncidentEvidence)
        .filter(
            IncidentEvidence.id == evidence_id,
            IncidentEvidence.incident_id == incident_id,
        )
        .first()
    )
    if not evidence or not evidence.data:
        raise HTTPException(status_code=404, detail="Evidence blob not found")

    # Strip any MIME parameters (we use video/mp2t;duration=N internally to
    # remember clip length without a schema migration — browsers don't need it).
    raw_mime = evidence.data_mime or "application/octet-stream"
    media_type = raw_mime.split(";", 1)[0].strip() or "application/octet-stream"

    return Response(
        content=evidence.data,
        media_type=media_type,
        headers={"Cache-Control": "private, max-age=300"},
    )


@router.get("/{incident_id}/evidence/{evidence_id}/playlist.m3u8")
@limiter.limit("120/minute")
async def get_incident_evidence_playlist(
    request: Request,
    incident_id: int,
    evidence_id: int,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Synthetic single-segment HLS playlist for a clip evidence blob.
    Lets the dashboard reuse hls.js to play attach_clip captures with the
    same JWT auth as the live player. Returns 404 unless the evidence is a
    clip with attached video data.

    Rate-limited to 120 req/min per org — same cap as the evidence
    blob endpoint it points at; both are per-call cheap but together
    they're a video-bandwidth path that should match the HLS
    endpoints' protection model.
    """
    _get_owned_incident(db, user.org_id, incident_id)

    evidence = (
        db.query(IncidentEvidence)
        .filter(
            IncidentEvidence.id == evidence_id,
            IncidentEvidence.incident_id == incident_id,
        )
        .first()
    )
    if not evidence or not evidence.data or evidence.kind != "clip":
        raise HTTPException(status_code=404, detail="Clip not found")

    # Pull the duration parameter back out of the stored mime, falling back
    # to a generous default that's >= max EXTINF (HLS spec requirement).
    duration = 60.0
    raw_mime = evidence.data_mime or ""
    if ";" in raw_mime:
        for param in raw_mime.split(";")[1:]:
            param = param.strip()
            if param.startswith("duration="):
                try:
                    duration = float(param.split("=", 1)[1])
                except (ValueError, IndexError):
                    pass
    target_duration = max(1, int(duration) + 1)

    # Use an absolute path for the segment so hls.js doesn't try to resolve it
    # against the playlist URL (which lives at .../playlist.m3u8 — relative
    # resolution would land in the wrong place).
    segment_url = f"/api/incidents/{incident_id}/evidence/{evidence_id}"

    playlist = (
        "#EXTM3U\n"
        "#EXT-X-VERSION:3\n"
        f"#EXT-X-TARGETDURATION:{target_duration}\n"
        "#EXT-X-MEDIA-SEQUENCE:0\n"
        "#EXT-X-PLAYLIST-TYPE:VOD\n"
        f"#EXTINF:{duration:.3f},\n"
        f"{segment_url}\n"
        "#EXT-X-ENDLIST\n"
    )

    return Response(
        content=playlist,
        media_type="application/vnd.apple.mpegurl",
        headers={"Cache-Control": "private, max-age=300"},
    )
