"""Phase 3 integration motion-SSE tests.

The integration motion feed reuses the org-wide motion pipeline but via a
SEPARATE subscriber pool so a persistent Home Assistant connection never
consumes a dashboard SSE slot. These cover the separate-pool design, the
per-org cap, and the endpoint's auth + 429 (the streaming success path is
left to the dashboard SSE's own coverage — it's an identical generator —
since a TestClient GET on an infinite stream would block).
"""

import hashlib

import pytest

from app.api.integration import INTEGRATION_MAX_SSE_SUBSCRIBERS
from app.api.motion import integration_motion_broadcaster, motion_broadcaster
from app.models.models import McpApiKey

ORG = "org_test123"
RAW_KEY = "osi_phase3testkey00000000000000000000"


@pytest.fixture(autouse=True)
def _clear_pools():
    """Both broadcasters are module singletons — reset their subscriber
    pools around each test so cap/separation assertions are deterministic."""
    integration_motion_broadcaster._subscribers.clear()
    motion_broadcaster._subscribers.clear()
    yield
    integration_motion_broadcaster._subscribers.clear()
    motion_broadcaster._subscribers.clear()


def _make_integration_key(db, org=ORG, raw=RAW_KEY):
    db.add(McpApiKey(
        org_id=org,
        key_hash=hashlib.sha256(raw.encode()).hexdigest(),
        name="Home Assistant",
        kind="integration",
    ))
    db.commit()


# ── Separate-pool design ────────────────────────────────────────────

def test_integration_broadcaster_is_a_separate_instance():
    assert integration_motion_broadcaster is not motion_broadcaster


def test_integration_broadcaster_delivers_events():
    q = integration_motion_broadcaster.subscribe(ORG, INTEGRATION_MAX_SSE_SUBSCRIBERS)
    payload = {"type": "motion", "camera_id": "cam_a", "score": 80}
    integration_motion_broadcaster.notify(ORG, payload)
    assert q.get_nowait() == payload


def test_pools_are_independent():
    """Filling the integration pool to its cap must NOT block the dashboard
    pool (and vice versa) — HA connections and dashboard tabs don't contend."""
    for _ in range(INTEGRATION_MAX_SSE_SUBSCRIBERS):
        assert integration_motion_broadcaster.subscribe(
            ORG, INTEGRATION_MAX_SSE_SUBSCRIBERS
        ) is not None
    # Integration pool is now full...
    assert integration_motion_broadcaster.subscribe(
        ORG, INTEGRATION_MAX_SSE_SUBSCRIBERS
    ) is None
    # ...but the dashboard pool is entirely untouched.
    assert motion_broadcaster.subscribe(ORG, 5) is not None


def test_cap_is_per_org():
    """One org filling its integration pool doesn't affect another org."""
    for _ in range(INTEGRATION_MAX_SSE_SUBSCRIBERS):
        integration_motion_broadcaster.subscribe(ORG, INTEGRATION_MAX_SSE_SUBSCRIBERS)
    assert integration_motion_broadcaster.subscribe(ORG, INTEGRATION_MAX_SSE_SUBSCRIBERS) is None
    # A different org has its own budget.
    assert integration_motion_broadcaster.subscribe(
        "org_other", INTEGRATION_MAX_SSE_SUBSCRIBERS
    ) is not None


# ── Endpoint ────────────────────────────────────────────────────────

def test_motion_stream_requires_key(unauthenticated_client):
    # 401 is raised by the auth dependency before any streaming begins.
    assert unauthenticated_client.get("/api/integration/motion/stream").status_code == 401


def test_motion_stream_429_when_pool_full(unauthenticated_client, db):
    """A valid key still 429s when the org's integration pool is full — the
    429 is raised before the StreamingResponse, so the GET returns cleanly."""
    _make_integration_key(db)
    for _ in range(INTEGRATION_MAX_SSE_SUBSCRIBERS):
        integration_motion_broadcaster.subscribe(ORG, INTEGRATION_MAX_SSE_SUBSCRIBERS)

    resp = unauthenticated_client.get(
        "/api/integration/motion/stream",
        headers={"Authorization": f"Bearer {RAW_KEY}"},
    )
    assert resp.status_code == 429
