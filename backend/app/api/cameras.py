import logging
from datetime import UTC, datetime
from typing import Optional

from fastapi import APIRouter, Depends, HTTPException, Query, Request
from sqlalchemy.orm import Session

from app.core.audit import audit_label, write_audit
from app.core.auth import AuthUser, require_admin, require_view
from app.core.codec import sanitize_video_codec
from app.core.csv_export import filename_for, stream_csv_response
from app.core.database import get_db
from app.core.limiter import limiter
from app.models.models import AuditLog, Camera, CameraGroup, Setting
from app.schemas.schemas import (
    CameraGroupCreate,
    CameraRecordingPolicy,
    NotificationSettings,
)

# Shared defaults for notification toggles — used by the GET handler and
# the /api/settings aggregate.  Pydantic's NotificationSettings holds the
# canonical values; this dict re-expresses them as strings for Setting.get_many.
_NOTIFICATION_SETTING_DEFAULTS = {
    "motion_notifications": "true",
    "camera_transition_notifications": "true",
    "node_transition_notifications": "true",
}

router = APIRouter(prefix="/api", tags=["api"])
logger = logging.getLogger(__name__)


# Camera CRUD
@router.get("/cameras")
async def list_cameras(
    user: AuthUser = Depends(require_view), db: Session = Depends(get_db)
):
    """List all cameras for the user's organization."""
    from sqlalchemy.orm import selectinload

    from app.models.models import CameraNode

    # selectinload: Camera.to_dict() reads .node and .group — without
    # eager loading that's an N+1 lazy query per distinct node/group on
    # a route the dashboard polls every 5 seconds per open tab.
    cameras = (
        db.query(Camera)
        .options(selectinload(Camera.node), selectinload(Camera.group))
        .filter_by(org_id=user.org_id)
        .all()
    )

    # Check for orphaned cameras in a single query instead of N+1
    if cameras:
        node_ids = {cam.node_id for cam in cameras if cam.node_id}
        if node_ids:
            existing_node_ids = {
                n.id
                for n in db.query(CameraNode.id)
                .filter(CameraNode.id.in_(node_ids))
                .all()
            }
            for cam in cameras:
                if cam.node_id and cam.node_id not in existing_node_ids:
                    logger.warning(
                        "Orphan camera %s (node_id=%s not found)",
                        cam.camera_id,
                        cam.node_id,
                    )

    result = [c.to_dict() for c in cameras]
    logger.debug("Returning %d cameras for org %s", len(result), user.org_id)
    return result


@router.get("/cameras/{camera_id}")
async def get_camera(
    camera_id: str,
    user: AuthUser = Depends(require_view),
    db: Session = Depends(get_db),
):
    """Get a specific camera by ID."""
    camera = db.query(Camera).filter_by(camera_id=camera_id, org_id=user.org_id).first()
    if not camera:
        raise HTTPException(status_code=404, detail="Camera not found")
    return camera.to_dict()


@router.post("/cameras/{camera_id}/snapshot")
@limiter.limit("30/minute")
async def take_snapshot(
    camera_id: str,
    request: Request,
    user: AuthUser = Depends(require_view),
    db: Session = Depends(get_db),
):
    """Tell the camera node to capture and store a snapshot locally."""
    from app.api.ws import manager
    from app.models.models import CameraNode

    camera = db.query(Camera).filter_by(camera_id=camera_id, org_id=user.org_id).first()
    if not camera:
        raise HTTPException(status_code=404, detail="Camera not found")
    if not camera.node_id:
        raise HTTPException(status_code=400, detail="Camera has no assigned node")

    node = db.query(CameraNode).filter_by(id=camera.node_id).first()
    if not node:
        raise HTTPException(status_code=400, detail="Camera node not found")
    if not manager.is_connected(node.node_id):
        raise HTTPException(status_code=503, detail="Camera node is offline")

    try:
        result = await manager.send_command(
            node.node_id,
            "take_snapshot",
            {"camera_id": camera_id},
            timeout=15.0,
        )
        return result
    except TimeoutError:
        raise HTTPException(status_code=504, detail="Snapshot request timed out") from None
    except ValueError as e:
        raise HTTPException(status_code=503, detail=str(e)) from e


@router.post("/cameras/{camera_id}/recording")
@limiter.limit("30/minute")
async def toggle_recording(
    camera_id: str,
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Start or stop continuous recording on this camera (manual
    record button on the dashboard).  Admin-only — recording state
    changes are operational decisions, not view-only.

    Implementation note: as of v0.1.43 this is a thin wrapper that
    flips ``continuous_24_7`` on the Camera row.  The heartbeat
    handler reconciles CameraNode's in-memory recording state from
    that field every ~30s, so a manual press here lands within one
    heartbeat (no separate WebSocket command needed, no in-memory
    state that gets lost when the node restarts).
    """
    body = await request.json()
    recording = bool(body.get("recording", False))

    camera = db.query(Camera).filter_by(camera_id=camera_id, org_id=user.org_id).first()
    if not camera:
        raise HTTPException(status_code=404, detail="Camera not found")

    camera.continuous_24_7 = recording
    db.commit()

    write_audit(
        db,
        org_id=user.org_id,
        event="recording_toggled",
        user_id=user.user_id,
        username=audit_label(user),
        details={"camera_id": camera_id, "recording": recording},
        request=request,
    )
    return {"success": True, "camera_id": camera_id, "recording": recording}


@router.patch("/cameras/{camera_id}/recording-settings")
@limiter.limit("30/minute")
async def update_camera_recording_policy(
    camera_id: str,
    data: CameraRecordingPolicy,
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Update the per-camera recording policy.

    Replaces the (never-actually-wired) org-level
    ``POST /api/settings/recording`` endpoint with a per-camera one.
    Each field is optional so a PATCH can flip just one toggle without
    re-asserting the others — the operator toggling Continuous 24/7
    on doesn't need to also pass `scheduled_recording: false` etc.

    The chosen state is persisted on the Camera row; the heartbeat
    handler computes the camera's *current* desired recording state
    on each tick (continuous OR (scheduled AND in-window)) and the
    CameraNode reconciler applies it.
    """
    camera = db.query(Camera).filter_by(camera_id=camera_id, org_id=user.org_id).first()
    if not camera:
        raise HTTPException(status_code=404, detail="Camera not found")

    # Apply the patch into local variables first so we can validate
    # the *resulting* state before commit.  The two recording-mode
    # toggles are mutually exclusive (see invariant comment below);
    # rejecting the bad combination here means the DB never has an
    # impossible row, even if a direct API caller tries to set both.
    next_continuous = (
        data.continuous_24_7
        if data.continuous_24_7 is not None
        else camera.continuous_24_7
    )
    next_scheduled = (
        data.scheduled_recording
        if data.scheduled_recording is not None
        else camera.scheduled_recording
    )

    # Invariant: at most ONE recording mode active at a time.  The
    # heartbeat handler treats `continuous_24_7 OR (scheduled AND
    # in-window)` so both-true silently makes scheduled a no-op,
    # which is confusing UX — better to fail loud here and force the
    # caller to pick a mode.  Frontend toggles the other off
    # automatically when the user flips one on, so this 422 only
    # fires for direct API / MCP calls that pass both true.
    if next_continuous and next_scheduled:
        raise HTTPException(
            status_code=422,
            detail=(
                "continuous_24_7 and scheduled_recording cannot both be "
                "true. Pick one mode — continuous OR scheduled — or "
                "turn the existing one off in the same PATCH."
            ),
        )

    if data.continuous_24_7 is not None:
        camera.continuous_24_7 = data.continuous_24_7
    if data.scheduled_recording is not None:
        camera.scheduled_recording = data.scheduled_recording
    if data.scheduled_start is not None:
        camera.scheduled_start = data.scheduled_start or None
    if data.scheduled_end is not None:
        camera.scheduled_end = data.scheduled_end or None

    db.commit()
    write_audit(
        db,
        org_id=user.org_id,
        event="camera_recording_policy_updated",
        user_id=user.user_id,
        username=audit_label(user),
        details={
            "camera_id": camera_id,
            "continuous_24_7": camera.continuous_24_7,
            "scheduled_recording": camera.scheduled_recording,
            "scheduled_start": camera.scheduled_start,
            "scheduled_end": camera.scheduled_end,
        },
        request=request,
    )
    return {
        "success": True,
        "camera_id": camera_id,
        "recording_policy": {
            "continuous_24_7": camera.continuous_24_7,
            "scheduled_recording": camera.scheduled_recording,
            "scheduled_start": camera.scheduled_start,
            "scheduled_end": camera.scheduled_end,
        },
    }


# Camera Groups
@router.get("/camera-groups")
async def list_camera_groups(
    user: AuthUser = Depends(require_view), db: Session = Depends(get_db)
):
    """List all camera groups for the user's organization."""
    groups = db.query(CameraGroup).filter_by(org_id=user.org_id).all()
    return [g.to_dict() for g in groups]


@router.post("/camera-groups")
@limiter.limit("20/minute")
async def create_camera_group(
    data: CameraGroupCreate,
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Create a new camera group."""
    if db.query(CameraGroup).filter_by(org_id=user.org_id, name=data.name).first():
        raise HTTPException(status_code=400, detail="Group name already exists")

    group = CameraGroup(
        org_id=user.org_id,
        name=data.name,
        color=data.color,
        icon=data.icon,
    )
    db.add(group)
    db.commit()

    return {"success": True, "id": group.id, "name": group.name}


@router.delete("/camera-groups/{group_id}")
@limiter.limit("60/minute")
async def delete_camera_group(
    request: Request,
    group_id: int,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Delete a camera group."""
    group = db.query(CameraGroup).filter_by(id=group_id, org_id=user.org_id).first()
    if not group:
        raise HTTPException(status_code=404, detail="Group not found")

    for camera in group.cameras:
        camera.group_id = None

    db.delete(group)
    db.commit()

    return {"success": True, "deleted": group.name}


@router.put("/cameras/{camera_id}/group")
@limiter.limit("60/minute")
async def assign_camera_group(
    camera_id: str,
    request: Request,
    group_id: int = None,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Assign a camera to a group."""
    camera = db.query(Camera).filter_by(camera_id=camera_id, org_id=user.org_id).first()
    if not camera:
        raise HTTPException(status_code=404, detail="Camera not found")

    if group_id:
        group = db.query(CameraGroup).filter_by(id=group_id, org_id=user.org_id).first()
        if not group:
            raise HTTPException(status_code=404, detail="Group not found")
        camera.group_id = group_id
    else:
        camera.group_id = None

    db.commit()
    return {"success": True, "camera_id": camera_id, "group_id": group_id}


# Settings
@router.get("/settings")
async def get_all_settings(
    user: AuthUser = Depends(require_view), db: Session = Depends(get_db)
):
    """Get org-level settings (notifications + timezone).  Recording
    config moved per-camera in v0.1.43 — see ``Camera.recording_policy``
    in the ``/api/cameras`` response and the
    ``PATCH /api/cameras/{id}/recording-settings`` endpoint to update it.
    """
    vals = Setting.get_many(db, user.org_id, _NOTIFICATION_SETTING_DEFAULTS)
    timezone_name = Setting.get(db, user.org_id, "timezone", "UTC") or "UTC"
    return {
        "notifications": {
            "motion_notifications": vals["motion_notifications"] == "true",
            "camera_transition_notifications": vals["camera_transition_notifications"]
            == "true",
            "node_transition_notifications": vals["node_transition_notifications"]
            == "true",
        },
        # IANA timezone name (e.g. "America/Los_Angeles", "UTC").
        # Drives the wall-clock interpretation of per-camera
        # `scheduled_start` / `scheduled_end` in the heartbeat
        # handler — see `_camera_should_record_now` in api/nodes.py.
        # Defaults to "UTC" for orgs that haven't set one (back-compat
        # with v0.1.43 nodes that pre-date the per-org timezone).
        "timezone": timezone_name,
    }


@router.post("/settings/timezone")
@limiter.limit("30/minute")
async def update_org_timezone(
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Set the org's IANA timezone name (e.g. "America/Los_Angeles").

    The heartbeat handler's per-camera scheduled-recording window check
    uses this to interpret HH:MM start/end times as the operator's
    local wall clock instead of UTC.  DST transitions are handled by
    Python's ``zoneinfo`` for free — a "08:00–17:00" schedule in
    America/Los_Angeles fires at 8am local year-round, no operator
    intervention needed at the spring-forward / fall-back boundaries.

    Validates against the IANA database (``zoneinfo.available_timezones``)
    so a typo or made-up zone name 422s rather than landing in the DB
    and silently breaking the schedule for the affected org.
    """
    from zoneinfo import available_timezones
    body = await request.json()
    tz_name = (body.get("timezone") or "").strip()
    if not tz_name:
        raise HTTPException(
            status_code=422,
            detail="`timezone` is required (IANA name like 'America/Los_Angeles' or 'UTC')",
        )
    if tz_name not in available_timezones():
        raise HTTPException(
            status_code=422,
            detail=(
                f"Unknown timezone {tz_name!r}. Use an IANA name like "
                "'America/Los_Angeles', 'Europe/London', or 'UTC'."
            ),
        )
    Setting.set(db, user.org_id, "timezone", tz_name)
    write_audit(
        db,
        org_id=user.org_id,
        event="timezone_updated",
        user_id=user.user_id,
        username=audit_label(user),
        details={"timezone": tz_name},
        request=request,
    )
    return {"success": True, "timezone": tz_name}


# Notification preferences.  GET is view-level (every member needs to
# know what's on), POST is admin-only.  Recording settings used to live
# alongside these as org-level toggles; v0.1.43 moved them per-camera.


@router.get("/settings/notifications")
async def get_notification_settings(
    user: AuthUser = Depends(require_view), db: Session = Depends(get_db)
):
    """Return the org's notification preferences.

    Defaults to "all on" for backward compat with orgs that existed
    before the settings UI landed — the gate only starts filtering
    after an admin explicitly flips a toggle off.
    """
    vals = Setting.get_many(db, user.org_id, _NOTIFICATION_SETTING_DEFAULTS)
    return {
        "motion_notifications": vals["motion_notifications"] == "true",
        "camera_transition_notifications": vals["camera_transition_notifications"]
        == "true",
        "node_transition_notifications": vals["node_transition_notifications"]
        == "true",
    }


@router.get("/settings/motion-ingestion")
async def get_motion_ingestion_setting(
    user: AuthUser = Depends(require_view), db: Session = Depends(get_db)
):
    """Return whether server-side motion-event ingestion is enabled
    for this org.

    Defaults to enabled — the kill switch only takes effect after an
    admin explicitly flips it off via POST.  This is a safety valve
    for runaway sensors flooding events; under normal operation it
    stays on and the per-camera recording policy is the granularity
    operators usually want.
    """
    enabled = Setting.get(db, user.org_id, "motion_ingestion_enabled", "true").lower() == "true"
    return {"motion_ingestion_enabled": enabled}


@router.post("/settings/motion-ingestion")
@limiter.limit("30/minute")
async def update_motion_ingestion_setting(
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Toggle server-side motion-event ingestion for the org.

    Body: ``{"enabled": true|false}``.

    When set to false, ``POST /api/cameras/{id}/motion`` short-circuits
    with ``{"ingested": false}`` and no MotionEvent rows are written.
    Stops a misbehaving / mis-configured camera or node from flooding
    the events table without requiring physical access to the node.
    Audited so there's a record of who flipped it.
    """
    payload = await request.json()
    enabled = bool(payload.get("enabled"))
    Setting.set(
        db, user.org_id, "motion_ingestion_enabled", "true" if enabled else "false"
    )
    write_audit(
        db,
        org_id=user.org_id,
        event="motion_ingestion_toggled",
        user_id=user.user_id,
        username=audit_label(user),
        details={"enabled": enabled},
        request=request,
    )
    return {"motion_ingestion_enabled": enabled}


@router.post("/settings/notifications")
@limiter.limit("30/minute")
async def update_notification_settings(
    data: NotificationSettings,
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Update notification preferences. Requires admin.

    Persists each toggle as a stringified bool so the existing Setting
    key/value table can store it without a schema change — same
    convention the recording toggles use.
    """
    Setting.set(
        db, user.org_id, "motion_notifications", str(data.motion_notifications).lower()
    )
    Setting.set(
        db,
        user.org_id,
        "camera_transition_notifications",
        str(data.camera_transition_notifications).lower(),
    )
    Setting.set(
        db,
        user.org_id,
        "node_transition_notifications",
        str(data.node_transition_notifications).lower(),
    )
    write_audit(
        db,
        org_id=user.org_id,
        event="notification_settings_updated",
        user_id=user.user_id,
        username=audit_label(user),
        details={
            "motion_notifications": bool(data.motion_notifications),
            "camera_transition_notifications": bool(
                data.camera_transition_notifications
            ),
            "node_transition_notifications": bool(data.node_transition_notifications),
        },
        request=request,
    )
    return {"success": True}


# Audit Logs
@router.get("/audit-logs")
@limiter.limit("120/minute")
async def list_audit_logs(
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
    # Filters — all optional; the dashboard's filter UI sends them
    # only when the operator narrows the view.  The same filters
    # apply to the CSV branch so an exported audit window matches
    # what the operator was looking at on screen.
    event: Optional[str] = Query(default=None),
    username: Optional[str] = Query(default=None),
    # Pagination matches the sibling /api/audit/stream-logs +
    # /api/mcp/activity/logs response shape so the frontend can
    # share the pagination component logic across all three.
    # OFFSET cap defends against a "skip a billion rows" attack
    # — SQLite's OFFSET is O(n) even with an index.
    limit: int = Query(default=100, ge=1, le=500),
    offset: int = Query(default=0, ge=0, le=1_000_000),
    # ``format=csv`` switches the response from JSON to a streaming
    # CSV download.  Honoured for the same admin auth as the JSON
    # path; same per-org bucket counts against the 120/min limit.
    # When CSV is requested the ``limit`` cap is raised to give an
    # auditor a meaningful export window — see the CSV branch below.
    format: str = Query(default="json", pattern="^(json|csv)$"),
):
    """List audit logs for the user's organization.

    Returns ``{total, limit, offset, logs}`` JSON by default — same
    shape as ``/api/audit/stream-logs`` and ``/api/mcp/activity/logs``
    so the dashboard's pagination/filter logic is shared across the
    three audit surfaces.

    Pass ``?format=csv`` for a streaming download suitable for
    spreadsheet review or compliance archiving.  Filters apply to
    both JSON and CSV.
    """
    query = db.query(AuditLog).filter(AuditLog.org_id == user.org_id)

    if event:
        query = query.filter(AuditLog.event == event)

    if username:
        # Case-insensitive substring match on the human-readable
        # username field.  Admin-only endpoint so this is filter
        # precision, not a security boundary; underscores in usernames
        # are common (clerk_user_xyz) so escape SQL-LIKE wildcards
        # the user doesn't actually want.
        escaped = (
            username
            .replace("\\", "\\\\")
            .replace("%", "\\%")
            .replace("_", "\\_")
        )
        query = query.filter(
            AuditLog.username.ilike(f"%{escaped}%", escape="\\")
        )

    if format == "csv":
        # CSV exports are bound by row count, not by JSON payload size.
        # Lift the limit substantially so an org admin can grab a
        # meaningful chunk of history in one call.  At 50k rows × ~200
        # bytes/row = ~10 MB CSV — within reasonable download size and
        # still constant-memory thanks to the streaming generator.
        csv_query = query.order_by(AuditLog.timestamp.desc()).limit(50_000)

        def _rows():
            for log in csv_query.yield_per(500):
                yield [
                    log.timestamp.isoformat() if log.timestamp else "",
                    log.event or "",
                    log.username or "",
                    log.user_id or "",
                    log.ip_address or "",
                    log.details or "",
                ]

        return stream_csv_response(
            filename=filename_for("audit-log", user.org_id),
            header=["timestamp", "event", "username", "user_id", "ip_address", "details"],
            rows=_rows(),
        )

    total = query.count()
    logs = (
        query.order_by(AuditLog.timestamp.desc())
        .offset(offset)
        .limit(limit)
        .all()
    )
    return {
        "total": total,
        "limit": limit,
        "offset": offset,
        "logs": [l.to_dict() for l in logs],
    }


# Health Check (for API endpoint health)
@router.post("/cameras/{camera_id}/codec")
@limiter.limit("30/minute")
async def report_camera_codec(
    camera_id: str,
    request: Request,
    db: Session = Depends(get_db),
):
    """
    Report video/audio codec for a camera.
    Called by CameraNode after detecting codec from first segment.
    """
    import hashlib

    from app.models.models import CameraNode

    # Verify node API key
    node_api_key = request.headers.get("X-Node-API-Key")
    if not node_api_key:
        raise HTTPException(status_code=401, detail="Missing API key")

    api_key_hash = hashlib.sha256(node_api_key.encode()).hexdigest()
    node = db.query(CameraNode).filter_by(api_key_hash=api_key_hash).first()
    if not node:
        raise HTTPException(status_code=401, detail="Invalid API key")

    # Verify camera belongs to this node AND node's org — defense-in-depth
    # against any future schema drift where camera.org_id and node.org_id
    # could diverge.  The camera_id column has a unique constraint, so
    # today this check is redundant; tomorrow it might not be.
    camera = (
        db.query(Camera)
        .filter_by(camera_id=camera_id, node_id=node.id, org_id=node.org_id)
        .first()
    )
    if not camera:
        raise HTTPException(status_code=404, detail="Camera not found")

    # Parse codec info from request body
    import json

    try:
        body = await request.body()
        codec_data = json.loads(body)
        video_codec = codec_data.get("video_codec")
        audio_codec = codec_data.get("audio_codec")
    except Exception:
        raise HTTPException(status_code=400, detail="Invalid request body") from None

    if not video_codec:
        raise HTTPException(status_code=400, detail="video_codec is required")

    # Codec strings are stored for diagnostics and MCP tool reporting —
    # reject newlines or absurd lengths to prevent playlist corruption.
    if len(video_codec) > 64 or "\n" in video_codec or "\r" in video_codec:
        raise HTTPException(status_code=400, detail="Invalid video_codec format")
    if audio_codec and (
        len(audio_codec) > 64 or "\n" in audio_codec or "\r" in audio_codec
    ):
        raise HTTPException(status_code=400, detail="Invalid audio_codec format")

    # Defensive sanitization — older CameraNode builds shipped garbage
    # H.264 codec strings (level 1.0) for the Pi's h264_v4l2m2m encoder.
    # Catch them server-side so a stale binary in the field doesn't
    # silently brick streaming again.
    video_codec = sanitize_video_codec(video_codec)

    # Update camera codec fields
    camera.video_codec = video_codec
    camera.audio_codec = audio_codec or "mp4a.40.2"  # Default to AAC-LC
    camera.codec_detected_at = datetime.now(tz=UTC).replace(tzinfo=None)

    # Also update node codec if this is the first camera to detect
    if node and not node.video_codec:
        node.video_codec = video_codec
        node.audio_codec = camera.audio_codec
        node.codec_detected_at = datetime.now(tz=UTC).replace(tzinfo=None)
        logger.info(
            "Updated node %s codec: video=%s, audio=%s",
            node.node_id,
            video_codec,
            camera.audio_codec,
        )

    db.commit()

    logger.info(
        "Updated codec for camera %s: video=%s, audio=%s",
        camera_id,
        video_codec,
        camera.audio_codec,
    )

    return {"success": True, "message": "Codec updated"}


# ── Danger Zone ──────────────────────────────────────────────────────


def _require_active_paid_plan(user: AuthUser, db: Session) -> None:
    """Gate destructive danger-zone endpoints on BOTH the JWT-claimed
    feature AND the DB-resolved plan.

    The JWT ``features`` claim is what Clerk asserts the user has on
    their current session token — it refreshes ~once per minute,
    which means a user who downgrades from Pro → Free retains the
    ``admin`` feature in their token for up to that window.  Without
    this server-side double-check, a recently-downgraded user could
    fire ``/wipe-logs`` or ``/full-reset`` on the strength of a
    soon-to-be-stale claim, and the destructive action would land
    despite their active plan no longer permitting it.

    ``effective_plan_for_caps`` reads the DB-cached ``org_plan``
    setting which the Clerk webhook handler keeps current within
    seconds of an upgrade/downgrade.  That's the authoritative
    answer for "is this org currently on a paid plan, right now,
    not at the time the JWT was issued."

    Raises 403 if either check fails.  Both are required: the JWT
    check is the cheap pre-filter, the DB check is the
    point-of-truth.
    """
    from app.core.plans import PAID_PLAN_SLUGS, effective_plan_for_caps

    if "admin" not in user.features:
        raise HTTPException(
            status_code=403,
            detail="Danger zone requires a Pro or Pro Plus plan.",
        )

    current_plan = effective_plan_for_caps(db, user.org_id)
    if current_plan not in PAID_PLAN_SLUGS:
        raise HTTPException(
            status_code=403,
            detail=(
                "Danger zone requires an active paid plan; the current "
                "billing record for this organization doesn't include "
                "this feature.  If you just upgraded, sign out and back "
                "in to refresh your session."
            ),
        )


@router.post("/settings/danger/wipe-logs")
@limiter.limit("5/hour")
async def wipe_stream_logs(
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Permanently delete all stream access + MCP activity logs for
    this organization.

    Plan gate: Pro / Pro Plus.  This is operator-convenience —
    selective audit-log hygiene that keeps the org otherwise running.
    It is **not** the GDPR right-to-erasure endpoint (that's
    ``/full-reset`` below, which is available on every plan); a Free
    -tier customer who wants to purge their stream-access history
    can always use Full Reset and re-add their cameras after.  The
    paid gate here is the same JWT + DB double-check pattern used
    on every Pro-only operation; see ``_require_active_paid_plan``.
    """
    _require_active_paid_plan(user, db)
    from app.models import StreamAccessLog

    count = db.query(StreamAccessLog).filter_by(org_id=user.org_id).delete()
    from app.models.models import McpActivityLog

    mcp_count = db.query(McpActivityLog).filter_by(org_id=user.org_id).delete()
    db.commit()
    logger.warning(
        "Admin wiped %d stream + %d MCP logs (org redacted)", count, mcp_count
    )
    write_audit(
        db,
        org_id=user.org_id,
        event="logs_wiped",
        user_id=user.user_id,
        username=audit_label(user),
        details={"stream_logs_deleted": count, "mcp_logs_deleted": mcp_count},
        request=request,
    )
    return {"success": True, "deleted_logs": count, "deleted_mcp_logs": mcp_count}


@router.post("/settings/danger/full-reset")
@limiter.limit("3/hour")
async def full_reset(
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """
    Full organization reset — the GDPR Article 17 right-to-erasure path.

    Behaviour:
      1. Send each CameraNode a ``wipe_data`` command so the per-node
         local recordings + encrypted blobs are erased on the device.
      2. Clean up Command Center in-memory caches (HLS segments,
         broadcaster subscribers — those survive a DB delete).
      3. Delete every row across every org-scoped table via the
         shared ``app.core.gdpr.delete_org_data`` helper — the same
         path the GDPR Article 17 endpoint uses, so a customer
         clicking "Delete my data" and an operator clicking "Full
         Reset" produce identical end-states.

    Previously this endpoint only cleared 5 tables (Settings, Audit,
    Stream, MCP activity, CameraNode/Camera).  The other 10 (motion
    events, notifications, incidents, email logs, monthly usage,
    etc.) silently persisted, which was both an Article 17 violation
    and a quiet way for stale data to leak across cancellations.

    Plan gate: **none**.  Every plan (Free included) can self-serve
    full erasure of their organization's data — this is the GDPR
    Article 17 right-to-erasure path, which is a legal requirement
    we cannot gate behind a paid plan.  The sibling ``wipe-logs``
    endpoint *is* still paid-only because it's an operator-convenience
    feature (selective audit-log hygiene that keeps the org running),
    not a right-to-erasure obligation.  Admin-only via
    ``require_admin``, typed-confirmation in the UI, audit-logged
    before the cascade runs, and rate-limited to 3/hour so a
    runaway script can't repeatedly nuke an org by accident.
    """
    from app.api.hls import cleanup_camera_cache
    from app.api.ws import manager
    from app.core.gdpr import delete_org_data
    from app.models import CameraNode

    nodes_wiped = 0

    # 1. Tell each CameraNode to wipe its local data BEFORE we delete
    # the row.  After deletion we wouldn't have the node_id to send
    # to.  Failures here are logged but don't block the local delete
    # — a node that's already offline can't ack the command anyway,
    # and we can't leave the customer's CC data sitting around
    # waiting for a node that may never come back.
    nodes = db.query(CameraNode).filter_by(org_id=user.org_id).all()
    for node in nodes:
        try:
            result = await manager.send_command(
                node.node_id, "wipe_data", {}, timeout=10
            )
            if result and result.get("status") == "success":
                nodes_wiped += 1
        except Exception as e:
            logger.warning("Could not send wipe_data to node %s: %s", node.node_id, e)

        # Drop the segment cache for every camera under this node;
        # Query.delete() can't reach the in-memory dict.
        for camera in list(node.cameras):
            cleanup_camera_cache(camera.camera_id)

    # 2. Single-source-of-truth cascade.  ``delete_org_data`` flushes
    # but doesn't commit — commit it HERE, before the audit write.
    # ``write_audit`` commits internally and, on a failed commit,
    # swallows the error and rolls the session back; with the cascade
    # still uncommitted that rollback silently reverted the entire
    # erasure while the handler went on to return success — a false
    # "your data was erased" on an Article-17 obligation.  Committing
    # first makes the deletes durable; the audit row is best-effort
    # after the fact (matching every other commit-then-audit caller).
    counts = delete_org_data(db, user.org_id)
    db.commit()

    results = {
        "nodes_wiped": nodes_wiped,
        # Surfaced fields the existing dashboard JSON expects;
        # the full per-table breakdown is logged + audited below.
        "nodes_deleted":    counts.get("camera_nodes", 0),
        "cameras_deleted":  counts.get("cameras", 0),
        "logs_deleted":     counts.get("stream_access_logs", 0),
        "mcp_logs_deleted": counts.get("mcp_activity_logs", 0),
        "settings_deleted": counts.get("settings", 0),
    }

    logger.warning("Admin performed FULL RESET (org redacted): %s", counts)
    write_audit(
        db,
        org_id=user.org_id,
        event="full_reset",
        user_id=user.user_id,
        username=audit_label(user),
        details=counts,
        request=request,
    )
    return {"success": True, **results}
