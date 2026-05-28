"""
Sentinel MCP Server — gives AI tools (Claude Code, etc.) direct access
to an organization's cameras, nodes, streams, and settings.

Mounted inside the main FastAPI app at /mcp.
Auth: Bearer token using org-scoped MCP API keys.
Rate limited per API key based on org plan.
"""

import asyncio
import base64
import collections
import contextvars
import functools
import hashlib
import hmac
import logging
import threading
import time
import uuid as uuid_mod
from datetime import UTC, datetime, timedelta
from typing import Annotated

from fastmcp import FastMCP
from fastmcp.exceptions import ToolError
from fastmcp.server.dependencies import get_http_headers
from fastmcp.server.middleware.middleware import Middleware
from fastmcp.utilities.types import Image
from pydantic import Field
from sqlalchemy.orm import Session

from app.api.hls import snapshot_recent_segment_bytes
from app.core.config import settings
from app.core.database import SessionLocal
from app.mcp.activity import McpEvent, tracker
from app.models.models import (
    INCIDENT_SEVERITIES,
    INCIDENT_STATUSES,
    Camera,
    CameraGroup,
    CameraNode,
    Incident,
    IncidentEvidence,
    McpApiKey,
    StreamAccessLog,
)

logger = logging.getLogger(__name__)

# Context variables — set by _auth(), read by the tracking decorator
_ctx_org_id = contextvars.ContextVar("mcp_org_id", default="")
_ctx_key_name = contextvars.ContextVar("mcp_key_name", default="")

# ---------------------------------------------------------------------------
# Per-key tool scoping — canonical classification of every MCP tool.
# A key's scope_mode + scope_tools (on McpApiKey) is evaluated against these
# sets by ScopeMiddleware below to gate discovery and invocation.
# Keep in sync with the @mcp.tool() registrations further down.
# ---------------------------------------------------------------------------

MCP_READ_TOOLS: frozenset[str] = frozenset({
    # Cameras
    "list_cameras",
    "get_camera",
    "get_stream_url",
    "view_camera",
    "watch_camera",
    "list_camera_groups",
    # Nodes
    "list_nodes",
    "get_node",
    # System / recording
    "get_camera_recording_policy",
    "get_stream_logs",
    "get_stream_stats",
    "get_system_status",
    # Incidents (read side)
    "list_incidents",
    "get_incident",
    "get_incident_snapshot",
    "get_incident_clip",
})

MCP_WRITE_TOOLS: frozenset[str] = frozenset({
    "create_incident",
    "add_observation",
    "attach_snapshot",
    "attach_clip",
    "update_incident",
    "finalize_incident",
    "set_camera_recording_policy",
})

MCP_ALL_TOOLS: frozenset[str] = MCP_READ_TOOLS | MCP_WRITE_TOOLS


def compute_allowed_tools(scope_mode: str | None, scope_tools: list[str] | None) -> frozenset[str]:
    """Resolve a key's scope config into the concrete allowed-tool set.

    - ``"all"`` (or ``None``) → every tool.
    - ``"readonly"`` → only tools in ``MCP_READ_TOOLS``.
    - ``"custom"``  → intersection of ``scope_tools`` with all known tools
      (unknown names are silently dropped so a disallowed tool can't be
      enabled by typo or by adding a new WRITE tool server-side).
    """
    mode = (scope_mode or "all").lower()
    if mode == "readonly":
        return MCP_READ_TOOLS
    if mode == "custom":
        if not scope_tools:
            return frozenset()
        return frozenset(scope_tools) & MCP_ALL_TOOLS
    # "all" or unknown → full access
    return MCP_ALL_TOOLS

# ---------------------------------------------------------------------------
# Per-key rate limiter — sliding window (calls per minute)
# ---------------------------------------------------------------------------

# Plan-based rate limits per API key:
#   - minute_limit: protects against short-burst spam (e.g. runaway retry loops)
#   - daily_limit:  protects against runaway automations overnight (e.g. an
#                   agent stuck in a loop burning through your Clerk API spend
#                   and your DB throughput for 8 hours while you sleep)
RATE_LIMITS = {
    "pro":      {"minute": 30,  "daily": 5_000},
    "pro_plus": {"minute": 120, "daily": 30_000},
}
DEFAULT_RATE_LIMIT = None  # Block unrecognized plans (MCP requires Pro+)


class _RateLimiter:
    """Thread-safe sliding-window rate limiter keyed by API key hash.

    Tracks two windows in parallel per key: a 60-second minute window and a
    24-hour daily window. A request is only allowed when both windows have
    headroom; the failure message tells the caller which window they tripped.
    """

    def __init__(self):
        # {key_hash: deque of timestamps} — two separate maps so the purges
        # stay cheap (the minute deque stays small, the daily deque is bigger
        # but we only touch it on a request, not in a background sweep).
        self._minute: dict[str, collections.deque] = {}
        self._daily: dict[str, collections.deque] = {}
        self._lock = threading.Lock()

    def check(
        self,
        key_hash: str,
        minute_limit: int,
        daily_limit: int,
    ) -> tuple[bool, int, str]:
        """Check if a request is allowed.

        Returns ``(allowed, remaining_minute, breach_reason)``. ``breach_reason``
        is ``""`` on success, ``"minute"`` when the per-minute cap tripped,
        or ``"daily"`` when the 24h cap tripped — the caller uses this to
        craft an accurate error message.
        """
        now = time.time()
        minute_cutoff = now - 60.0
        daily_cutoff = now - 86_400.0

        with self._lock:
            minute_dq = self._minute.setdefault(key_hash, collections.deque())
            daily_dq = self._daily.setdefault(key_hash, collections.deque())

            while minute_dq and minute_dq[0] < minute_cutoff:
                minute_dq.popleft()
            while daily_dq and daily_dq[0] < daily_cutoff:
                daily_dq.popleft()

            # Check the tightest window first so the caller gets the most
            # actionable hint (e.g. "you're spamming" vs "you've been looping
            # for hours").
            if len(minute_dq) >= minute_limit:
                return False, 0, "minute"
            if len(daily_dq) >= daily_limit:
                return False, 0, "daily"

            minute_dq.append(now)
            daily_dq.append(now)
            return True, minute_limit - len(minute_dq), ""


_rate_limiter = _RateLimiter()

# ---------------------------------------------------------------------------
# MCP Server
# ---------------------------------------------------------------------------

mcp = FastMCP(
    "Sentinel by SourceBox",
    instructions=(
        "You are connected to a Sentinel Command Center organization. "
        "You can SEE what cameras see via view_camera (returns a live JPEG "
        "snapshot), list cameras, check node status, get stream URLs, manage "
        "recording settings, and view audit logs. All operations are scoped "
        "to the authenticated organization."
    ),
)

# ---------------------------------------------------------------------------
# ScopeMiddleware — per-key tool scoping
#
# Resolves the Bearer token to its McpApiKey row and uses compute_allowed_tools
# to determine which tools the key may see/invoke. Applied to the FastMCP
# instance via add_middleware below so it runs before tools/list and tools/call.
#
# We re-do the lookup on every middleware event (no caching) so scope edits in
# the dashboard propagate immediately. The extra DB round-trip is cheap; the
# tool itself will call _auth() again and share the same key_hash, which also
# bumps last_used_at and enforces the rate limit.
# ---------------------------------------------------------------------------

class ScopeMiddleware(Middleware):
    """Filter tools/list and gate tools/call by the caller's key scope."""

    def _lookup_allowed(self) -> frozenset[str] | None:
        """Return the allowed-tool set for the bearer-token caller.

        Returns ``None`` when there's no usable token or the key doesn't exist —
        in those cases we defer to the tool's own ``_auth()`` to produce the
        right error. Never raising from the middleware avoids double-errors.
        """
        try:
            headers = get_http_headers(include={"authorization"})
        except Exception:
            return None
        auth = (headers or {}).get("authorization", "")
        if not auth.lower().startswith("bearer "):
            return None
        raw_key = auth.split(" ", 1)[1].strip()
        if not raw_key:
            return None
        key_hash = hashlib.sha256(raw_key.encode()).hexdigest()

        db = SessionLocal()
        try:
            mcp_key = (
                db.query(McpApiKey)
                .filter_by(key_hash=key_hash, revoked=False)
                .first()
            )
            if not mcp_key:
                return None
            return compute_allowed_tools(mcp_key.scope_mode, mcp_key.get_scope_tools())
        except Exception:
            logger.exception("ScopeMiddleware: key lookup failed")
            return None
        finally:
            db.close()

    async def on_list_tools(self, context, call_next):
        tools = await call_next(context)
        allowed = self._lookup_allowed()
        if allowed is None:
            return tools
        return [t for t in tools if t.name in allowed]

    async def on_call_tool(self, context, call_next):
        name = getattr(context.message, "name", None)
        if name:
            allowed = self._lookup_allowed()
            if allowed is not None and name not in allowed:
                raise ToolError(
                    f"Tool '{name}' is not enabled for this API key. "
                    "Update the key's scope in the Sentinel dashboard or use a different key."
                )
        return await call_next(context)


mcp.add_middleware(ScopeMiddleware())


# ---------------------------------------------------------------------------
# Auth helper — resolve Bearer token to org_id
# ---------------------------------------------------------------------------

def _resolve_org(headers: dict | None) -> tuple[str, Session]:
    """Validate the Bearer token, enforce rate limit, return (org_id, db_session).

    Two auth paths:
      1. Per-org MCP key (osc_*) — bearer matches an McpApiKey row;
         org_id comes from that row.
      2. Multi-tenant agent key — bearer matches the
         ``SENTINEL_AGENT_MCP_KEY`` env var; org_id comes from the
         ``X-Agent-Org-Override`` header.  Used by the SourceBox
         Sentinel agent to make tool calls on behalf of any org
         it's processing a pending run for.
    """
    if not headers:
        raise ToolError("Unauthorized: no headers present")

    auth = headers.get("authorization", "")
    if not auth.lower().startswith("bearer "):
        raise ToolError("Unauthorized: missing Bearer token")

    raw_key = auth.split(" ", 1)[1].strip()
    if not raw_key:
        raise ToolError("Unauthorized: empty Bearer token")

    # ── Path 2: agent multi-tenant key ──────────────────────────────
    # Constant-time compare against the configured agent key so a
    # length-leak on `==` doesn't reveal anything about the secret.
    # Empty agent key (unset env var) hard-rejects every attempt
    # because hmac.compare_digest("", anything) is False.
    agent_key = settings.SENTINEL_AGENT_MCP_KEY
    if agent_key and hmac.compare_digest(raw_key, agent_key):
        return _resolve_via_agent_key(headers, agent_key)

    # ── Path 1: per-org osc_* key (existing behaviour) ──────────────
    key_hash = hashlib.sha256(raw_key.encode()).hexdigest()

    db = SessionLocal()
    try:
        mcp_key = (
            db.query(McpApiKey)
            .filter_by(key_hash=key_hash, revoked=False)
            .first()
        )
        if not mcp_key:
            db.close()
            raise ToolError("Unauthorized: invalid or revoked API key")

        # Stamp the activity-tracker context AS SOON AS the org is
        # known.  If a later check (plan, rate-limit) raises, the
        # @tracked decorator's exception path still logs the right
        # org_id + key_name in McpActivityLog — without this, failed
        # auth events end up with org_id="" and forensics suffers.
        _ctx_org_id.set(mcp_key.org_id)
        _ctx_key_name.set(mcp_key.name)

        # Look up org plan and enforce access + rate limit. Uses the
        # resolver so orgs whose subscription webhook never landed
        # still get checked against the live Clerk subscription state.
        from app.core.plans import resolve_org_plan
        plan = resolve_org_plan(db, mcp_key.org_id)
        limits = RATE_LIMITS.get(plan)
        if limits is None:
            db.close()
            raise ToolError("MCP requires a Pro or Pro Plus plan. Upgrade at /pricing.")
        allowed, _remaining, breach = _rate_limiter.check(
            key_hash,
            minute_limit=limits["minute"],
            daily_limit=limits["daily"],
        )
        if not allowed:
            db.close()
            plan_name = "Pro Plus" if plan == "pro_plus" else plan.title()
            if breach == "minute":
                raise ToolError(
                    f"Rate limit exceeded: {limits['minute']} calls/min allowed "
                    f"on the {plan_name} plan. Try again shortly."
                )
            else:
                # Daily cap — almost always means a runaway automation loop;
                # tell the caller explicitly so they know to check their agent
                # rather than just retry.
                raise ToolError(
                    f"Daily cap reached: {limits['daily']} calls/24h on the "
                    f"{plan_name} plan. This usually means an agent is stuck "
                    f"in a loop. The cap resets 24h after the first call. "
                    f"Upgrade your plan for a higher ceiling."
                )

        # Touch last_used_at
        mcp_key.last_used_at = datetime.now(tz=UTC).replace(tzinfo=None)
        db.commit()

        return mcp_key.org_id, db
    except ToolError:
        raise
    except Exception:
        db.close()
        raise ToolError("Authentication error") from None


def _resolve_via_agent_key(headers: dict, _agent_key: str) -> tuple[str, Session]:
    """Auth path for the multi-tenant Sentinel agent.

    The bearer token has already been verified against
    ``SENTINEL_AGENT_MCP_KEY`` by the caller.  Now:

    1. Read the override org_id from ``X-Agent-Org-Override``.
    2. Verify the override org actually has a Sentinel-eligible plan
       (Pro or Pro Plus, per ``SENTINEL_PLANS``) AND has Sentinel
       enabled.  Otherwise the agent is acting on behalf of an org
       that shouldn't be served — likely a stale pending run, or
       impersonation if the secret leaked.
    3. Apply rate limits scoped to the override org so a runaway
       agent loop on org X can't burn org Y's tool budget.
    4. Audit-log via the standard tracker context with key_name
       set to "<sentinel-agent>" so the audit trail shows the
       tool call came from the agent (and which org it was for).
    """
    override_org = headers.get("x-agent-org-override", "").strip()
    if not override_org:
        raise ToolError(
            "Unauthorized: agent key requires X-Agent-Org-Override header"
        )

    # Stamp the activity-tracker context AS SOON AS the override org
    # is known (and the bearer has already matched the agent key, so
    # `<sentinel-agent>` is the right key_name regardless of which
    # downstream check passes or fails).  Lets failed-auth events
    # for plan / config / rate-limit gates land in McpActivityLog
    # with the right org_id rather than "".
    _ctx_org_id.set(override_org)
    _ctx_key_name.set("<sentinel-agent>")

    db = SessionLocal()
    try:
        from app.core.plans import effective_plan_for_caps
        from app.core.sentinel_dispatch import SENTINEL_PLANS
        from app.models.models import SentinelConfig

        # Sentinel is paid-only (Pro or Pro Plus); the dispatch gate,
        # the route gates, and this resolver all check the same set.
        # Use effective_plan_for_caps so a long-past-due org gets
        # bumped to free_org and rejected — matches the resolver the
        # rest of the Sentinel surface uses.
        plan = effective_plan_for_caps(db, override_org)
        if plan not in SENTINEL_PLANS:
            db.close()
            raise ToolError(
                f"Agent override target org is not on a Sentinel-eligible "
                f"plan (plan={plan!r})"
            )
        # Apply the org's actual plan tier's MCP rate limits — Pro orgs
        # get Pro's per-minute / per-day caps, Pro Plus gets Pro Plus's.
        # Without this, a Pro-tier org would (paradoxically) get Pro
        # Plus's higher MCP budget when the agent acts on its behalf.
        limits = RATE_LIMITS[plan]

        # Defence in depth — the dispatcher already gates on
        # sentinel_config.enabled before creating a pending run, but
        # an operator could disable Sentinel between dispatch and
        # the agent picking it up.  Don't run tool calls for an org
        # that's currently disabled.
        cfg = db.query(SentinelConfig).filter_by(org_id=override_org).first()
        if cfg is None or not cfg.enabled:
            db.close()
            raise ToolError("Sentinel disabled for this org")

        # Per-org bucket so agent traffic for org X doesn't throttle
        # org Y.  Distinct from per-osc-key buckets so direct dashboard
        # MCP usage isn't accidentally affected by agent activity on
        # the same org either.
        rate_bucket = f"sentinel-agent:{override_org}"
        allowed, _remaining, breach = _rate_limiter.check(
            rate_bucket,
            minute_limit=limits["minute"],
            daily_limit=limits["daily"],
        )
        if not allowed:
            db.close()
            if breach == "minute":
                raise ToolError(
                    "Sentinel agent rate limit: too many tool calls in one "
                    "minute for this org. Tune the per-camera cooldown or "
                    "narrow the scope."
                )
            raise ToolError(
                "Sentinel agent daily cap reached for this org — check the "
                "agent's run log for a stuck loop."
            )

        return override_org, db
    except ToolError:
        raise
    except Exception:
        db.close()
        raise ToolError("Authentication error") from None


def _auth():
    """Shortcut: get headers, resolve org, return (org_id, db)."""
    headers = get_http_headers(include={"authorization", "x-agent-org-override"})
    return _resolve_org(headers)


# ---------------------------------------------------------------------------
# Activity-tracking decorator — wraps every MCP tool to log calls
# ---------------------------------------------------------------------------

def _summarize_args(kwargs: dict) -> str:
    """Create a short summary of tool arguments for the activity log."""
    parts = []
    for k, v in kwargs.items():
        if v is not None:
            sv = str(v)
            if len(sv) > 30:
                sv = sv[:27] + "..."
            parts.append(f"{k}={sv}")
    return ", ".join(parts) if parts else ""


def tracked(func):
    """Decorator that logs MCP tool calls to the activity tracker."""
    tool_name = func.__name__

    if asyncio.iscoroutinefunction(func):
        @functools.wraps(func)
        async def wrapper(*args, **kwargs):
            event_id = str(uuid_mod.uuid4())[:8]
            start = time.time()
            args_summary = _summarize_args(kwargs)
            try:
                result = await func(*args, **kwargs)
                org_id = _ctx_org_id.get("")
                key_name = _ctx_key_name.get("")
                tracker.log_event(McpEvent(
                    id=event_id,
                    timestamp=start,
                    tool_name=tool_name,
                    org_id=org_id,
                    key_name=key_name,
                    status="completed",
                    duration_ms=round((time.time() - start) * 1000),
                    args_summary=args_summary or None,
                ))
                return result
            except Exception as e:
                org_id = _ctx_org_id.get("")
                key_name = _ctx_key_name.get("")
                tracker.log_event(McpEvent(
                    id=event_id,
                    timestamp=start,
                    tool_name=tool_name,
                    org_id=org_id,
                    key_name=key_name,
                    status="error",
                    duration_ms=round((time.time() - start) * 1000),
                    error=str(e)[:200],
                    args_summary=args_summary or None,
                ))
                raise
        return wrapper
    else:
        @functools.wraps(func)
        def wrapper(*args, **kwargs):
            event_id = str(uuid_mod.uuid4())[:8]
            start = time.time()
            args_summary = _summarize_args(kwargs)
            try:
                result = func(*args, **kwargs)
                org_id = _ctx_org_id.get("")
                key_name = _ctx_key_name.get("")
                tracker.log_event(McpEvent(
                    id=event_id,
                    timestamp=start,
                    tool_name=tool_name,
                    org_id=org_id,
                    key_name=key_name,
                    status="completed",
                    duration_ms=round((time.time() - start) * 1000),
                    args_summary=args_summary or None,
                ))
                return result
            except Exception as e:
                org_id = _ctx_org_id.get("")
                key_name = _ctx_key_name.get("")
                tracker.log_event(McpEvent(
                    id=event_id,
                    timestamp=start,
                    tool_name=tool_name,
                    org_id=org_id,
                    key_name=key_name,
                    status="error",
                    duration_ms=round((time.time() - start) * 1000),
                    error=str(e)[:200],
                    args_summary=args_summary or None,
                ))
                raise
        return wrapper


# ---------------------------------------------------------------------------
# Camera Tools
# ---------------------------------------------------------------------------

@mcp.tool(
    name="list_cameras",
    description=(
        "List every camera in the organization with status, codec info, and "
        "group assignment. Start here when you don't yet know what cameras "
        "exist — most other camera tools take a camera_id from this output."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def list_cameras() -> list[dict]:
    org_id, db = _auth()
    try:
        cameras = db.query(Camera).filter_by(org_id=org_id).all()
        return [c.to_dict() for c in cameras]
    finally:
        db.close()


@mcp.tool(
    name="get_camera",
    description=(
        "Get full metadata for one camera by camera_id (status, codec, node, "
        "group, last seen). Use after list_cameras to inspect one closely. "
        "Returns text only — for the actual image, use view_camera."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def get_camera(
    camera_id: Annotated[str, "The camera_id string (e.g. 'node1-video0')"],
) -> dict:
    org_id, db = _auth()
    try:
        cam = (
            db.query(Camera)
            .filter_by(org_id=org_id, camera_id=camera_id)
            .first()
        )
        if not cam:
            raise ToolError(f"Camera '{camera_id}' not found")
        return cam.to_dict()
    finally:
        db.close()


@mcp.tool(
    name="get_stream_url",
    description=(
        "Return the authenticated HLS playlist URL for a camera. This is a URL "
        "a human or HLS player can open — YOU cannot watch video from it. Use "
        "only when you need to hand a stream URL back to the user. To see a "
        "frame yourself, use view_camera (single frame) or watch_camera "
        "(multi-frame burst)."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def get_stream_url(
    camera_id: Annotated[str, "The camera_id to get the stream URL for"],
) -> dict:
    org_id, db = _auth()
    try:
        cam = (
            db.query(Camera)
            .filter_by(org_id=org_id, camera_id=camera_id)
            .first()
        )
        if not cam:
            raise ToolError(f"Camera '{camera_id}' not found")

        return {
            "camera_id": camera_id,
            "stream_url": f"/api/cameras/{camera_id}/stream.m3u8",
            "format": "HLS",
            "note": "Requires Bearer auth. Open in the dashboard or an HLS player with auth headers.",
        }
    finally:
        db.close()


# ---------------------------------------------------------------------------
# Visual Access Tools
# ---------------------------------------------------------------------------

@mcp.tool(
    name="view_camera",
    description=(
        "See what a camera sees RIGHT NOW — returns a single live JPEG you "
        "can actually look at. Use for a one-shot situational check ('is "
        "anyone in the workshop?'). For motion or change over time, use "
        "watch_camera instead. To preserve what you saw as evidence on an "
        "incident, follow up with attach_snapshot. The camera's node must be "
        "online."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
async def view_camera(
    camera_id: Annotated[str, "The camera_id to view (e.g. 'node1-video0')"],
) -> Image:
    org_id, db = _auth()
    try:
        cam = (
            db.query(Camera)
            .filter_by(org_id=org_id, camera_id=camera_id)
            .first()
        )
        if not cam:
            raise ToolError(f"Camera '{camera_id}' not found")

        node = db.query(CameraNode).filter_by(id=cam.node_id).first()
        if not node:
            raise ToolError(f"Camera '{camera_id}' has no assigned node")

        node_id = node.node_id
    finally:
        db.close()

    # Send take_snapshot command to CloudNode via WebSocket
    from app.api.ws import manager

    if not manager.is_connected(node_id):
        raise ToolError(f"Node '{node_id}' is offline — cannot capture snapshot")

    try:
        result = await manager.send_command(
            node_id, "take_snapshot", {"camera_id": camera_id}, timeout=15.0,
        )
    except TimeoutError:
        raise ToolError("Snapshot timed out — camera node did not respond in time") from None
    except ValueError as e:
        raise ToolError(str(e)) from e

    image_b64 = _extract_snapshot_image_b64(result, camera_id)
    return Image(data=base64.b64decode(image_b64), format="jpeg")


@mcp.tool(
    name="watch_camera",
    description=(
        "Take a burst of snapshots from one camera (count × interval_seconds "
        "wide). Use when a single view_camera frame isn't enough — to confirm "
        "whether a subject is moving, whether motion is sustained or fleeting, "
        "or whether something is returning to a scene. Each frame is a JPEG "
        "you can look at. The total window is short by design (max 10 frames "
        "× 30s); for longer evidence retention on an incident, use attach_clip."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
async def watch_camera(
    camera_id: Annotated[str, "The camera_id to watch"],
    count: Annotated[int, Field(description="Number of snapshots to take", ge=2, le=10)] = 3,
    interval_seconds: Annotated[int, Field(description="Seconds between snapshots", ge=1, le=30)] = 5,
):
    org_id, db = _auth()
    try:
        cam = (
            db.query(Camera)
            .filter_by(org_id=org_id, camera_id=camera_id)
            .first()
        )
        if not cam:
            raise ToolError(f"Camera '{camera_id}' not found")

        node = db.query(CameraNode).filter_by(id=cam.node_id).first()
        if not node:
            raise ToolError(f"Camera '{camera_id}' has no assigned node")

        node_id = node.node_id
    finally:
        db.close()

    from app.api.ws import manager

    if not manager.is_connected(node_id):
        raise ToolError(f"Node '{node_id}' is offline — cannot capture snapshots")

    results = []
    for i in range(count):
        if i > 0:
            await asyncio.sleep(interval_seconds)
        try:
            result = await manager.send_command(
                node_id, "take_snapshot", {"camera_id": camera_id}, timeout=15.0,
            )
            image_b64 = result.get("data", {}).get("image_b64") or result.get("image_b64")
            if image_b64:
                results.append(Image(data=base64.b64decode(image_b64), format="jpeg"))
            else:
                results.append(f"[Frame {i+1}] No image data returned")
        except (TimeoutError, ValueError) as e:
            results.append(f"[Frame {i+1}] Failed: {e}")

    if not any(isinstance(r, Image) for r in results):
        raise ToolError("Failed to capture any snapshots — check node status")

    return results


# ---------------------------------------------------------------------------
# Camera Group Tools
# ---------------------------------------------------------------------------

@mcp.tool(
    name="list_camera_groups",
    description=(
        "List the camera groups defined in the dashboard. A group is a "
        "user-defined zone (e.g. 'Front yard', 'Workshop') that bundles "
        "cameras together. Use when the user names a place and you need to "
        "find which cameras live there."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def list_camera_groups() -> list[dict]:
    org_id, db = _auth()
    try:
        groups = db.query(CameraGroup).filter_by(org_id=org_id).all()
        return [g.to_dict() for g in groups]
    finally:
        db.close()




# ---------------------------------------------------------------------------
# Node Tools
# ---------------------------------------------------------------------------

@mcp.tool(
    name="list_nodes",
    description=(
        "List every CloudNode (the physical box running cameras on the local "
        "network) for the org with status, hostname, and camera count. Use "
        "when troubleshooting at the box level — e.g. whether a whole node "
        "is offline vs whether one of its cameras is. For per-camera state, "
        "use list_cameras."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def list_nodes() -> list[dict]:
    org_id, db = _auth()
    try:
        nodes = db.query(CameraNode).filter_by(org_id=org_id).all()
        return [n.to_dict() for n in nodes]
    finally:
        db.close()


@mcp.tool(
    name="get_node",
    description=(
        "Get full detail for one CloudNode by node_id (hostname, IP, port, "
        "status, camera count). Use after list_nodes when you need detail on "
        "one specific box — e.g. to confirm which physical device the user "
        "should power-cycle."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def get_node(
    node_id: Annotated[str, "The node_id string (8-char UUID prefix)"],
) -> dict:
    org_id, db = _auth()
    try:
        node = (
            db.query(CameraNode)
            .filter_by(org_id=org_id, node_id=node_id)
            .first()
        )
        if not node:
            raise ToolError(f"Node '{node_id}' not found")
        return node.to_dict()
    finally:
        db.close()


# ---------------------------------------------------------------------------
# Recording Settings Tools
# ---------------------------------------------------------------------------

@mcp.tool(
    name="get_camera_recording_policy",
    description=(
        "Return the recording policy for a specific camera: whether 24/7 "
        "continuous recording is on, whether scheduled recording is on, and "
        "the scheduled start/end times (HH:MM, interpreted in the org's "
        "configured timezone — NOT UTC). Per-camera since v0.1.43 — "
        "replaces the previous org-level get_recording_settings. Use when "
        "the user asks 'is the garage cam recording right now?' or before "
        "filing an incident if it's relevant whether the moment was being "
        "recorded to disk on the CloudNode."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def get_camera_recording_policy(camera_id: str) -> dict:
    org_id, db = _auth()
    try:
        camera = db.query(Camera).filter_by(camera_id=camera_id, org_id=org_id).first()
        if not camera:
            return {"error": "camera_not_found", "camera_id": camera_id}
        return {
            "camera_id": camera_id,
            "continuous_24_7": bool(camera.continuous_24_7),
            "scheduled_recording": bool(camera.scheduled_recording),
            "scheduled_start": camera.scheduled_start,
            "scheduled_end": camera.scheduled_end,
        }
    finally:
        db.close()


@mcp.tool(
    name="set_camera_recording_policy",
    description=(
        "Set the recording policy for a specific camera. Any field omitted "
        "(or set to null) is left unchanged — pass only what you want to "
        "update. Use when the user asks 'turn on recording for the garage "
        "cam' or 'set scheduled recording on the front door cam from 18:00 "
        "to 06:00'. Times are HH:MM 24-hour, interpreted in the org's "
        "configured timezone (NOT UTC) — pass exactly what the user said, "
        "do not convert. Mutual-exclusion invariant: continuous_24_7 and "
        "scheduled_recording can't both be true; the call returns "
        "{error: 'modes_conflict'} if you try. Returns the new effective "
        "policy. Per-camera since v0.1.43."
    ),
)
@tracked
def set_camera_recording_policy(
    camera_id: str,
    continuous_24_7: bool | None = None,
    scheduled_recording: bool | None = None,
    scheduled_start: str | None = None,
    scheduled_end: str | None = None,
) -> dict:
    import re
    org_id, db = _auth()
    try:
        camera = db.query(Camera).filter_by(camera_id=camera_id, org_id=org_id).first()
        if not camera:
            return {"error": "camera_not_found", "camera_id": camera_id}

        # Validate HH:MM strings before assigning so a bad value from
        # an AI agent doesn't end up in the DB and silently break the
        # heartbeat handler's window check.
        for label, val in (("scheduled_start", scheduled_start),
                           ("scheduled_end", scheduled_end)):
            if val is not None and val != "" and not re.match(
                r"^([01]\d|2[0-3]):[0-5]\d$", val
            ):
                return {
                    "error": "invalid_time_format",
                    "field": label,
                    "value": val,
                    "expected": "HH:MM 24-hour, e.g. 08:30",
                }

        # Mutual-exclusion invariant: continuous and scheduled can't
        # both be on (heartbeat would silently ignore scheduled).
        # See update_camera_recording_policy in api/cameras.py for the
        # canonical comment.  An agent that wants to switch modes has
        # to pass the OFF for the previous mode in the same call.
        next_continuous = (
            continuous_24_7
            if continuous_24_7 is not None
            else camera.continuous_24_7
        )
        next_scheduled = (
            scheduled_recording
            if scheduled_recording is not None
            else camera.scheduled_recording
        )
        if next_continuous and next_scheduled:
            return {
                "error": "modes_conflict",
                "message": (
                    "continuous_24_7 and scheduled_recording can't both "
                    "be true. Pass one as false in the same call to switch."
                ),
            }

        if continuous_24_7 is not None:
            camera.continuous_24_7 = continuous_24_7
        if scheduled_recording is not None:
            camera.scheduled_recording = scheduled_recording
        if scheduled_start is not None:
            camera.scheduled_start = scheduled_start or None
        if scheduled_end is not None:
            camera.scheduled_end = scheduled_end or None

        db.commit()
        return {
            "success": True,
            "camera_id": camera_id,
            "continuous_24_7": bool(camera.continuous_24_7),
            "scheduled_recording": bool(camera.scheduled_recording),
            "scheduled_start": camera.scheduled_start,
            "scheduled_end": camera.scheduled_end,
        }
    finally:
        db.close()




# ---------------------------------------------------------------------------
# Audit / Stream Log Tools
# ---------------------------------------------------------------------------

@mcp.tool(
    name="get_stream_logs",
    description=(
        "Get recent stream-access log entries (one row per user × camera × "
        "~5min window). Use to audit who watched a sensitive camera, check "
        "whether a user reviewed a feed during a time of interest, or "
        "investigate suspicious viewing activity. Filter by camera_id to "
        "scope to one feed."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def get_stream_logs(
    camera_id: Annotated[str | None, "Filter by camera_id"] = None,
    limit: Annotated[int, Field(description="Max results", ge=1, le=500)] = 50,
) -> list[dict]:
    org_id, db = _auth()
    try:
        query = db.query(StreamAccessLog).filter_by(org_id=org_id)
        if camera_id:
            query = query.filter_by(camera_id=camera_id)
        logs = query.order_by(StreamAccessLog.accessed_at.desc()).limit(limit).all()
        return [log.to_dict() for log in logs]
    finally:
        db.close()


@mcp.tool(
    name="get_stream_stats",
    description=(
        "Get aggregated stream-viewing stats over the last N days: totals, "
        "by-camera, and by-user. Use to find the most-watched cameras, build "
        "a usage summary, or establish a baseline before deciding whether a "
        "viewing pattern looks unusual. For per-event detail, use "
        "get_stream_logs."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def get_stream_stats(
    days: Annotated[int, Field(description="Number of days to look back", ge=1, le=30)] = 7,
) -> dict:
    org_id, db = _auth()
    try:
        from sqlalchemy import func

        cutoff = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(days=days)
        base = db.query(StreamAccessLog).filter(
            StreamAccessLog.org_id == org_id,
            StreamAccessLog.accessed_at >= cutoff,
        )
        total = base.count()

        by_camera = (
            base.with_entities(
                StreamAccessLog.camera_id,
                func.count(StreamAccessLog.id).label("views"),
            )
            .group_by(StreamAccessLog.camera_id)
            .all()
        )

        by_user = (
            base.with_entities(
                StreamAccessLog.user_id,
                StreamAccessLog.user_email,
                func.count(StreamAccessLog.id).label("views"),
            )
            .group_by(StreamAccessLog.user_id, StreamAccessLog.user_email)
            .all()
        )

        return {
            "days": days,
            "total_views": total,
            "by_camera": [
                {"camera_id": cid, "views": v} for cid, v in by_camera
            ],
            "by_user": [
                {"user_id": uid, "email": email or "", "views": v}
                for uid, email, v in by_user
            ],
        }
    finally:
        db.close()


# ---------------------------------------------------------------------------
# System Overview Tool
# ---------------------------------------------------------------------------

@mcp.tool(
    name="get_system_status",
    description=(
        "High-level snapshot of the org's Sentinel deployment: camera count "
        "with online/offline split, node count with online/offline split, and "
        "the active plan. Good first call to orient before drilling in. For "
        "per-camera detail, use list_cameras."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def get_system_status() -> dict:
    org_id, db = _auth()
    try:
        cameras = db.query(Camera).filter_by(org_id=org_id).all()
        nodes = db.query(CameraNode).filter_by(org_id=org_id).all()

        online_cameras = sum(1 for c in cameras if c.effective_status != "offline")
        online_nodes = sum(1 for n in nodes if n.effective_status not in ("offline", "pending"))

        from app.core.plans import resolve_org_plan
        plan = resolve_org_plan(db, org_id)

        return {
            "org_id": org_id,
            "plan": plan,
            "cameras": {
                "total": len(cameras),
                "online": online_cameras,
                "offline": len(cameras) - online_cameras,
            },
            "nodes": {
                "total": len(nodes),
                "online": online_nodes,
                "offline": len(nodes) - online_nodes,
            },
        }
    finally:
        db.close()


# ---------------------------------------------------------------------------
# Incident Reporting Tools (write)
# ---------------------------------------------------------------------------
#
# These let an MCP agent file a structured, persistent incident report when it
# observes something noteworthy. Reports show up on the MCP page in the
# Command Center, where a human can acknowledge / resolve / dismiss them.
#
# Workflow the agent should follow:
#   1. create_incident(...) → returns incident_id
#   2. attach_snapshot(incident_id, camera_id) for each camera worth capturing
#   3. add_observation(incident_id, "I checked cameras X, Y, Z; saw...")
#   4. finalize_incident(incident_id, full_report_markdown)
# ---------------------------------------------------------------------------


def _agent_label() -> str:
    """Build the `created_by` label from the current MCP key context."""
    key_name = _ctx_key_name.get("") or "unknown"
    return f"mcp:{key_name}"


def _extract_snapshot_image_b64(result: dict, camera_id: str) -> str:
    """Pull `image_b64` out of a CloudNode `take_snapshot` response, or raise
    ToolError with a message the *user* can act on.

    CloudNode (>= the release that added the snapshot API) wraps its WS
    `command_result` payload as:
        success -> {"status": "success", "data": {"image_b64": "...", ...}}
        failure -> {"status": "error",   "error": "<human-readable reason>"}

    Older CloudNodes (pre-snapshot) send neither field and we fall back to
    the "update CloudNode" hint.

    Prior behaviour was to check only for `image_b64` and, if absent, blame
    the CloudNode version — even when the real cause was an FFmpeg crash,
    a dead HLS pipeline, or disk-full. That's the bug this helper fixes:
    any `status == "error"` is surfaced verbatim, and the couple of
    common cases get a friendlier hint."""
    if result.get("status") == "error":
        err = (result.get("error") or "").strip()
        err_lower = err.lower()
        # Pipeline-is-dead patterns the CloudNode actually sends today
        # (see cmd_take_snapshot in CloudNode's websocket.rs).
        if "no segments" in err_lower or "error opening input" in err_lower:
            raise ToolError(
                f"Camera '{camera_id}' has no active video stream. The CloudNode "
                f"is online but isn't producing HLS segments right now — common "
                f"causes are a dead FFmpeg worker, a full disk on the node, or "
                f"the camera being unplugged. Check the CloudNode dashboard for "
                f"the underlying error, then restart the node if needed."
            )
        if "ffmpeg" in err_lower:
            raise ToolError(
                f"Camera '{camera_id}' snapshot failed inside FFmpeg on the "
                f"CloudNode: {err or 'no detail provided'}. The video pipeline "
                f"may need to be restarted on the node."
            )
        # Fallback: just pass the node's message through.
        raise ToolError(
            f"Snapshot failed on the CloudNode: {err or 'unspecified failure'}"
        )

    image_b64 = (
        result.get("data", {}).get("image_b64") or result.get("image_b64")
    )
    if not image_b64:
        # Truly unknown response shape — most likely an old CloudNode
        # that pre-dates both the `{status, data}` envelope and image_b64.
        raise ToolError(
            "Camera node did not return image data — update CloudNode to latest version"
        )
    return image_b64


async def _capture_snapshot_bytes(
    org_id: str, camera_id: str
) -> tuple[bytes, str]:
    """Pull a fresh JPEG snapshot from a camera node via the WS bridge.
    Returns (jpeg_bytes, node_id). Raises ToolError on any failure."""
    db = SessionLocal()
    try:
        cam = (
            db.query(Camera)
            .filter_by(org_id=org_id, camera_id=camera_id)
            .first()
        )
        if not cam:
            raise ToolError(f"Camera '{camera_id}' not found")
        node = db.query(CameraNode).filter_by(id=cam.node_id).first()
        if not node:
            raise ToolError(f"Camera '{camera_id}' has no assigned node")
        node_id = node.node_id
    finally:
        db.close()

    from app.api.ws import manager

    if not manager.is_connected(node_id):
        raise ToolError(f"Node '{node_id}' is offline — cannot capture snapshot")

    try:
        result = await manager.send_command(
            node_id, "take_snapshot", {"camera_id": camera_id}, timeout=15.0,
        )
    except TimeoutError:
        raise ToolError("Snapshot timed out — camera node did not respond in time") from None
    except ValueError as e:
        raise ToolError(str(e)) from e

    image_b64 = _extract_snapshot_image_b64(result, camera_id)
    return base64.b64decode(image_b64), node_id


@mcp.tool(
    name="create_incident",
    description=(
        "Open a new incident report. Use when you observe something noteworthy "
        "(possible intruder, suspicious activity, equipment problem) that the "
        "user should review later. Returns the new incident_id, which you "
        "should pass to attach_snapshot/add_observation/finalize_incident as "
        "you continue investigating."
    ),
)
@tracked
def create_incident(
    title: Annotated[str, "Short title for the incident (max 200 chars)"],
    summary: Annotated[str, "One or two sentence summary of what was observed"],
    severity: Annotated[
        str,
        Field(description="Severity level: low, medium, high, or critical"),
    ] = "medium",
    camera_id: Annotated[
        str | None,
        "Optional: the primary camera_id this incident relates to",
    ] = None,
) -> dict:
    if severity not in INCIDENT_SEVERITIES:
        raise ToolError(
            f"Invalid severity '{severity}'. Must be one of: {', '.join(INCIDENT_SEVERITIES)}"
        )
    if not title.strip():
        raise ToolError("title is required")
    if not summary.strip():
        raise ToolError("summary is required")

    org_id, db = _auth()
    try:
        if camera_id:
            cam = (
                db.query(Camera)
                .filter_by(org_id=org_id, camera_id=camera_id)
                .first()
            )
            if not cam:
                raise ToolError(f"Camera '{camera_id}' not found")

        incident = Incident(
            org_id=org_id,
            camera_id=camera_id,
            title=title.strip()[:200],
            summary=summary.strip(),
            severity=severity,
            status="open",
            created_by=_agent_label(),
        )
        db.add(incident)
        db.commit()
        db.refresh(incident)

        # Fire an inbox + email notification.  Audience='all' because
        # any member of the org should know an AI agent created an
        # incident in their environment — this is the hand-off from
        # automated triage to human review.  ``severity='warning'``
        # for low/medium incidents and 'critical' for high/critical
        # so the inbox styling matches the actual urgency.
        try:
            from app.api.notifications import create_notification

            notif_severity = "critical" if severity in ("high", "critical") else "warning"
            create_notification(
                org_id=org_id,
                kind="incident_created",
                title=f"Incident #{incident.id}: {incident.title}",
                body=f"[{severity.upper()}] {incident.summary}",
                severity=notif_severity,
                audience="all",
                link=f"/incidents/{incident.id}",
                camera_id=camera_id,
                meta={"incident_id": incident.id, "severity": severity},
                db=db,
            )
        except Exception:
            # Notification failure must NEVER fail the incident
            # creation — the agent already wrote the row, and the
            # human can find it in the dashboard regardless.  Logged
            # for triage but not surfaced to the agent.
            import logging as _logging
            _logging.getLogger(__name__).exception(
                "[create_incident] notification emit failed for incident=%s",
                incident.id,
            )

        return incident.to_dict()
    finally:
        db.close()


@mcp.tool(
    name="add_observation",
    description=(
        "Append a text observation to an existing incident. Use this to record "
        "what you saw on additional cameras, what you ruled out, or any other "
        "context that will help the human reviewer understand the situation."
    ),
)
@tracked
def add_observation(
    incident_id: Annotated[int, "The incident id returned by create_incident"],
    text: Annotated[str, "Free-form observation text"],
    camera_id: Annotated[
        str | None,
        "Optional: camera this observation pertains to",
    ] = None,
) -> dict:
    if not text.strip():
        raise ToolError("text is required")

    org_id, db = _auth()
    try:
        incident = (
            db.query(Incident)
            .filter_by(id=incident_id, org_id=org_id)
            .first()
        )
        if not incident:
            raise ToolError(f"Incident {incident_id} not found")

        # Same org-scope check we do in attach_clip / attach_snapshot:
        # only let the agent reference cameras the org actually owns.
        # Without this, an agent could record a foreign camera_id as
        # text on our own incident, polluting the audit trail with
        # references that don't resolve in this org's context.  Not a
        # data-leak (the foreign cam doesn't get queried), just hygiene.
        if camera_id is not None:
            cam_exists = (
                db.query(Camera)
                .filter_by(org_id=org_id, camera_id=camera_id)
                .first()
            )
            if not cam_exists:
                raise ToolError(
                    f"Camera '{camera_id}' not found in this organization"
                )

        evidence = IncidentEvidence(
            incident_id=incident.id,
            kind="observation",
            text=text.strip(),
            camera_id=camera_id,
        )
        db.add(evidence)
        # Touch the incident so updated_at refreshes
        incident.updated_at = datetime.now(tz=UTC).replace(tzinfo=None)
        db.commit()
        db.refresh(evidence)
        return evidence.to_dict()
    finally:
        db.close()


@mcp.tool(
    name="attach_snapshot",
    description=(
        "Capture a fresh JPEG snapshot from a camera and attach it as evidence "
        "to an incident. The camera must be online. Use this to preserve what "
        "you saw at the moment of investigation."
    ),
)
@tracked
async def attach_snapshot(
    incident_id: Annotated[int, "The incident id to attach the snapshot to"],
    camera_id: Annotated[str, "The camera_id to capture from"],
    note: Annotated[
        str | None,
        "Optional caption for this snapshot (e.g. 'workshop entrance')",
    ] = None,
) -> dict:
    # Verify ownership before we hit the camera node
    org_id, db = _auth()
    try:
        incident = (
            db.query(Incident)
            .filter_by(id=incident_id, org_id=org_id)
            .first()
        )
        if not incident:
            raise ToolError(f"Incident {incident_id} not found")
    finally:
        db.close()

    jpeg_bytes, _node_id = await _capture_snapshot_bytes(org_id, camera_id)

    db = SessionLocal()
    try:
        evidence = IncidentEvidence(
            incident_id=incident_id,
            kind="snapshot",
            text=note.strip() if note else None,
            camera_id=camera_id,
            data=jpeg_bytes,
            data_mime="image/jpeg",
        )
        db.add(evidence)
        # Touch parent.  Org filter is belt-and-suspenders here — the
        # first session at line ~1408 already verified ownership and
        # Incident.org_id is functionally immutable in this codebase —
        # but pinning it on the second-session lookup makes the touch
        # silently no-op rather than mutate the wrong row in any
        # future where org_id ever becomes mutable.
        incident = (
            db.query(Incident)
            .filter_by(id=incident_id, org_id=org_id)
            .first()
        )
        if incident:
            incident.updated_at = datetime.now(tz=UTC).replace(tzinfo=None)
        db.commit()
        db.refresh(evidence)
        return evidence.to_dict()
    finally:
        db.close()


# Approximate seconds per HLS segment — CloudNode currently emits 2s .ts files.
# This is only used to map a requested duration_seconds to a segment count and
# to populate EXTINF in the synthetic playlist; the actual playback duration
# comes from the TS PCR timestamps and will be exactly correct.
_APPROX_SEGMENT_SECONDS = 2.0


@mcp.tool(
    name="attach_clip",
    description=(
        "Save a short video clip from a camera's recent live buffer as evidence "
        "on an incident. Pulls the most recent N segments from the in-memory "
        "HLS cache (no recording is started — this captures what's already "
        "buffered) and stores them as a single .ts blob the human reviewer can "
        "play back from the dashboard. Use after attach_snapshot when motion "
        "context matters more than a single frame. The camera's stream must "
        "have been live recently — only segments still in the buffer (~60s "
        "depending on server config) are available."
    ),
)
@tracked
def attach_clip(
    incident_id: Annotated[int, "The incident id to attach the clip to"],
    camera_id: Annotated[str, "The camera_id to capture from"],
    duration_seconds: Annotated[
        int,
        Field(
            description=(
                "How many seconds of recent video to capture (clamped to what's "
                "in the buffer; default 15)"
            ),
            ge=2,
            le=60,
        ),
    ] = 15,
    note: Annotated[
        str | None,
        "Optional caption for this clip (e.g. 'subject crossing into yard')",
    ] = None,
) -> dict:
    # Verify ownership before touching the cache.
    org_id, db = _auth()
    try:
        incident = (
            db.query(Incident)
            .filter_by(id=incident_id, org_id=org_id)
            .first()
        )
        if not incident:
            raise ToolError(f"Incident {incident_id} not found")
        cam = (
            db.query(Camera)
            .filter_by(org_id=org_id, camera_id=camera_id)
            .first()
        )
        if not cam:
            raise ToolError(f"Camera '{camera_id}' not found")
    finally:
        db.close()

    # Pull the most recent segments from the live cache.
    #
    # `attach_clip` is a SYNC tool — FastMCP runs it in an AnyIO worker
    # thread, off the event loop.  We must NOT iterate `_segment_cache`
    # directly here: the event loop's push→evict path mutates it
    # concurrently, and `sorted(cam_cache.keys())` mid-eviction raises
    # `RuntimeError: dictionary changed size during iteration`.
    # `snapshot_recent_segment_bytes` does the whole read under
    # `_segment_cache_lock` and hands back immutable bytes.
    wanted = max(1, int(round(duration_seconds / _APPROX_SEGMENT_SECONDS)))
    chunks = snapshot_recent_segment_bytes(camera_id, wanted)
    if chunks is None:
        raise ToolError(
            f"No buffered segments for camera '{camera_id}'. The stream must be "
            "live (or have been live very recently) for attach_clip to work."
        )
    if not chunks:
        raise ToolError(
            f"Buffer entries for '{camera_id}' were evicted before they could "
            "be read; try again."
        )

    # Concatenate raw .ts bytes — MPEG-TS is byte-concat-safe (PCR timestamps
    # carry through) so the result plays end-to-end without remuxing.
    blob = b"".join(chunks)
    segment_count = len(chunks)
    approx_duration = round(segment_count * _APPROX_SEGMENT_SECONDS, 1)

    db = SessionLocal()
    try:
        evidence = IncidentEvidence(
            incident_id=incident_id,
            kind="clip",
            text=note.strip() if note else None,
            camera_id=camera_id,
            data=blob,
            # Encode the duration as a MIME parameter so the playback endpoint
            # can populate EXTINF without a schema migration. Browsers ignore
            # parameters they don't recognise on `video/mp2t`.
            data_mime=f"video/mp2t;duration={approx_duration}",
        )
        db.add(evidence)
        # Touch parent.  Pin org_id on the lookup as belt-and-suspenders;
        # the first session at line ~1492 already verified ownership and
        # Incident.org_id is functionally immutable.
        incident = (
            db.query(Incident)
            .filter_by(id=incident_id, org_id=org_id)
            .first()
        )
        if incident:
            incident.updated_at = datetime.now(tz=UTC).replace(tzinfo=None)
        db.commit()
        db.refresh(evidence)
        result = evidence.to_dict()
        result["segment_count"] = segment_count
        result["approx_duration_seconds"] = approx_duration
        result["bytes"] = len(blob)
        return result
    finally:
        db.close()


@mcp.tool(
    name="update_incident",
    description=(
        "Edit fields on an existing incident. Use to escalate severity if the "
        "situation worsens, mark resolved/dismissed after confirming a false "
        "alarm, fix the short summary, or revise the long-form markdown report "
        "after new evidence. Pass only the fields you want to change — others "
        "are left alone. The report parameter REPLACES the existing body, so "
        "include the full revised text (the agent must already have it in "
        "context, e.g. from get_incident). For the very first report write, "
        "use finalize_incident instead."
    ),
)
@tracked
def update_incident(
    incident_id: Annotated[int, "The incident id to update"],
    status: Annotated[
        str | None,
        "New status: open, acknowledged, resolved, or dismissed",
    ] = None,
    severity: Annotated[
        str | None,
        "New severity: low, medium, high, or critical",
    ] = None,
    summary: Annotated[str | None, "New short summary text"] = None,
    report: Annotated[
        str | None,
        (
            "Full replacement for the long-form markdown report body. Provide "
            "the COMPLETE revised text — this overwrites the existing body. "
            "Leave None to keep the current report unchanged."
        ),
    ] = None,
) -> dict:
    if status is not None and status not in INCIDENT_STATUSES:
        raise ToolError(
            f"Invalid status '{status}'. Must be one of: {', '.join(INCIDENT_STATUSES)}"
        )
    if severity is not None and severity not in INCIDENT_SEVERITIES:
        raise ToolError(
            f"Invalid severity '{severity}'. Must be one of: {', '.join(INCIDENT_SEVERITIES)}"
        )
    if report is not None and not report.strip():
        raise ToolError(
            "report cannot be empty — pass None to leave the existing report "
            "unchanged, or pass the full revised markdown body"
        )

    org_id, db = _auth()
    try:
        incident = (
            db.query(Incident)
            .filter_by(id=incident_id, org_id=org_id)
            .first()
        )
        if not incident:
            raise ToolError(f"Incident {incident_id} not found")

        if status is not None:
            if status in ("resolved", "dismissed") and incident.status not in (
                "resolved",
                "dismissed",
            ):
                incident.resolved_at = datetime.now(tz=UTC).replace(tzinfo=None)
                incident.resolved_by = _agent_label()
            elif status == "open":
                incident.resolved_at = None
                incident.resolved_by = None
            incident.status = status
        if severity is not None:
            incident.severity = severity
        if summary is not None:
            incident.summary = summary.strip()
        if report is not None:
            incident.report = report.strip()

        db.commit()
        db.refresh(incident)
        return incident.to_dict()
    finally:
        db.close()


@mcp.tool(
    name="finalize_incident",
    description=(
        "Write the long-form markdown report body for the FIRST time at the "
        "end of your investigation, after you've attached snapshots/clips and "
        "added observations. This is the normal end-of-investigation step. "
        "If you need to revise an already-written report after new evidence, "
        "use update_incident with the report parameter instead — that path is "
        "designed for revisions."
    ),
)
@tracked
def finalize_incident(
    incident_id: Annotated[int, "The incident id to finalize"],
    report: Annotated[
        str,
        "Full incident report in markdown — include what you saw, where, when, "
        "what you ruled out, and any recommended actions",
    ],
) -> dict:
    if not report.strip():
        raise ToolError("report is required")

    org_id, db = _auth()
    try:
        incident = (
            db.query(Incident)
            .filter_by(id=incident_id, org_id=org_id)
            .first()
        )
        if not incident:
            raise ToolError(f"Incident {incident_id} not found")

        incident.report = report.strip()
        incident.updated_at = datetime.now(tz=UTC).replace(tzinfo=None)
        db.commit()
        db.refresh(incident)
        return incident.to_dict()
    finally:
        db.close()


@mcp.tool(
    name="list_incidents",
    description=(
        "List incident reports for this organization, most recent first. "
        "Use this to check what incidents are already open before filing a "
        "duplicate, to follow up on past reports, or to look at activity "
        "patterns. Returns compact rows (id, title, severity, status, camera, "
        "timestamps, evidence count) without the full report body — call "
        "get_incident for the full detail of a specific one."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def list_incidents(
    status: Annotated[
        str | None,
        "Filter by status: open, acknowledged, resolved, or dismissed",
    ] = None,
    severity: Annotated[
        str | None,
        "Filter by severity: low, medium, high, or critical",
    ] = None,
    camera_id: Annotated[
        str | None,
        "Filter to incidents attached to a specific camera",
    ] = None,
    limit: Annotated[
        int,
        Field(description="Max number of incidents to return", ge=1, le=100),
    ] = 20,
    offset: Annotated[
        int,
        Field(description="How many rows to skip (for pagination)", ge=0),
    ] = 0,
) -> dict:
    if status is not None and status not in INCIDENT_STATUSES:
        raise ToolError(
            f"Invalid status '{status}'. Must be one of: {', '.join(INCIDENT_STATUSES)}"
        )
    if severity is not None and severity not in INCIDENT_SEVERITIES:
        raise ToolError(
            f"Invalid severity '{severity}'. Must be one of: {', '.join(INCIDENT_SEVERITIES)}"
        )

    org_id, db = _auth()
    try:
        q = db.query(Incident).filter_by(org_id=org_id)
        if status is not None:
            q = q.filter(Incident.status == status)
        if severity is not None:
            q = q.filter(Incident.severity == severity)
        if camera_id is not None:
            q = q.filter(Incident.camera_id == camera_id)

        total = q.count()
        rows = (
            q.order_by(Incident.created_at.desc())
            .offset(offset)
            .limit(limit)
            .all()
        )

        # Strip the report body from the list view — call get_incident to read it.
        incidents = []
        for r in rows:
            d = r.to_dict(include_evidence=False)
            # Keep a short preview so agents can scan without a second call
            report_text = d.pop("report", "") or ""
            d["has_report"] = bool(report_text.strip())
            incidents.append(d)

        return {
            "total": total,
            "returned": len(incidents),
            "offset": offset,
            "limit": limit,
            "incidents": incidents,
        }
    finally:
        db.close()


@mcp.tool(
    name="get_incident",
    description=(
        "Get the full detail of a single incident: summary, full markdown "
        "report, all observations, and all evidence metadata (including "
        "evidence ids you can pass to get_incident_snapshot to see the "
        "attached images). Use this to read back a past report in full."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def get_incident(
    incident_id: Annotated[int, "The incident id to fetch"],
) -> dict:
    org_id, db = _auth()
    try:
        incident = (
            db.query(Incident)
            .filter_by(id=incident_id, org_id=org_id)
            .first()
        )
        if not incident:
            raise ToolError(f"Incident {incident_id} not found")
        return incident.to_dict(include_evidence=True)
    finally:
        db.close()


@mcp.tool(
    name="get_incident_snapshot",
    description=(
        "Fetch a snapshot image that was previously attached to an incident "
        "as evidence. Returns the stored JPEG so you can actually SEE what "
        "was captured. Pair with get_incident to discover evidence ids."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def get_incident_snapshot(
    incident_id: Annotated[int, "The incident id the snapshot belongs to"],
    evidence_id: Annotated[int, "The evidence id from get_incident's evidence list"],
) -> Image:
    org_id, db = _auth()
    try:
        incident = (
            db.query(Incident)
            .filter_by(id=incident_id, org_id=org_id)
            .first()
        )
        if not incident:
            raise ToolError(f"Incident {incident_id} not found")

        evidence = (
            db.query(IncidentEvidence)
            .filter_by(id=evidence_id, incident_id=incident_id)
            .first()
        )
        if not evidence:
            raise ToolError(
                f"Evidence {evidence_id} not found on incident {incident_id}"
            )
        if evidence.kind != "snapshot" or evidence.data is None:
            raise ToolError(
                f"Evidence {evidence_id} is not a snapshot with attached image data"
            )

        # Map stored mime type to the Image format fastmcp expects.
        mime = (evidence.data_mime or "image/jpeg").lower()
        if mime == "image/jpeg" or mime == "image/jpg":
            fmt = "jpeg"
        elif mime == "image/png":
            fmt = "png"
        elif mime == "image/webp":
            fmt = "webp"
        else:
            fmt = "jpeg"  # sensible fallback — the data likely still decodes

        return Image(data=bytes(evidence.data), format=fmt)
    finally:
        db.close()


@mcp.tool(
    name="get_incident_clip",
    description=(
        "Look up metadata about a video clip previously attached to an "
        "incident with attach_clip. Returns size, approximate duration, MIME, "
        "and the camera it came from. Note: this returns metadata only — "
        "the agent can't watch video, but a human reviewer can play the clip "
        "from the dashboard. Use this to confirm a clip was saved correctly."
    ),
    annotations={"readOnlyHint": True},
)
@tracked
def get_incident_clip(
    incident_id: Annotated[int, "The incident id the clip belongs to"],
    evidence_id: Annotated[int, "The evidence id from get_incident's evidence list"],
) -> dict:
    org_id, db = _auth()
    try:
        incident = (
            db.query(Incident)
            .filter_by(id=incident_id, org_id=org_id)
            .first()
        )
        if not incident:
            raise ToolError(f"Incident {incident_id} not found")

        evidence = (
            db.query(IncidentEvidence)
            .filter_by(id=evidence_id, incident_id=incident_id)
            .first()
        )
        if not evidence:
            raise ToolError(
                f"Evidence {evidence_id} not found on incident {incident_id}"
            )
        if evidence.kind != "clip" or evidence.data is None:
            raise ToolError(
                f"Evidence {evidence_id} is not a clip with attached video data"
            )

        # Pull the duration parameter back out of the MIME type if present.
        mime = evidence.data_mime or "video/mp2t"
        approx_duration: float | None = None
        if ";" in mime:
            base_mime, *params = [p.strip() for p in mime.split(";")]
            for p in params:
                if p.startswith("duration="):
                    try:
                        approx_duration = float(p.split("=", 1)[1])
                    except (ValueError, IndexError):
                        pass
        else:
            base_mime = mime

        d = evidence.to_dict()
        d.update({
            "mime": base_mime,
            "approx_duration_seconds": approx_duration,
            "bytes": len(evidence.data),
            "playback_hint": (
                "A human reviewer can play this clip from the dashboard's "
                "incident detail view; agents cannot watch video directly."
            ),
        })
        return d
    finally:
        db.close()
