"""Integration API — ``/api/integration/*``.

Phase 1: REST integration key (``osi_``) management. These keys authenticate
the data-plane endpoints added in later phases (camera discovery, snapshots,
recording control, motion) via
``app.core.integration_auth.require_integration_org``.

Keys reuse the ``mcp_api_keys`` table with ``kind="integration"``; see that
model plus the cross-kind guards in ``app/mcp/server.py`` and
``app/api/mcp_keys.py`` for why the two key kinds can't cross surfaces.
"""

import asyncio
import hashlib
import json
import logging
import secrets

from fastapi import APIRouter, Depends, HTTPException, Request, Response
from fastapi.responses import StreamingResponse
from sqlalchemy.orm import Session

from app.core.audit import audit_label, write_audit
from app.core.auth import AuthUser, require_admin
from app.core.database import get_db
from app.core.integration_auth import require_integration_org
from app.core.limiter import limiter
from app.models.models import Camera, CameraNode, McpApiKey
from app.schemas.schemas import IntegrationKeyCreate

logger = logging.getLogger(__name__)

router = APIRouter(prefix="/api/integration", tags=["integration"])

KEY_PREFIX = "osi_"

# Per-org cap on concurrent integration motion-SSE streams. This is a
# SEPARATE pool from the dashboard (see integration_motion_broadcaster in
# motion.py), so an HA connection never consumes a dashboard subscriber
# slot. A small fixed cap is plenty — a home runs one or two HA instances —
# and bounds memory against a scripted connect loop.
INTEGRATION_MAX_SSE_SUBSCRIBERS = 10


def _generate_key() -> str:
    """Generate a random integration API key: ``osi_`` + 32 hex chars."""
    return KEY_PREFIX + secrets.token_hex(16)


@router.post("/keys")
@limiter.limit("10/hour")
async def create_integration_key(
    request: Request,
    payload: IntegrationKeyCreate,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Mint an integration key for the org.

    Admin-only. The raw key is returned exactly once; only its SHA-256 hash
    is stored. Not gated on billing — the integration control plane is a
    free, trust-building feature (proxied video still inherits the
    viewer-hour cap downstream).
    """
    raw_key = _generate_key()
    key_hash = hashlib.sha256(raw_key.encode()).hexdigest()

    key = McpApiKey(
        org_id=user.org_id,
        key_hash=key_hash,
        name=payload.name,
        kind="integration",
        scope_mode=None,  # integration keys have no per-tool scoping
        scope_tools=None,
    )
    db.add(key)
    db.commit()
    db.refresh(key)

    write_audit(
        db,
        org_id=user.org_id,
        event="integration_key_created",
        user_id=user.user_id,
        username=audit_label(user),
        details={"key_id": key.id, "name": payload.name},
        request=request,
    )

    # Admin notification — same security-signal pattern as MCP keys: a new
    # credential is a sensitive event, and naming the actor lets a recipient
    # who IS the actor recognise their own action vs. a possible compromise.
    try:
        from app.api.notifications import create_notification
        actor = audit_label(user) or user.user_id or "unknown user"
        create_notification(
            org_id=user.org_id,
            kind="integration_key_created",
            title=f"New integration key created: {payload.name}",
            body=(
                f"{actor} just created a new integration API key "
                f"\"{payload.name}\" (used to connect tools like Home "
                f"Assistant to your cameras). If this was you, no action "
                f"needed. If not, revoke it immediately."
            ),
            severity="warning",
            audience="admin",
            link="/mcp",
            meta={
                "key_id": key.id,
                "key_name": payload.name,
                "actor_user_id": user.user_id,
            },
            db=db,
        )
    except Exception:
        # Audit row already written; losing the notification email is
        # annoying but not a security regression. Don't fail the API call.
        logger.exception(
            "[IntegrationKeys] notification emit failed for key_id=%s", key.id,
        )

    return {
        "id": key.id,
        "name": key.name,
        "key": raw_key,  # Only returned once — never stored in plaintext.
        "created_at": key.created_at.isoformat(),
        "kind": "integration",
        "warning": "Save this key now. You won't be able to see it again.",
    }


@router.get("/keys")
async def list_integration_keys(
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """List the org's integration keys (hashes / plaintext never returned)."""
    keys = (
        db.query(McpApiKey)
        .filter_by(org_id=user.org_id, revoked=False, kind="integration")
        .order_by(McpApiKey.created_at.desc())
        .all()
    )
    return [k.to_dict() for k in keys]


@router.delete("/keys/{key_id}")
@limiter.limit("30/hour")
async def revoke_integration_key(
    key_id: int,
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Revoke an integration key. ``kind="integration"`` scoping means an
    MCP key id passed here 404s rather than crossing surfaces."""
    key = (
        db.query(McpApiKey)
        .filter_by(id=key_id, org_id=user.org_id, kind="integration")
        .first()
    )
    if not key:
        raise HTTPException(status_code=404, detail="Key not found")

    key.revoked = True
    db.commit()

    write_audit(
        db,
        org_id=user.org_id,
        event="integration_key_revoked",
        user_id=user.user_id,
        username=audit_label(user),
        details={"key_id": key_id, "name": key.name},
        request=request,
    )
    return {"success": True}


# ── Data plane (Phase 2) ────────────────────────────────────────────
#
# All routes authenticate with the integration key (require_integration_org)
# and are org-scoped from that key — no plan gate (Home Assistant is
# available to every tier). Video is LAN-direct: the discovery payload hands
# HA the node's own HLS URL so the bytes never traverse the CC proxy or the
# viewer-hour meter. An off-LAN proxy URL (for HA not co-located with the
# cameras) is intentionally deferred — it needs an integration-authed HLS
# proxy + viewer-hour metering on the streaming hot path (Phase 2b) — so
# `proxy_url` is null for now and `local_url` carries the common case.


def _local_stream_url(camera_id: str, node: CameraNode | None) -> str | None:
    """LAN-direct HLS URL, or None when the node can't currently serve it.

    Built only when the node is online and advertising a LAN IP. HA on the
    same network pulls video straight from the node — uncapped for every
    tier, which is the whole point of the Local-direct path.
    """
    if node and node.local_ip and node.effective_status == "online":
        port = node.http_port or 8080
        return f"http://{node.local_ip}:{port}/hls/{camera_id}/stream.m3u8"
    return None


def _camera_dto(cam: Camera) -> dict:
    node = cam.node
    eff = cam.effective_status
    return {
        "id": cam.camera_id,
        "name": cam.name,
        "status": eff,
        "online": eff != "offline",
        "video_codec": cam.video_codec,
        "audio_codec": cam.audio_codec,
        "node_id": node.node_id if node else None,
        "node_name": node.name if node else None,
        "node_online": (node.effective_status == "online") if node else False,
        "recording": bool(cam.continuous_24_7),
        "scheduled_recording": bool(cam.scheduled_recording),
        "snapshot_url": f"/api/integration/cameras/{cam.camera_id}/snapshot",
        "stream": {
            # LAN-direct: works today, uncapped, any tier.
            "local_url": _local_stream_url(cam.camera_id, node),
            # Off-LAN relay through the CC proxy — Phase 2b.
            "proxy_url": None,
        },
    }


@router.get("/cameras")
@limiter.limit("120/minute")
async def integration_list_cameras(
    request: Request,
    user: AuthUser = Depends(require_integration_org),
    db: Session = Depends(get_db),
):
    """Every camera across every node in the org, each with its LAN-direct
    stream URL (when reachable), snapshot URL, and recording state.

    The single call Home Assistant polls to build all its entities; it
    re-syncs as nodes/cameras come and go — no HA reconfiguration on churn.
    """
    cams = (
        db.query(Camera)
        .filter_by(org_id=user.org_id)
        .order_by(Camera.created_at.asc())
        .all()
    )
    return {"cameras": [_camera_dto(c) for c in cams]}


@router.get("/cameras/{camera_id}/snapshot")
@limiter.limit("30/minute")
async def integration_snapshot(
    camera_id: str,
    request: Request,
    user: AuthUser = Depends(require_integration_org),
    db: Session = Depends(get_db),
):
    """Live JPEG for one camera (HA's still image).

    Reuses the same node round-trip the MCP ``view_camera`` tool uses, which
    is org-scoped and checks the node is online. 404 for an unknown camera,
    503 when the node can't currently capture (offline / FFmpeg error).
    """
    from app.mcp.server import _capture_snapshot_bytes

    cam = db.query(Camera).filter_by(camera_id=camera_id, org_id=user.org_id).first()
    if not cam:
        raise HTTPException(status_code=404, detail="Camera not found")

    try:
        jpeg, _node_id = await _capture_snapshot_bytes(user.org_id, camera_id)
    except Exception as e:
        # _capture_snapshot_bytes raises on offline node / capture failure;
        # surface as 503 so HA renders "unavailable" rather than an error.
        raise HTTPException(status_code=503, detail=str(e)) from None

    return Response(
        content=jpeg,
        media_type="image/jpeg",
        headers={"Cache-Control": "no-store"},
    )


@router.post("/cameras/{camera_id}/recording")
@limiter.limit("60/minute")
async def integration_set_recording(
    camera_id: str,
    request: Request,
    user: AuthUser = Depends(require_integration_org),
    db: Session = Depends(get_db),
):
    """Toggle continuous recording — the HA switch.

    Flips ``continuous_24_7`` on the Camera row; the heartbeat reconciler
    drives the node within ~30s (same path as the dashboard record button).
    Body: ``{"recording": bool}``.
    """
    body = await request.json()
    recording = bool(body.get("recording", False))

    cam = db.query(Camera).filter_by(camera_id=camera_id, org_id=user.org_id).first()
    if not cam:
        raise HTTPException(status_code=404, detail="Camera not found")

    cam.continuous_24_7 = recording
    db.commit()

    write_audit(
        db,
        org_id=user.org_id,
        event="recording_toggled",
        user_id=user.user_id,
        username=audit_label(user),
        details={"camera_id": camera_id, "recording": recording, "via": "integration"},
        request=request,
    )
    return {"camera_id": camera_id, "recording": recording}


@router.get("/status")
@limiter.limit("120/minute")
async def integration_status(
    request: Request,
    user: AuthUser = Depends(require_integration_org),
    db: Session = Depends(get_db),
):
    """Org rollup for HA sensors: camera / node online counts, per-node disk
    + version, and the plan (informational). Also the validation target the
    HA config flow hits to confirm the URL + key are good."""
    from app.core.plans import resolve_org_plan

    cams = db.query(Camera).filter_by(org_id=user.org_id).all()
    nodes = db.query(CameraNode).filter_by(org_id=user.org_id).all()

    node_items = [
        {
            "node_id": n.node_id,
            "name": n.name,
            "online": n.effective_status == "online",
            "local_ip": n.local_ip,
            "version": n.node_version,
            "storage": {
                "used_bytes": n.storage_used_bytes,
                "max_bytes": n.storage_max_bytes,
                "disk_free_bytes": n.storage_disk_free_bytes,
                "disk_total_bytes": n.storage_disk_total_bytes,
            },
        }
        for n in nodes
    ]

    return {
        "org_id": user.org_id,
        "plan": resolve_org_plan(db, user.org_id),
        "cameras": {
            "total": len(cams),
            "online": sum(1 for c in cams if c.effective_status != "offline"),
        },
        "nodes": {
            "total": len(nodes),
            "online": sum(1 for n in nodes if n.effective_status == "online"),
            "items": node_items,
        },
    }


@router.get("/motion/stream")
@limiter.limit("60/minute")
async def integration_motion_stream(
    request: Request,
    user: AuthUser = Depends(require_integration_org),
):
    """Server-Sent Events feed of motion detections across ALL of the org's
    cameras — the source for Home Assistant motion ``binary_sensor``s.

    Reuses the same org-wide motion pipeline the dashboard consumes (nodes
    push motion over WS → broadcast), but via a SEPARATE subscriber pool
    (``integration_motion_broadcaster``) so a persistent HA connection never
    eats into the dashboard's per-tier SSE cap.

    Each event is ``{type:"motion", camera_id, node_id, score, timestamp}``;
    a ``": keepalive"`` comment is sent every 25s to hold the connection
    open. Capped at ``INTEGRATION_MAX_SSE_SUBSCRIBERS`` streams per org (429
    past that).
    """
    from app.api.motion import integration_motion_broadcaster

    org_id = user.org_id
    queue = integration_motion_broadcaster.subscribe(
        org_id, INTEGRATION_MAX_SSE_SUBSCRIBERS
    )
    if queue is None:
        raise HTTPException(
            status_code=429,
            detail=(
                f"Too many open integration motion streams for this org "
                f"(cap: {INTEGRATION_MAX_SSE_SUBSCRIBERS}). Close unused "
                f"connections and retry."
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
                    yield ": keepalive\n\n"
        except asyncio.CancelledError:
            pass
        finally:
            integration_motion_broadcaster.unsubscribe(org_id, queue)

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
        },
    )
