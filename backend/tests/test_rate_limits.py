"""
Tests pinning the rate-limit decorators we audited.

These exist to keep someone from quietly removing a ``@limiter.limit``
decorator during a refactor and silently re-opening one of the abuse
vectors we closed.

Approach:

  - SSE endpoints stream forever (``while True: yield ...``) so a
    plain ``client.get("/stream")`` would block reading the body.
    We monkeypatch each broadcaster's ``subscribe`` to return ``None``
    so the endpoint short-circuits with a 429 (fast path).  The
    slowapi rate-limit decorator runs BEFORE the endpoint body, so
    the limiter still increments on every request.  The first ``cap``
    requests return the broadcaster's 429; the (cap+1)th returns
    slowapi's 429 — distinguishable by the response body shape that
    ``rate_limit_exceeded_handler`` in ``app/main.py`` emits
    (``{"error": "rate_limit_exceeded", ...}`` + ``Retry-After: 60``).
  - REST endpoints (admin DB queries, incident evidence) return a
    real response in milliseconds — no streaming workaround needed.
  - The websocket connect-throttle is in-memory (not slowapi-backed)
    and tested directly against the throttle object.

The ``reset_rate_limiter`` autouse fixture in conftest.py clears
slowapi's in-process counters between tests, so tests are
order-independent and each starts from a clean bucket.
"""

from __future__ import annotations

import collections
import time

import pytest

from app.api.ws import (
    WS_MAX_CONNECTS_PER_MINUTE,
    NodeRateLimiter,
    _ws_connect_throttle,
)
from app.mcp.server import _RateLimiter

# ── Helpers ─────────────────────────────────────────────────────────


def _is_slowapi_429(resp) -> bool:
    """Return True iff the response is a slowapi rate-limit 429.

    Distinguishes from app-level 429s (broadcaster-cap-hit, viewer-hour
    cap, etc.) by the custom envelope ``rate_limit_exceeded_handler``
    in ``app/main.py`` emits.
    """
    if resp.status_code != 429:
        return False
    try:
        body = resp.json()
    except Exception:
        return False
    return body.get("error") == "rate_limit_exceeded"


def _hammer(client, method: str, path: str, n: int):
    """Hit ``path`` ``n`` times sequentially.  Returns the list of
    responses so the caller can assert on the boundary behavior."""
    fn = getattr(client, method.lower())
    return [fn(path) for _ in range(n)]


# ── Fixtures: short-circuit each SSE broadcaster so requests return fast ──


@pytest.fixture
def fast_notifications_sse(monkeypatch):
    """Force the notifications broadcaster's subscribe() to return None
    so the SSE endpoint immediately raises 429 instead of streaming
    forever.  The slowapi limiter still counts every request."""
    from app.api import notifications as mod
    monkeypatch.setattr(
        mod.notification_broadcaster, "subscribe",
        lambda *args, **kwargs: None,
    )


@pytest.fixture
def fast_motion_sse(monkeypatch):
    """Same trick for the motion-events SSE."""
    from app.api import motion as mod
    monkeypatch.setattr(
        mod.motion_broadcaster, "subscribe",
        lambda *args, **kwargs: None,
    )


@pytest.fixture
def fast_mcp_activity_sse(monkeypatch):
    """Same trick for the MCP-activity SSE."""
    from app.api import mcp_activity as mod
    monkeypatch.setattr(
        mod.tracker, "subscribe",
        lambda *args, **kwargs: None,
    )


# ── SSE connect-rate limits (60/min each) ──────────────────────────


def test_notifications_stream_rate_limited(viewer_client, fast_notifications_sse):
    """``/api/notifications/stream`` caps connect attempts at 60/min
    per org.  Without this, an attacker could burn JWT-verify CPU by
    rapidly opening connections that the per-org subscriber cap then
    rejects."""
    # First 60 → broadcaster returns None → endpoint raises its own 429.
    responses = _hammer(viewer_client, "get", "/api/notifications/stream", 60)
    for i, r in enumerate(responses):
        assert r.status_code == 429, f"req #{i + 1}: expected 429, got {r.status_code}"
        assert not _is_slowapi_429(r), f"req #{i + 1}: should be broadcaster 429, not slowapi"

    # 61st → slowapi rate limit fires before the endpoint body runs.
    overflow = viewer_client.get("/api/notifications/stream")
    assert _is_slowapi_429(overflow), (
        f"expected slowapi rate-limit 429 at req #61, got "
        f"{overflow.status_code} body={overflow.text!r}"
    )
    assert overflow.headers.get("retry-after") == "60"


def test_motion_events_stream_rate_limited(viewer_client, fast_motion_sse):
    """``/api/motion/events/stream`` — same protection as the
    notifications stream."""
    for r in _hammer(viewer_client, "get", "/api/motion/events/stream", 60):
        assert r.status_code == 429
        assert not _is_slowapi_429(r)

    overflow = viewer_client.get("/api/motion/events/stream")
    assert _is_slowapi_429(overflow)


def test_mcp_activity_stream_rate_limited(admin_client, fast_mcp_activity_sse):
    """``/api/mcp/activity/stream`` — admin-only SSE, same cap."""
    for r in _hammer(admin_client, "get", "/api/mcp/activity/stream", 60):
        assert r.status_code == 429
        assert not _is_slowapi_429(r)

    overflow = admin_client.get("/api/mcp/activity/stream")
    assert _is_slowapi_429(overflow)


# ── Admin DB-heavy endpoints ───────────────────────────────────────


def test_audit_stream_logs_rate_limited(admin_client):
    """Admin's stream-access-logs list runs a multi-clause SQL query
    against StreamAccessLog.  120/min cap matches sibling
    /api/audit-logs and keeps a runaway dashboard tab from saturating
    SQLite."""
    for r in _hammer(admin_client, "get", "/api/audit/stream-logs", 120):
        # Empty DB → 200 with empty list — that's the success path.
        assert r.status_code == 200, f"expected 200, got {r.status_code}: {r.text}"

    overflow = admin_client.get("/api/audit/stream-logs")
    assert _is_slowapi_429(overflow)


def test_audit_stream_logs_stats_rate_limited(admin_client):
    """Stats endpoint is heavier (multiple aggregating queries), so
    half the budget — 60/min."""
    for r in _hammer(admin_client, "get", "/api/audit/stream-logs/stats", 60):
        assert r.status_code == 200

    overflow = admin_client.get("/api/audit/stream-logs/stats")
    assert _is_slowapi_429(overflow)


def test_mcp_activity_logs_rate_limited(admin_client):
    """MCP audit log list — same calculus as audit/stream-logs.  120/min."""
    for r in _hammer(admin_client, "get", "/api/mcp/activity/logs", 120):
        assert r.status_code == 200

    overflow = admin_client.get("/api/mcp/activity/logs")
    assert _is_slowapi_429(overflow)


def test_mcp_activity_logs_stats_rate_limited(admin_client):
    """MCP stats — 60/min like audit stats."""
    for r in _hammer(admin_client, "get", "/api/mcp/activity/logs/stats", 60):
        assert r.status_code == 200

    overflow = admin_client.get("/api/mcp/activity/logs/stats")
    assert _is_slowapi_429(overflow)


# ── Incident evidence proxy ────────────────────────────────────────


def test_incident_evidence_blob_rate_limited(admin_client):
    """Evidence blob endpoint serves arbitrary-size video bytes from
    the DB and BYPASSES the viewer-hour cap that protects the live
    HLS endpoints.  120/min keeps it from becoming a poor-man's
    bandwidth tap.

    We point at a non-existent incident so the handler short-circuits
    with 404 — but the limiter fires BEFORE the handler body, so the
    cap is still exercised."""
    for r in _hammer(admin_client, "get", "/api/incidents/99999/evidence/1", 120):
        assert r.status_code == 404, f"expected 404, got {r.status_code}"

    overflow = admin_client.get("/api/incidents/99999/evidence/1")
    assert _is_slowapi_429(overflow)


def test_incident_evidence_playlist_rate_limited(admin_client):
    """``/playlist.m3u8`` proxy endpoint — same cap as the blob endpoint
    it points at."""
    for r in _hammer(
        admin_client, "get", "/api/incidents/99999/evidence/1/playlist.m3u8", 120,
    ):
        assert r.status_code == 404

    overflow = admin_client.get(
        "/api/incidents/99999/evidence/1/playlist.m3u8",
    )
    assert _is_slowapi_429(overflow)


# ── WebSocket connect throttle (per-node-id, in-memory) ────────────


def test_ws_connect_throttle_allows_under_cap():
    """Up to ``WS_MAX_CONNECTS_PER_MINUTE`` attempts in a window must
    all be allowed."""
    throttle = NodeRateLimiter(
        max_per_window=WS_MAX_CONNECTS_PER_MINUTE,
        window_seconds=60.0,
    )
    for i in range(WS_MAX_CONNECTS_PER_MINUTE):
        assert throttle.allow("node_alpha"), f"attempt #{i + 1} should be allowed"


def test_ws_connect_throttle_rejects_over_cap():
    """Past the cap, allow() returns False — endpoint then closes the
    handshake with code 1013 (the WS spec equivalent of HTTP 429)."""
    throttle = NodeRateLimiter(
        max_per_window=WS_MAX_CONNECTS_PER_MINUTE,
        window_seconds=60.0,
    )
    for _ in range(WS_MAX_CONNECTS_PER_MINUTE):
        throttle.allow("node_beta")
    # The (cap + 1)th attempt is rejected.
    assert not throttle.allow("node_beta")


def test_ws_connect_throttle_isolated_per_node_id():
    """A noisy node hitting its cap must not block a quiet node from
    connecting.  Without this, one stolen API key could deny service
    to every other node in the same org."""
    throttle = NodeRateLimiter(
        max_per_window=WS_MAX_CONNECTS_PER_MINUTE,
        window_seconds=60.0,
    )
    # Saturate node_loud.
    for _ in range(WS_MAX_CONNECTS_PER_MINUTE):
        throttle.allow("node_loud")
    assert not throttle.allow("node_loud")

    # node_quiet has its own bucket — first connect still works.
    assert throttle.allow("node_quiet")


def test_ws_connect_throttle_window_evicts_old_attempts():
    """Old attempts age out of the sliding window so a node that
    hit its cap a long time ago can connect again now."""
    # Tiny window so we don't have to wait.
    throttle = NodeRateLimiter(max_per_window=2, window_seconds=0.05)
    assert throttle.allow("node_window")
    assert throttle.allow("node_window")
    assert not throttle.allow("node_window")  # at cap

    # Wait past the window and try again — bucket should be empty.
    time.sleep(0.06)
    assert throttle.allow("node_window")


def test_ws_connect_throttle_singleton_actually_wired():
    """Pin the singleton's existence so the import path the WS
    endpoint uses doesn't get accidentally renamed.  If this import
    fails, the endpoint reference would NameError at first connect."""
    from app.api.ws import _ws_connect_throttle as throttle_ref
    # Same object used by node_websocket() — sanity check.
    assert throttle_ref is _ws_connect_throttle
    # Default cap matches the documented constant.
    assert throttle_ref._max == WS_MAX_CONNECTS_PER_MINUTE


# ── MCP rate-limiter idle-key prune (unbounded-dict leak fix) ──────

# The MCP `_RateLimiter` keyed its two windows by API-key hash and never
# forgot a key, so every key that ever called MCP — revoked, rotated,
# one-off — kept a dict entry forever.  `_prune` sweeps fully-aged-out
# keys; `check()` fires it opportunistically once per `_PRUNE_INTERVAL`.
# These tests pin that the sweep drops dead keys, keeps live ones, and
# stays off the hot path inside the interval.


def test_mcp_rate_limiter_prune_drops_fully_aged_out_key():
    """A key whose 24h daily window has fully expired is forgotten
    entirely — both its minute and daily deques are dropped."""
    rl = _RateLimiter()
    now = time.time()
    stale_ts = now - 86_400.0 - 100.0  # past the 24h window
    rl._minute["dead_key"] = collections.deque([stale_ts])
    rl._daily["dead_key"] = collections.deque([stale_ts])

    rl._prune(now)

    assert "dead_key" not in rl._daily
    assert "dead_key" not in rl._minute


def test_mcp_rate_limiter_prune_keeps_active_key():
    """A key with a hit inside the 24h window survives, and its still-valid
    timestamp is not discarded by the sweep."""
    rl = _RateLimiter()
    now = time.time()
    fresh_ts = now - 10.0  # 10s ago — inside both windows
    rl._minute["live_key"] = collections.deque([fresh_ts])
    rl._daily["live_key"] = collections.deque([fresh_ts])

    rl._prune(now)

    assert "live_key" in rl._daily
    assert "live_key" in rl._minute
    assert list(rl._daily["live_key"]) == [fresh_ts]


def test_mcp_rate_limiter_prune_is_selective():
    """Mixed population: only the aged-out key is dropped; the active one
    and its timestamps are untouched."""
    rl = _RateLimiter()
    now = time.time()
    rl._daily["dead"] = collections.deque([now - 86_400.0 - 1.0])
    rl._minute["dead"] = collections.deque([now - 86_400.0 - 1.0])
    rl._daily["alive"] = collections.deque([now - 30.0])
    rl._minute["alive"] = collections.deque([now - 30.0])

    rl._prune(now)

    assert "dead" not in rl._daily and "dead" not in rl._minute
    assert "alive" in rl._daily and "alive" in rl._minute


def test_mcp_rate_limiter_check_triggers_prune_after_interval():
    """The opportunistic sweep fires from check() once _PRUNE_INTERVAL has
    elapsed, so the leak is bounded without a background task.  We force
    the time-gate open by backdating _last_prune."""
    rl = _RateLimiter()
    # A key untouched for over a day (epoch timestamp is unambiguously old).
    rl._daily["ancient"] = collections.deque([0.0])
    rl._minute["ancient"] = collections.deque([0.0])
    rl._last_prune = 0.0  # pretend we last swept long ago → gate open

    allowed, _, _ = rl.check("newcomer", minute_limit=10, daily_limit=100)
    assert allowed

    # The ancient key was swept; the newcomer is now tracked.
    assert "ancient" not in rl._daily
    assert "ancient" not in rl._minute
    assert "newcomer" in rl._daily


def test_mcp_rate_limiter_check_does_not_prune_within_interval():
    """Within the interval the sweep must NOT run — otherwise it'd be an
    O(keys) walk on every request.  A stale key present just before a
    check() (with a recent _last_prune) must still be there afterward."""
    rl = _RateLimiter()
    rl._daily["ancient"] = collections.deque([0.0])
    rl._minute["ancient"] = collections.deque([0.0])
    rl._last_prune = time.time()  # just pruned → gate closed

    rl.check("newcomer", minute_limit=10, daily_limit=100)

    # check() only touches its own key, and the gate held, so "ancient" stays.
    assert "ancient" in rl._daily
