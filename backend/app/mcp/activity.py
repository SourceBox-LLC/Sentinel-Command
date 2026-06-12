"""
MCP Activity Tracker — in-memory event log + DB persistence.

Tracks every MCP tool invocation, maintains session info,
publishes events to SSE subscribers (the MCP dashboard),
and persists completed events to the database for audit.
"""

import asyncio
import logging
import threading
import time
from collections import deque
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from typing import Optional

logger = logging.getLogger(__name__)


def _persist_event(event: "McpEvent"):
    """Write an MCP event to the database (runs in background thread)."""
    try:
        from app.core.database import SessionLocal
        from app.models.models import McpActivityLog

        db = SessionLocal()
        try:
            log = McpActivityLog(
                org_id=event.org_id,
                tool_name=event.tool_name,
                key_name=event.key_name,
                status=event.status,
                duration_ms=int(event.duration_ms) if event.duration_ms else None,
                args_summary=event.args_summary,
                error=event.error,
                timestamp=datetime.fromtimestamp(event.timestamp, tz=UTC).replace(tzinfo=None),
            )
            db.add(log)
            db.commit()
        finally:
            db.close()
    except Exception:
        logger.exception("[Activity] Failed to persist MCP event to DB")


@dataclass
class McpEvent:
    """A single MCP tool invocation event."""
    id: str
    timestamp: float
    tool_name: str
    org_id: str
    key_name: str
    status: str  # "started" | "completed" | "error"
    duration_ms: Optional[float] = None
    error: Optional[str] = None
    args_summary: Optional[str] = None

    def to_dict(self) -> dict:
        return {k: v for k, v in asdict(self).items() if v is not None}


# Per-org SSE subscriber cap. Tiered — see the matching comment in api/motion.py.
# Route handler passes the plan-specific cap from PLAN_LIMITS.
MAX_SSE_SUBSCRIBERS_PER_ORG = 100  # fallback — Pro Plus default


class McpActivityTracker:
    """
    Thread-safe in-memory tracker for MCP tool calls.

    - Stores a rolling window of recent events (circular buffer).
    - Tracks active sessions by API key name + org.
    - Publishes events to async subscribers (SSE connections).
    """

    def __init__(self, max_events: int = 500):
        self._events: deque[McpEvent] = deque(maxlen=max_events)
        # {org_id: {key_name: {"last_active": float, "call_count": int, "key_id": int}}}
        self._sessions: dict[str, dict[str, dict]] = {}
        # {org_id: [asyncio.Queue, ...]}
        self._subscribers: dict[str, list[asyncio.Queue]] = {}
        self._lock = threading.Lock()
        self._total_calls: dict[str, int] = {}  # org_id -> total call count
        # Event loop the SSE subscribers live on — captured at subscribe
        # time so worker-thread publishers can call_soon_threadsafe.
        self._loop: asyncio.AbstractEventLoop | None = None

    def log_event(self, event: McpEvent):
        """Log a tool call event and notify subscribers."""
        with self._lock:
            self._events.append(event)

            # Track session
            if event.org_id not in self._sessions:
                self._sessions[event.org_id] = {}
            sess = self._sessions[event.org_id]
            if event.key_name not in sess:
                sess[event.key_name] = {"call_count": 0}
            sess[event.key_name]["last_active"] = event.timestamp
            sess[event.key_name]["call_count"] += 1

            # Track total calls
            self._total_calls[event.org_id] = self._total_calls.get(event.org_id, 0) + 1

        # Persist to DB in background thread (non-blocking)
        threading.Thread(target=_persist_event, args=(event,), daemon=True).start()

        # Notify SSE subscribers (non-blocking)
        self._notify(event)

    def _notify(self, event: McpEvent):
        """Push event to all SSE subscribers for this org.

        Most MCP tools are sync ``def``s run on anyio WORKER threads, so
        this is usually called OFF the event loop — and ``asyncio.Queue``
        is not thread-safe (its waiter wakeup uses non-threadsafe
        ``call_soon``, so a cross-thread ``put_nowait`` can delay or in
        rare interleavings break delivery).  Publish via
        ``call_soon_threadsafe`` when a loop is registered; snapshot the
        subscriber list under the lock so a concurrent subscribe/
        unsubscribe can't mutate it mid-iteration.
        """
        with self._lock:
            queues = list(self._subscribers.get(event.org_id, []))
        if not queues:
            return

        loop = self._loop
        def _deliver():
            dead = []
            for q in queues:
                try:
                    q.put_nowait(event)
                except asyncio.QueueFull:
                    dead.append(q)
            if dead:
                with self._lock:
                    for q in dead:
                        try:
                            self._subscribers[event.org_id].remove(q)
                        except (ValueError, KeyError):
                            pass

        if loop is not None and not loop.is_closed():
            try:
                loop.call_soon_threadsafe(_deliver)
                return
            except RuntimeError:
                pass  # loop shutting down — fall through to best-effort
        _deliver()

    def subscribe(self, org_id: str, cap: int = MAX_SSE_SUBSCRIBERS_PER_ORG) -> Optional[asyncio.Queue]:
        """Create a new SSE subscription for an org.

        ``cap`` is the per-tier subscriber cap the caller looked up (see
        PLAN_LIMITS). Returns the queue on success, or ``None`` when the
        org is already at the cap — the route handler translates that into
        a 429 so the client doesn't hang on an endless fake stream.
        """
        with self._lock:
            existing = self._subscribers.setdefault(org_id, [])
            if len(existing) >= cap:
                logger.warning(
                    "[Activity] SSE cap hit for org %s (%d/%d) — rejecting",
                    org_id, len(existing), cap,
                )
                return None
            q: asyncio.Queue = asyncio.Queue(maxsize=100)
            existing.append(q)
            # Capture the subscriber's loop so worker-thread publishers
            # (_notify) can hand events over via call_soon_threadsafe.
            try:
                self._loop = asyncio.get_running_loop()
            except RuntimeError:
                pass
        logger.info("[Activity] New SSE subscriber for org %s (%d/%d)",
                     org_id, len(existing), cap)
        return q

    def unsubscribe(self, org_id: str, q: asyncio.Queue):
        """Remove an SSE subscription."""
        with self._lock:
            if org_id in self._subscribers:
                try:
                    self._subscribers[org_id].remove(q)
                except ValueError:
                    pass

    def get_recent_events(self, org_id: str, limit: int = 50) -> list[McpEvent]:
        """Get the most recent events for an org."""
        with self._lock:
            org_events = [e for e in self._events if e.org_id == org_id]
            return org_events[-limit:]

    def get_active_sessions(self, org_id: str, timeout: float = 300.0) -> list[dict]:
        """
        Get sessions that have been active within `timeout` seconds.
        Returns list of {key_name, last_active, call_count, status}.
        """
        now = time.time()
        with self._lock:
            sessions = self._sessions.get(org_id, {})
            result = []
            for key_name, info in sessions.items():
                last_active = info.get("last_active", 0)
                age = now - last_active
                result.append({
                    "key_name": key_name,
                    "last_active": last_active,
                    "last_active_ago": round(age),
                    "call_count": info.get("call_count", 0),
                    "status": "active" if age < 60 else ("idle" if age < timeout else "disconnected"),
                })
            # Sort by most recently active
            result.sort(key=lambda s: s["last_active"], reverse=True)
            # Only return non-disconnected
            return [s for s in result if s["status"] != "disconnected"]

    def get_stats(self, org_id: str) -> dict:
        """Get aggregate stats for an org."""
        now = time.time()
        with self._lock:
            org_events = [e for e in self._events if e.org_id == org_id]
            # Calls in last 60 seconds
            recent = [e for e in org_events if now - e.timestamp < 60]
            # Calls in last 5 minutes
            recent_5m = [e for e in org_events if now - e.timestamp < 300]
            # Error rate
            errors = [e for e in org_events if e.status == "error"]

            sessions = self._sessions.get(org_id, {})
            active_count = sum(
                1 for info in sessions.values()
                if now - info.get("last_active", 0) < 300
            )

            return {
                "total_calls": self._total_calls.get(org_id, 0),
                "calls_per_min": len(recent),
                "calls_5m": len(recent_5m),
                "error_count": len(errors),
                "active_clients": active_count,
                "recent_event_count": len(org_events),
            }


# Singleton — imported by server.py and the activity API router
tracker = McpActivityTracker()
