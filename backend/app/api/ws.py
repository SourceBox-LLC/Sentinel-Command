"""
WebSocket command channel for CloudNode ↔ Backend communication.

Auth:

    The node sends its API key + node_id as request HEADERS during the
    WS upgrade handshake:

        X-Node-API-Key: nak_<32-byte-hex>
        X-Node-Id: <node_id>

    Pre-v0.1.65 CloudNode clients send these as URL query-string
    parameters instead (`?api_key=…&node_id=…`).  We still accept that
    path for back-compat with older nodes, but log a deprecation
    warning each time — every node we ship from v0.1.65+ uses the
    header path so that path will eventually be retired.

    Header-vs-query matters because URLs end up in many more log
    sinks than headers do (uvicorn access log, Fly platform access
    log, log-shipping pipeline exports, browser referer headers if
    the URL ever escapes the process).  Sentry already scrubs the
    query string (`app/core/sentry.py::_scrub_event`), but defense
    in depth says don't put credentials in URLs at all when you
    have a choice.  Custom WS clients (which CloudNode is — it's
    not a browser) can set arbitrary headers on the upgrade request.

Message format (both directions):
    {"type": "<message_type>", "id": "<optional_correlation_id>", "payload": {...}}

Node → Backend types:
    heartbeat       — periodic camera status update
    command_result  — response to a backend-issued command

Backend → Node types:
    ack             — heartbeat acknowledged
    command         — request the node to do something (snapshot, recording, etc.)
    error           — something went wrong
"""

import asyncio
import hashlib
import logging
import time
import uuid
from collections import deque
from datetime import UTC, datetime
from typing import Optional

from fastapi import APIRouter, Query, WebSocket, WebSocketDisconnect
from sqlalchemy.orm import Session

from app.core.database import SessionLocal
from app.core.versions import check_node_version
from app.models import Camera, CameraNode, MotionEvent

logger = logging.getLogger(__name__)

router = APIRouter()


# ── Per-node message rate limiter ────────────────────────────────────
# WebSocket messages bypass slowapi entirely, so we need our own
# in-process throttle to keep a compromised (or buggy) node from
# hammering the backend.  Heartbeats fire ~every 30s and command
# results are sporadic, so 180 msg/minute leaves ~6× legitimate headroom.
# Dropped messages just return an error response; we don't kill the
# connection, because a transient burst shouldn't cost the node its
# status updates for the next reconnect-backoff window.
WS_MAX_MSGS_PER_MINUTE = 180
WS_RATE_WINDOW_SECONDS = 60.0


class NodeRateLimiter:
    """Sliding-window rate limiter keyed by node_id."""

    def __init__(self, max_per_window: int = WS_MAX_MSGS_PER_MINUTE,
                 window_seconds: float = WS_RATE_WINDOW_SECONDS):
        self._windows: dict[str, deque[float]] = {}
        self._max = max_per_window
        self._window = window_seconds

    def allow(self, node_id: str) -> bool:
        now = time.monotonic()
        window = self._windows.setdefault(node_id, deque())
        # Evict entries older than the window.
        cutoff = now - self._window
        while window and window[0] < cutoff:
            window.popleft()
        if len(window) >= self._max:
            return False
        window.append(now)
        return True

    def forget(self, node_id: str):
        self._windows.pop(node_id, None)


_ws_rate_limiter = NodeRateLimiter()


# ── Per-node CONNECT-rate throttle ───────────────────────────────────
# Separate from the message-rate limiter above because the threat
# model is different.  An attacker who has stolen (or guessed) a
# valid node API key can spam-connect to /ws/node — each successful
# connect calls ``manager.connect()`` which CLOSES the previous
# connection (intentional, to handle stale-reconnect after a node
# crash).  Without throttling, the attacker can hold the legitimate
# node permanently disconnected by reconnecting every few millis.
#
# Also catches the unauthenticated-spam case: each connect attempt
# does a SHA-256 hash + DB lookup before rejecting, so 1000s of
# attempts/sec from a single IP would burn auth CPU.  We throttle
# by node_id (always present per FastAPI Query(...)) which doubles
# as a per-attacker-bucket since the attacker has to pick SOME id
# to send.
#
# 10 connects/minute is well above legitimate traffic — a healthy
# node connects once at startup and reconnects on the order of
# seconds-to-minutes when the link drops.  Burst-of-three on a
# flaky network is fine; ten in 60 seconds is anomalous.
WS_MAX_CONNECTS_PER_MINUTE = 10

_ws_connect_throttle = NodeRateLimiter(
    max_per_window=WS_MAX_CONNECTS_PER_MINUTE,
    window_seconds=WS_RATE_WINDOW_SECONDS,
)


# ── Connection Manager ────────────────────────────────────────────────
# Tracks all active WebSocket connections keyed by node_id.
# Provides methods to send commands to specific nodes and wait for
# responses via correlation IDs.

class ConnectionManager:
    def __init__(self):
        # {node_id: WebSocket}
        self._connections: dict[str, WebSocket] = {}
        # Pending command futures: {correlation_id: (node_id, asyncio.Future)}
        self._pending_commands: dict[str, tuple[str, asyncio.Future]] = {}

    @property
    def connected_nodes(self) -> list[str]:
        return list(self._connections.keys())

    def is_connected(self, node_id: str) -> bool:
        return node_id in self._connections

    async def connect(self, node_id: str, ws: WebSocket):
        # Close any existing connection for this node (stale reconnect)
        old = self._connections.get(node_id)
        if old:
            try:
                await old.close(code=1000, reason="Replaced by new connection")
            except Exception:
                pass
        self._connections[node_id] = ws
        # Use print() so it always appears in fly logs (logger.info is filtered by default)
        print(f"[WS] Node {node_id} connected via WebSocket")

    def disconnect(self, node_id: str, ws: WebSocket | None = None):
        # Identity guard for the reconnect race: when a node reconnects to
        # the SAME process, `connect()` replaces the old socket with the new
        # one, but the OLD socket's receive loop then exits and calls
        # disconnect(node_id). A blind pop(node_id) would evict the NEW
        # (live) connection — leaving a node that keeps heartbeating yet
        # reports `is_connected() == False`, so every command (snapshot /
        # view / recording over WS) fails until it reconnects again. Only
        # tear down if the stored socket is the one actually disconnecting.
        current = self._connections.get(node_id)
        if ws is not None and current is not None and current is not ws:
            # A newer connection already replaced us — leave the registry and
            # the new connection's pending commands untouched.
            return
        self._connections.pop(node_id, None)
        # Cancel pending command futures so callers don't wait until
        # timeout for a node that's already gone.
        stale = [cid for cid, (nid, _) in self._pending_commands.items() if nid == node_id]
        for cid in stale:
            _, future = self._pending_commands.pop(cid)
            if not future.done():
                future.cancel()
        print(f"[WS] Node {node_id} disconnected from WebSocket")

    async def send_command(
        self,
        node_id: str,
        command: str,
        payload: dict | None = None,
        timeout: float = 30.0,
    ) -> dict:
        """
        Send a command to a node and wait for the response.
        Returns the command_result payload or raises TimeoutError/ValueError.
        """
        ws = self._connections.get(node_id)
        if not ws:
            raise ValueError(f"Node {node_id} is not connected")

        correlation_id = str(uuid.uuid4())
        future: asyncio.Future = asyncio.get_running_loop().create_future()
        self._pending_commands[correlation_id] = (node_id, future)

        try:
            await ws.send_json({
                "type": "command",
                "id": correlation_id,
                "command": command,
                "payload": payload or {},
            })
            return await asyncio.wait_for(future, timeout=timeout)
        except TimeoutError:
            raise TimeoutError(f"Command {command} to node {node_id} timed out") from None
        except asyncio.CancelledError:
            raise ValueError(f"Node {node_id} disconnected while awaiting {command}") from None
        finally:
            self._pending_commands.pop(correlation_id, None)

    def resolve_command(self, correlation_id: str, result: dict):
        """Called when a command_result message arrives from a node."""
        entry = self._pending_commands.get(correlation_id)
        if entry:
            _, future = entry
            if not future.done():
                future.set_result(result)


# Singleton — imported by other modules to send commands to nodes.
manager = ConnectionManager()


# ── WebSocket Endpoint ────────────────────────────────────────────────

@router.websocket("/ws/node")
async def node_websocket(
    ws: WebSocket,
    api_key: Optional[str] = Query(None),
    node_id: Optional[str] = Query(None),
):
    """
    Persistent WebSocket channel for a CloudNode.
    Authentication happens during handshake via headers (preferred)
    or — for pre-v0.1.65 clients — query-string parameters.  See the
    module docstring for the rationale.
    """
    # --- Auth credential resolution (headers preferred) ---
    # FastAPI's WebSocket.headers preserves the canonical lower-case
    # form that the ASGI spec normalises to, so we look up the
    # lowercased header names.
    header_api_key = ws.headers.get("x-node-api-key")
    header_node_id = ws.headers.get("x-node-id")

    auth_path: str  # "header" | "query" — only used for the deprecation log below.
    if header_api_key:
        api_key = header_api_key
        auth_path = "header"
    else:
        auth_path = "query"
    if header_node_id:
        node_id = header_node_id

    if not api_key or not node_id:
        # Either path must provide both — reject before we even try
        # the DB.  4001 = the same code we use below for a key that
        # mismatched a real node; intentional, so a client probing
        # for "is this auth-required" can't distinguish missing-cred
        # from wrong-cred.
        logger.warning(
            "[WS] Connect rejected — missing api_key or node_id "
            "(api_key_present=%s, node_id_present=%s)",
            api_key is not None, node_id is not None,
        )
        await ws.close(code=4001, reason="Missing api_key or node_id")
        return

    # --- Connect-rate throttle (BEFORE auth) ---
    # Reject the handshake without burning a DB query if this node_id
    # has already attempted ``WS_MAX_CONNECTS_PER_MINUTE`` connects in
    # the last 60s.  See the throttle's docstring above for the threat
    # model.  Code 1013 (Try Again Later) is the WS-spec equivalent of
    # HTTP 429.
    if not _ws_connect_throttle.allow(node_id):
        logger.warning(
            "[WS] Connect throttle hit for node_id=%s — rejecting handshake",
            node_id,
        )
        await ws.close(code=1013, reason="Too many connection attempts")
        return

    # --- Authenticate ---
    api_key_hash = hashlib.sha256(api_key.encode()).hexdigest()

    db: Session = SessionLocal()
    try:
        node = db.query(CameraNode).filter_by(node_id=node_id).first()
        if not node or node.api_key_hash != api_key_hash:
            logger.warning(
                "[WS] Auth failed for node_id=%s (found=%s)",
                node_id, node is not None,
            )
            await ws.close(code=4001, reason="Invalid node_id or API key")
            return
        org_id = node.org_id
        node_db_id = node.id
    finally:
        db.close()

    # Auth succeeded.  Log the deprecation warning AFTER auth so we
    # don't spam the warning for invalid-key probes that never had
    # legitimate credentials — only authenticated nodes still on the
    # old path generate this signal.
    if auth_path == "query":
        logger.warning(
            "[WS] Node %s authenticated via deprecated query-string "
            "api_key — upgrade CloudNode to v0.1.65+ to move the "
            "credential into request headers (X-Node-API-Key / "
            "X-Node-Id).  Query-string auth is still accepted but "
            "logs the key in uvicorn / Fly access pipelines.",
            node_id,
        )

    await ws.accept()
    await manager.connect(node_id, ws)

    try:
        while True:
            data = await ws.receive_json()

            # Per-node message rate limit — see NodeRateLimiter above.
            # Over-limit messages get an error response but we keep the
            # socket open; disconnecting would reset the node's status
            # tracking and cascade into bigger UX problems than a
            # temporarily misbehaving node.
            if not _ws_rate_limiter.allow(node_id):
                logger.warning(
                    "[WS] Rate limit exceeded for node %s — dropping message", node_id,
                )
                try:
                    await ws.send_json({
                        "type": "error",
                        "id": data.get("id"),
                        "payload": {"detail": "Rate limit exceeded"},
                    })
                except Exception:
                    pass
                continue

            msg_type = data.get("type")

            if msg_type == "heartbeat":
                hb_result = await _handle_heartbeat(node_id, node_db_id, org_id, data.get("payload", {}))
                # Pass version-compat hints back through the ack so CloudNode
                # can log "update available" or "you're below the supported
                # floor" without needing a separate channel.  Keys are
                # omitted when there's nothing to say (no update, supported)
                # so old nodes that don't parse the new fields stay happy.
                ack_payload = {
                    "timestamp": datetime.now(tz=UTC).replace(tzinfo=None).isoformat(),
                }
                if hb_result and hb_result.get("update_available"):
                    ack_payload["update_available"] = hb_result["update_available"]
                if hb_result and hb_result.get("unsupported"):
                    ack_payload["unsupported"] = True
                await ws.send_json({
                    "type": "ack",
                    "id": data.get("id"),
                    "payload": ack_payload,
                })

            elif msg_type == "command_result":
                correlation_id = data.get("id")
                if correlation_id:
                    manager.resolve_command(correlation_id, data.get("payload", {}))

            elif msg_type == "event":
                command = data.get("command")
                payload = data.get("payload", {})
                if command == "motion_detected":
                    await _handle_motion_event(node_id, org_id, payload)
                else:
                    logger.debug("Unhandled event command from node %s: %s", node_id, command)

            else:
                logger.warning("Unknown WS message type from node %s: %s", node_id, msg_type)
                await ws.send_json({
                    "type": "error",
                    "id": data.get("id"),
                    "payload": {"detail": f"Unknown message type: {msg_type}"},
                })

    except WebSocketDisconnect:
        pass
    except Exception as e:
        logger.error("WebSocket error for node %s: %s", node_id, e)
    finally:
        # Pass THIS socket so a stale old connection's cleanup can't evict a
        # newer one that already replaced it (see ConnectionManager.disconnect).
        manager.disconnect(node_id, ws)
        _ws_rate_limiter.forget(node_id)


# ── Heartbeat Handler ─────────────────────────────────────────────────

async def _handle_heartbeat(node_id: str, node_db_id: int, org_id: str, payload: dict) -> dict:
    """Process a heartbeat message — same logic as the HTTP endpoint.

    Also detects node/camera online↔offline transitions by comparing the
    previous ``status`` column against the incoming value; any change is
    emitted as a notification AFTER the DB commit so the inbox never
    shows a transition that later got rolled back.

    Returns a dict the caller can mix into the ack payload, currently:

        {"update_available": "X.Y.Z" | None, "unsupported": bool}

    so the node sees both the "newer release exists" hint and the
    "you're below the floor" warning over the same channel.  Unlike the
    HTTP endpoint we don't drop the WS connection on too-old — disconnecting
    cascades into reconnect storms — but the dashboard already flags the
    bad version and the next register call will get HTTP 426.
    """
    db: Session = SessionLocal()
    response: dict = {"update_available": None, "unsupported": False}

    # Accumulate transitions during the update pass so we can emit them
    # post-commit.  Tuple shape: (kind, entity_id, display_name, new_status, node_id|None)
    transitions: list[tuple[str, str, str, str, Optional[str]]] = []

    try:
        node = db.query(CameraNode).filter_by(node_id=node_id).first()
        if not node:
            db.close()
            return response

        reported_version = payload.get("node_version")
        version_check = check_node_version(reported_version)
        node.node_version = version_check["parsed"] if reported_version else None
        node.version_checked_at = datetime.now(tz=UTC).replace(tzinfo=None)
        response["update_available"] = version_check["update_available"]
        response["unsupported"] = not version_check["supported"]

        prev_node_status = node.status
        node.status = "online"
        node.last_seen = datetime.now(tz=UTC).replace(tzinfo=None)

        if prev_node_status != "online":
            transitions.append(("node", node.node_id, node.name or node.node_id, "online", None))

        local_ip = payload.get("local_ip")
        if local_ip:
            node.local_ip = local_ip

        cameras = payload.get("cameras", [])
        if cameras:
            camera_ids = [c["camera_id"] for c in cameras if "camera_id" in c]
            if camera_ids:
                cams = db.query(Camera).filter(
                    Camera.camera_id.in_(camera_ids),
                    Camera.node_id == node_db_id,
                ).all()
                cam_map = {c.camera_id: c for c in cams}
                now = datetime.now(tz=UTC).replace(tzinfo=None)
                for cam_data in cameras:
                    cam = cam_map.get(cam_data.get("camera_id"))
                    if cam:
                        prev_cam_status = cam.status
                        new_cam_status = cam_data.get("status", "online")
                        cam.status = new_cam_status
                        cam.last_seen = now
                        # Record (or clear) the pipeline failure reason.
                        # Healthy states wipe the field so stale errors
                        # don't linger once the supervisor recovers.
                        if new_cam_status in ("restarting", "failed", "error"):
                            cam.last_error = cam_data.get("last_error")
                        else:
                            cam.last_error = None
                        if (
                            prev_cam_status != new_cam_status
                            and new_cam_status in ("online", "offline")
                        ):
                            display = cam.name or cam.camera_id
                            transitions.append(
                                ("camera", cam.camera_id, display, new_cam_status, node_id)
                            )

        db.commit()
    except Exception as e:
        logger.error("Heartbeat DB error for node %s: %s", node_id, e)
        db.rollback()
        db.close()
        return response
    finally:
        # NOTE: db is intentionally NOT closed here — the post-commit
        # notification writes below reuse this same session. It's closed on
        # every exit path instead: the two early returns above close
        # explicitly, and the success path closes after the emit block.
        pass

    # Emit transitions post-commit — any failure here must not fail the
    # heartbeat loop.  Reuse the same session for efficiency.
    if transitions:
        try:
            from app.api.notifications import (
                emit_camera_transition,
                emit_node_transition,
            )
            for kind, eid, name, new_status, cam_node_id in transitions:
                if kind == "node":
                    emit_node_transition(
                        db,
                        node_id=eid,
                        org_id=org_id,
                        display_name=name,
                        new_status=new_status,
                    )
                elif kind == "camera":
                    emit_camera_transition(
                        db,
                        camera_id=eid,
                        org_id=org_id,
                        display_name=name,
                        new_status=new_status,
                        node_id=cam_node_id,
                    )
        except Exception:
            logger.exception("[Heartbeat] Failed to emit transition notifications")

    db.close()
    return response


# ── Motion Event Handler ─────────────────────────────────────────────

async def _handle_motion_event(node_id: str, org_id: str, payload: dict):
    """Persist a motion detection event reported by a CloudNode."""
    camera_id = payload.get("camera_id")
    score = payload.get("score")
    segment_seq = payload.get("segment_seq")
    event_ts = payload.get("timestamp")  # ISO 8601 from node

    if not camera_id or score is None:
        logger.warning("Motion event from node %s missing camera_id or score", node_id)
        return

    # Normalise and clamp score to 0-100
    try:
        score_int = max(0, min(100, int(score)))
    except (ValueError, TypeError):
        logger.warning("Motion event from node %s has non-numeric score: %r", node_id, score)
        return

    ts = None
    if event_ts:
        try:
            ts = datetime.fromisoformat(event_ts).replace(tzinfo=None)
        except (ValueError, TypeError):
            logger.debug("Unparseable timestamp from node %s, using server time", node_id)
    if ts is None:
        ts = datetime.now(tz=UTC).replace(tzinfo=None)

    try:
        seq = int(segment_seq) if segment_seq is not None else None
    except (ValueError, TypeError):
        seq = None

    db: Session = SessionLocal()
    try:
        # Verify the camera_id in the payload actually belongs to the
        # authenticated node in the authenticated org.  Without this
        # check a compromised node could spam its own org's inbox with
        # motion events referencing camera IDs it doesn't own (or that
        # don't exist).  Cross-tenant is still blocked because org_id
        # comes from the auth'd session, not the payload.
        from app.models.models import Camera as _Camera
        from app.models.models import CameraNode as _CameraNode

        owning_node = (
            db.query(_CameraNode)
            .filter_by(node_id=node_id, org_id=org_id)
            .first()
        )
        if not owning_node:
            logger.warning(
                "Motion event rejected: node %s not found in org %s", node_id, org_id,
            )
            return
        cam_row = (
            db.query(_Camera)
            .filter_by(camera_id=camera_id, node_id=owning_node.id, org_id=org_id)
            .first()
        )
        if not cam_row:
            logger.warning(
                "Motion event rejected: camera %s not owned by node %s (org %s)",
                camera_id, node_id, org_id,
            )
            return

        event = MotionEvent(
            org_id=org_id,
            camera_id=camera_id,
            node_id=node_id,
            score=score_int,
            segment_seq=seq,
            timestamp=ts,
        )
        db.add(event)
        db.commit()
        logger.info(
            "Motion event: camera=%s score=%d%% node=%s",
            camera_id, score_int, node_id,
        )

        # Broadcast to the motion SSE streams so live dashboards show
        # toasts immediately; the inbox notification below handles the
        # durable history.
        from app.api.motion import (
            integration_motion_broadcaster,
            motion_broadcaster,
        )
        motion_payload = {
            "type": "motion",
            "camera_id": camera_id,
            "node_id": node_id,
            "score": score_int,
            "timestamp": ts.isoformat(),
        }
        motion_broadcaster.notify(org_id, motion_payload)
        # Mirror to the Home Assistant integration SSE (separate pool so HA
        # doesn't consume dashboard subscriber slots — see motion.py).
        integration_motion_broadcaster.notify(org_id, motion_payload)


        # Also emit an inbox notification so the user can see motion
        # history in the bell panel.  Resolve the camera name for a
        # friendlier title — fall back to the camera_id if not found.
        try:
            from app.api.notifications import create_notification
            from app.models.models import Camera

            cam = (
                db.query(Camera)
                .filter_by(camera_id=camera_id, org_id=org_id)
                .first()
            )
            display_name = cam.name if cam and cam.name else camera_id

            create_notification(
                org_id=org_id,
                kind="motion",
                title=f"Motion on {display_name}",
                body=f"Scene change detected at {score_int}% intensity.",
                severity="info",
                audience="all",
                link=f"/dashboard?camera={camera_id}",
                camera_id=camera_id,
                node_id=node_id,
                meta={
                    "score": score_int,
                    "segment_seq": seq,
                    "event_timestamp": ts.isoformat(),
                },
                db=db,
            )
        except Exception:
            # Notification creation must never fail the motion event path.
            logger.exception("[Motion] Failed to create inbox notification")
    except Exception as e:
        logger.error("Failed to save motion event: %s", e)
        db.rollback()
    finally:
        db.close()
