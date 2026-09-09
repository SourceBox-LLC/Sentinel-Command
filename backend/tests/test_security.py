"""Tests for the SaaS security review fixes:

- H1: ``POST /api/cameras/{id}/recording`` requires admin
- M2: node-push endpoints (push-segment / playlist / motion) reject
      cameras owned by a different node even when camera_id is
      globally unique
- M3: mutating admin endpoints write AuditLog rows
"""

import hashlib
import uuid

from app.models.models import AuditLog, Camera, CameraNode


def _fresh_query(model, **filters):
    """Query a fresh session.  The ``db`` fixture session sees stale cache
    after an endpoint commits on a separate (override_get_db) session, so
    for post-endpoint DB assertions we open a new session each time."""
    from tests.conftest import TestSession

    s = TestSession()
    try:
        return s.query(model).filter_by(**filters).all()
    finally:
        s.close()


# ── M3: Audit log writes ─────────────────────────────────────────────

def test_audit_log_written_on_mcp_key_create(admin_client, db):
    """Creating an MCP key persists an audit row."""
    resp = admin_client.post("/api/mcp/keys", json={"name": "AuditMe"})
    assert resp.status_code == 200

    logs = _fresh_query(AuditLog, org_id="org_test123", event="mcp_key_created")
    assert len(logs) == 1
    assert logs[0].username == "admin@test.com"
    assert "AuditMe" in (logs[0].details or "")


def test_audit_log_written_on_mcp_key_revoke(admin_client, db):
    """Revoking an MCP key persists an audit row with the key name."""
    create = admin_client.post("/api/mcp/keys", json={"name": "Revokee"})
    key_id = create.json()["id"]

    resp = admin_client.delete(f"/api/mcp/keys/{key_id}")
    assert resp.status_code == 200

    logs = _fresh_query(AuditLog, org_id="org_test123", event="mcp_key_revoked")
    assert len(logs) == 1
    assert "Revokee" in (logs[0].details or "")


def test_audit_log_written_on_node_create(admin_client, db):
    """Creating a node persists an audit row."""
    resp = admin_client.post("/api/nodes", json={"name": "AuditNode"})
    assert resp.status_code == 200

    logs = _fresh_query(AuditLog, org_id="org_test123", event="node_created")
    assert len(logs) == 1
    assert "AuditNode" in (logs[0].details or "")


def test_audit_log_written_on_node_delete(admin_client, db):
    """Deleting a node persists an audit row."""
    create = admin_client.post("/api/nodes", json={"name": "DeleteMe"})
    node_id = create.json()["node_id"]

    resp = admin_client.delete(f"/api/nodes/{node_id}")
    assert resp.status_code == 200

    logs = _fresh_query(AuditLog, org_id="org_test123", event="node_deleted")
    assert len(logs) == 1
    assert "DeleteMe" in (logs[0].details or "")


def test_audit_log_written_on_node_key_rotate(admin_client, db):
    """Rotating a node key persists an audit row."""
    create = admin_client.post("/api/nodes", json={"name": "Rotatable"})
    node_id = create.json()["node_id"]

    resp = admin_client.post(f"/api/nodes/{node_id}/rotate-key")
    assert resp.status_code == 200

    logs = _fresh_query(AuditLog, org_id="org_test123", event="node_key_rotated")
    assert len(logs) == 1
    assert "Rotatable" in (logs[0].details or "")


def test_audit_log_written_on_camera_recording_policy_update(admin_client, db):
    """Updating a camera's recording policy persists an audit row.

    The org-level `/api/settings/recording` endpoint was retired in
    v0.1.43; this exercises the per-camera replacement.
    """
    from app.models.models import Camera, CameraNode
    # Seed a node + camera so the PATCH has something to target.
    node = CameraNode(
        node_id="nd_audit_rec", org_id="org_test123",
        api_key_hash="x" * 64, name="audit-test",
    )
    db.add(node)
    db.flush()
    db.add(Camera(
        camera_id="cam_audit_rec", org_id="org_test123",
        node_id=node.id, name="audit-cam",
    ))
    db.commit()

    resp = admin_client.patch(
        "/api/cameras/cam_audit_rec/recording-settings",
        json={"continuous_24_7": True},
    )
    assert resp.status_code == 200

    logs = _fresh_query(
        AuditLog, org_id="org_test123", event="camera_recording_policy_updated",
    )
    assert len(logs) == 1


# ── H1: toggle_recording requires admin ──────────────────────────────

def test_toggle_recording_rejects_viewer(viewer_client, db):
    """A viewer must NOT be able to start/stop recording — this is an
    operational change that should be admin-only (H1)."""
    # Seed a camera so the endpoint can't 404 before the auth check.
    node = CameraNode(
        node_id="node_for_toggle",
        org_id="org_test123",
        api_key_hash="x",
        name="n",
    )
    db.add(node)
    db.commit()
    db.refresh(node)
    db.add(Camera(
        camera_id="cam_toggle", org_id="org_test123",
        node_id=node.id, name="Cam",
    ))
    db.commit()

    resp = viewer_client.post(
        "/api/cameras/cam_toggle/recording",
        json={"recording": True},
    )
    # require_admin raises 403 for non-admins.
    assert resp.status_code == 403


# ── M2: node-push endpoints enforce node ownership ───────────────────
#
# These tests seed two nodes in the same org and verify that node A
# cannot manipulate a camera belonging to node B even though
# ``camera_id`` is globally unique.  The explicit ``node_id`` +
# ``org_id`` filters in each endpoint close that defense-in-depth gap.

def _seed_two_nodes(db, org_id="org_test123"):
    """Create two nodes each with its own camera.  Returns
    ``(node_a_key, node_b_key, cam_a_id, cam_b_id)``."""
    key_a = "raw-key-a-" + uuid.uuid4().hex
    key_b = "raw-key-b-" + uuid.uuid4().hex
    node_a = CameraNode(
        node_id="node_a",
        org_id=org_id,
        api_key_hash=hashlib.sha256(key_a.encode()).hexdigest(),
        name="NodeA",
    )
    node_b = CameraNode(
        node_id="node_b",
        org_id=org_id,
        api_key_hash=hashlib.sha256(key_b.encode()).hexdigest(),
        name="NodeB",
    )
    db.add_all([node_a, node_b])
    db.commit()
    db.refresh(node_a)
    db.refresh(node_b)
    db.add(Camera(camera_id="cam_a", org_id=org_id, node_id=node_a.id, name="CamA"))
    db.add(Camera(camera_id="cam_b", org_id=org_id, node_id=node_b.id, name="CamB"))
    db.commit()
    return key_a, key_b, "cam_a", "cam_b"


def test_push_segment_rejects_wrong_node(unauthenticated_client, db):
    """Node A cannot push a segment for Node B's camera (M2)."""
    key_a, _key_b, _cam_a, cam_b = _seed_two_nodes(db)

    resp = unauthenticated_client.post(
        f"/api/cameras/{cam_b}/push-segment?filename=segment_00001.ts",
        content=b"\x00\x01\x02",
        headers={"X-Node-API-Key": key_a},
    )
    assert resp.status_code == 404


def test_playlist_rejects_wrong_node(unauthenticated_client, db):
    """Node A cannot update playlist for Node B's camera (M2)."""
    key_a, _key_b, _cam_a, cam_b = _seed_two_nodes(db)

    resp = unauthenticated_client.post(
        f"/api/cameras/{cam_b}/playlist",
        content=b"#EXTM3U\n#EXT-X-VERSION:3\nsegment_00001.ts\n",
        headers={"X-Node-API-Key": key_a},
    )
    assert resp.status_code == 404


def test_motion_push_rejects_wrong_node(unauthenticated_client, db):
    """Node A cannot push a motion event for Node B's camera (M2)."""
    key_a, _key_b, _cam_a, cam_b = _seed_two_nodes(db)

    resp = unauthenticated_client.post(
        f"/api/cameras/{cam_b}/motion",
        json={"score": 75, "segment_seq": 5},
        headers={"X-Node-API-Key": key_a},
    )
    assert resp.status_code == 404


def test_codec_report_rejects_wrong_node(unauthenticated_client, db):
    """Node A cannot report codec for Node B's camera (M2)."""
    key_a, _key_b, _cam_a, cam_b = _seed_two_nodes(db)

    resp = unauthenticated_client.post(
        f"/api/cameras/{cam_b}/codec",
        json={"video_codec": "avc1.42e01e", "audio_codec": "mp4a.40.2"},
        headers={"X-Node-API-Key": key_a},
    )
    assert resp.status_code == 404


def test_push_segment_accepts_correct_node(unauthenticated_client, db):
    """The owning node CAN push a segment for its own camera —
    sanity check so the M2 test isn't a false pass."""
    key_a, _key_b, cam_a, _cam_b = _seed_two_nodes(db)

    resp = unauthenticated_client.post(
        f"/api/cameras/{cam_a}/push-segment?filename=segment_00001.ts",
        content=b"\x00\x01\x02",
        headers={"X-Node-API-Key": key_a},
    )
    assert resp.status_code == 200
    assert resp.json()["success"] is True


# ── L4: FRONTEND_URL validation ──────────────────────────────────────

def test_frontend_url_validator_rejects_missing_scheme():
    from app.main import _validate_frontend_url
    assert _validate_frontend_url("example.com") is None


def test_frontend_url_validator_rejects_whitespace():
    from app.main import _validate_frontend_url
    assert _validate_frontend_url("https://a.com b.com") is None


def test_frontend_url_validator_rejects_comma_list():
    from app.main import _validate_frontend_url
    assert _validate_frontend_url("https://a.com,https://b.com") is None


def test_frontend_url_validator_strips_trailing_slash():
    from app.main import _validate_frontend_url
    assert _validate_frontend_url("https://a.com/") == "https://a.com"


def test_frontend_url_validator_accepts_clean_url():
    from app.main import _validate_frontend_url
    assert _validate_frontend_url("https://opensentry.example.com") == "https://opensentry.example.com"


# ── Rate limiter bucket key ──────────────────────────────────────────

def test_tenant_aware_key_buckets_by_node_header():
    """X-Node-API-Key buckets per-node and is stable for a given key."""
    from unittest.mock import MagicMock

    from app.core.limiter import tenant_aware_key

    req = MagicMock()
    req.headers = {"X-Node-API-Key": "sekret-key-abc"}
    key1 = tenant_aware_key(req)
    assert key1.startswith("node:")

    # Same key → same bucket.
    req2 = MagicMock()
    req2.headers = {"X-Node-API-Key": "sekret-key-abc"}
    assert tenant_aware_key(req2) == key1

    # Different key → different bucket.
    req3 = MagicMock()
    req3.headers = {"X-Node-API-Key": "different-key-xyz"}
    assert tenant_aware_key(req3) != key1


def test_tenant_aware_key_uses_org_from_jwt():
    """A Bearer JWT with an org_id claim buckets by that org."""
    import base64
    import json
    from unittest.mock import MagicMock

    from app.core.limiter import tenant_aware_key

    # Build a minimal V1 JWT: header.payload.sig — only the payload matters.
    payload = base64.urlsafe_b64encode(
        json.dumps({"org_id": "org_via_jwt"}).encode()
    ).decode().rstrip("=")
    token = f"aGVhZGVy.{payload}.c2ln"

    req = MagicMock()
    req.headers = {"Authorization": f"Bearer {token}"}
    assert tenant_aware_key(req) == "org:org_via_jwt"


def test_tenant_aware_key_handles_v2_compact_claim():
    """A Bearer JWT with the V2 compact ``o.id`` claim also buckets by org."""
    import base64
    import json
    from unittest.mock import MagicMock

    from app.core.limiter import tenant_aware_key

    payload = base64.urlsafe_b64encode(
        json.dumps({"o": {"id": "org_v2_shape"}}).encode()
    ).decode().rstrip("=")
    token = f"aGVhZGVy.{payload}.c2ln"

    req = MagicMock()
    req.headers = {"Authorization": f"Bearer {token}"}
    assert tenant_aware_key(req) == "org:org_v2_shape"


# ── API schema exposure ──────────────────────────────────────────────


def test_api_docs_default_to_off_in_production():
    """FastAPI's docs must not be public on a deployed instance.

    They were: /api-docs, /api-redoc and /api/openapi.json all returned
    200 in production, serving 85 routes and 20 request/response schemas
    to anyone who asked. Not a vulnerability on its own — every
    sensitive route still 401s, which was verified separately — but a
    free map of the attack surface, including the webhook, MCP, admin
    and agent-key endpoints.

    The default is derived from FLY_APP_NAME rather than hard-coded, so
    that neither environment needs configuring. This pins that
    derivation: getting it backwards would silently republish the schema
    on the next deploy, and nothing else would notice.
    """
    import importlib
    import os

    from app.core import config as config_module

    def _reload_with(env: dict) -> bool:
        saved = {k: os.environ.get(k) for k in ("FLY_APP_NAME", "API_DOCS_ENABLED")}
        try:
            for k in saved:
                os.environ.pop(k, None)
            os.environ.update(env)
            return importlib.reload(config_module).settings.API_DOCS_ENABLED
        finally:
            for k, v in saved.items():
                os.environ.pop(k, None)
                if v is not None:
                    os.environ[k] = v
            importlib.reload(config_module)

    # On Fly (production) → off, with no configuration required.
    assert _reload_with({"FLY_APP_NAME": "sentinel-command"}) is False

    # Local dev → on, also with no configuration required.
    assert _reload_with({}) is True

    # Explicit override wins in both directions, so a deploy can be
    # debugged without editing code.
    assert _reload_with({"FLY_APP_NAME": "x", "API_DOCS_ENABLED": "true"}) is True
    assert _reload_with({"API_DOCS_ENABLED": "false"}) is False
