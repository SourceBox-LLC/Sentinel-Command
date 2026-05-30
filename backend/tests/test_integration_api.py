"""Phase 2 integration data-plane tests.

Camera discovery (LAN-direct URLs), snapshot, recording toggle, and status —
all authenticated by an ``osi_`` integration key, org-scoped from that key,
with NO plan gate (Home Assistant is available to every tier).
"""

import hashlib
from datetime import UTC, datetime

from app.models.models import Camera, CameraNode, McpApiKey, Setting

ORG = "org_test123"
RAW_KEY = "osi_phase2testkey00000000000000000000"


def _auth(raw: str = RAW_KEY) -> dict:
    return {"Authorization": f"Bearer {raw}"}


def _make_integration_key(db, org: str = ORG, raw: str = RAW_KEY) -> None:
    db.add(McpApiKey(
        org_id=org,
        key_hash=hashlib.sha256(raw.encode()).hexdigest(),
        name="Home Assistant",
        kind="integration",
    ))
    db.commit()


def _seed(db, *, org=ORG, cam_id="cam_ha_0", node_id="node_ha",
          ip="192.168.1.50", node_online=True):
    now = datetime.now(tz=UTC).replace(tzinfo=None)
    node = CameraNode(
        node_id=node_id, org_id=org, name="Front Node", api_key_hash="x" * 64,
        status="online" if node_online else "offline",
        last_seen=now if node_online else None,
        local_ip=ip, http_port=8080, node_version="0.1.69",
        storage_used_bytes=1000, storage_max_bytes=4000,
        storage_disk_free_bytes=5000, storage_disk_total_bytes=10000,
    )
    db.add(node)
    db.flush()
    cam = Camera(
        camera_id=cam_id, org_id=org, node_id=node.id, name="Front Door",
        status="streaming", last_seen=now, video_codec="avc1.42e01e",
        continuous_24_7=False,
    )
    db.add(cam)
    db.commit()
    return node, cam


# ── Auth ────────────────────────────────────────────────────────────

def test_cameras_requires_key(unauthenticated_client):
    assert unauthenticated_client.get("/api/integration/cameras").status_code == 401


def test_cameras_rejects_unknown_key(unauthenticated_client):
    resp = unauthenticated_client.get("/api/integration/cameras", headers=_auth("osi_nope"))
    assert resp.status_code == 401


# ── Discovery ───────────────────────────────────────────────────────

def test_cameras_lists_with_lan_direct_url(unauthenticated_client, db):
    _make_integration_key(db)
    _seed(db, cam_id="cam_ha_0", ip="192.168.1.50")

    resp = unauthenticated_client.get("/api/integration/cameras", headers=_auth())
    assert resp.status_code == 200
    cams = resp.json()["cameras"]
    assert len(cams) == 1
    c = cams[0]
    assert c["id"] == "cam_ha_0"
    assert c["online"] is True
    assert c["node_online"] is True
    assert c["recording"] is False
    assert c["video_codec"] == "avc1.42e01e"
    # LAN-direct URL points straight at the node's own HLS server.
    assert c["stream"]["local_url"] == "http://192.168.1.50:8080/hls/cam_ha_0/stream.m3u8"
    assert c["stream"]["proxy_url"] is None  # off-LAN proxy is Phase 2b
    assert c["snapshot_url"] == "/api/integration/cameras/cam_ha_0/snapshot"


def test_local_url_null_when_node_offline(unauthenticated_client, db):
    _make_integration_key(db)
    _seed(db, cam_id="cam_off", node_id="node_off", node_online=False)

    c = unauthenticated_client.get(
        "/api/integration/cameras", headers=_auth()
    ).json()["cameras"][0]
    assert c["node_online"] is False
    assert c["stream"]["local_url"] is None


def test_cameras_org_scoped(unauthenticated_client, db):
    """A key for org A must not see another org's cameras."""
    _make_integration_key(db, org=ORG)
    _seed(db, org="org_other", cam_id="cam_other", node_id="node_other")

    cams = unauthenticated_client.get(
        "/api/integration/cameras", headers=_auth()
    ).json()["cameras"]
    assert cams == []


# ── Recording toggle ────────────────────────────────────────────────

def test_recording_toggle_sets_continuous(unauthenticated_client, db):
    _make_integration_key(db)
    _seed(db, cam_id="cam_rec")

    resp = unauthenticated_client.post(
        "/api/integration/cameras/cam_rec/recording",
        json={"recording": True}, headers=_auth(),
    )
    assert resp.status_code == 200
    assert resp.json() == {"camera_id": "cam_rec", "recording": True}

    db.expire_all()
    assert db.query(Camera).filter_by(camera_id="cam_rec").one().continuous_24_7 is True


def test_recording_toggle_unknown_camera_404(unauthenticated_client, db):
    _make_integration_key(db)
    resp = unauthenticated_client.post(
        "/api/integration/cameras/ghost/recording",
        json={"recording": True}, headers=_auth(),
    )
    assert resp.status_code == 404


# ── Snapshot ────────────────────────────────────────────────────────

def test_snapshot_unknown_camera_404(unauthenticated_client, db):
    _make_integration_key(db)
    resp = unauthenticated_client.get(
        "/api/integration/cameras/ghost/snapshot", headers=_auth()
    )
    assert resp.status_code == 404


def test_snapshot_success(unauthenticated_client, db, monkeypatch):
    _make_integration_key(db)
    _seed(db, cam_id="cam_snap")

    async def _fake_capture(org_id, camera_id):
        assert org_id == ORG and camera_id == "cam_snap"
        return (b"\xff\xd8\xff_fake_jpeg", "node_ha")

    # The endpoint does a local `from app.mcp.server import ...`, so patching
    # the module attribute is picked up at call time.
    import app.mcp.server as srv
    monkeypatch.setattr(srv, "_capture_snapshot_bytes", _fake_capture)

    resp = unauthenticated_client.get(
        "/api/integration/cameras/cam_snap/snapshot", headers=_auth()
    )
    assert resp.status_code == 200
    assert resp.headers["content-type"] == "image/jpeg"
    assert resp.content == b"\xff\xd8\xff_fake_jpeg"


def test_snapshot_offline_node_503(unauthenticated_client, db, monkeypatch):
    _make_integration_key(db)
    _seed(db, cam_id="cam_snap2")

    async def _raise(org_id, camera_id):
        from fastmcp.exceptions import ToolError
        raise ToolError("Node is offline — cannot capture snapshot")

    import app.mcp.server as srv
    monkeypatch.setattr(srv, "_capture_snapshot_bytes", _raise)

    resp = unauthenticated_client.get(
        "/api/integration/cameras/cam_snap2/snapshot", headers=_auth()
    )
    assert resp.status_code == 503


# ── Status ──────────────────────────────────────────────────────────

def test_status_rollup(unauthenticated_client, db):
    _make_integration_key(db)
    _seed(db, cam_id="cam_a", node_id="node_a")
    # Cache a paid plan so resolve_org_plan hits the fast path (no live
    # Clerk lookup) and the assertion is deterministic.
    Setting.set(db, ORG, "org_plan", "pro")

    resp = unauthenticated_client.get("/api/integration/status", headers=_auth())
    assert resp.status_code == 200
    data = resp.json()
    assert data["org_id"] == ORG
    assert data["plan"] == "pro"
    assert data["cameras"] == {"total": 1, "online": 1}
    assert data["nodes"]["total"] == 1
    assert data["nodes"]["online"] == 1
    item = data["nodes"]["items"][0]
    assert item["node_id"] == "node_a"
    assert item["online"] is True
    assert item["version"] == "0.1.69"
    assert item["storage"]["disk_total_bytes"] == 10000


def test_status_requires_key(unauthenticated_client):
    assert unauthenticated_client.get("/api/integration/status").status_code == 401
