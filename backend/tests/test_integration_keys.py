"""Integration API key tests.

Covers minting / listing / revoking ``osi_`` keys, and — the load-bearing
part — the cross-kind boundary that keeps integration keys (``osi_``) and
MCP keys (``osc_``) from crossing surfaces even though they share the
``mcp_api_keys`` table:

  - an integration key must not authenticate to the MCP tool surface,
  - an MCP key must not authenticate to the integration surface,
  - neither kind leaks into the other's management list / revoke.
"""

import asyncio

import pytest
from fastapi import HTTPException
from fastmcp.exceptions import ToolError
from starlette.requests import Request

from app.core.integration_auth import require_integration_org
from app.mcp.server import _resolve_org

ORG = "org_test123"  # matches the admin_client fixture's org


def _request_with_auth(token: str | None) -> Request:
    headers = []
    if token is not None:
        headers.append((b"authorization", f"Bearer {token}".encode()))
    return Request({"type": "http", "method": "GET", "path": "/", "headers": headers})


def _resolve(token: str | None):
    """Drive the async dependency from a sync test (no running loop here).

    The dependency uses its own short-lived session internally (see
    integration_auth), so we don't pass one — keys created via admin_client
    are committed to the shared in-memory DB and visible to that session.
    """
    return asyncio.run(require_integration_org(_request_with_auth(token)))


# ── CRUD ────────────────────────────────────────────────────────────

def test_create_integration_key(admin_client):
    resp = admin_client.post("/api/integration/keys", json={"name": "Home Assistant"})
    assert resp.status_code == 200
    data = resp.json()
    assert data["name"] == "Home Assistant"
    assert data["key"].startswith("osi_")
    assert len(data["key"]) == 36  # "osi_" + 32 hex chars
    assert data["kind"] == "integration"
    assert "warning" in data


def test_create_defaults_name(admin_client):
    resp = admin_client.post("/api/integration/keys", json={})
    assert resp.status_code == 200
    assert resp.json()["name"] == "Home Assistant"


def test_list_integration_keys(admin_client):
    admin_client.post("/api/integration/keys", json={"name": "K1"})
    admin_client.post("/api/integration/keys", json={"name": "K2"})
    keys = admin_client.get("/api/integration/keys").json()
    assert len(keys) == 2
    for k in keys:
        assert "key" not in k and "key_hash" not in k  # never leak the secret
        assert k["kind"] == "integration"


def test_revoke_integration_key(admin_client):
    kid = admin_client.post("/api/integration/keys", json={"name": "Temp"}).json()["id"]
    assert admin_client.delete(f"/api/integration/keys/{kid}").status_code == 200
    assert admin_client.get("/api/integration/keys").json() == []


def test_revoke_nonexistent_key(admin_client):
    assert admin_client.delete("/api/integration/keys/999999").status_code == 404


# ── Cross-kind separation on the management surfaces ────────────────

def test_kinds_do_not_cross_in_lists(admin_client):
    """An osi_ key must not appear on the MCP keys page, nor an osc_ key on
    the integration page — they share a table but not a surface."""
    admin_client.post("/api/integration/keys", json={"name": "HA"})
    admin_client.post("/api/mcp/keys", json={"name": "Agent"})

    assert [k["name"] for k in admin_client.get("/api/mcp/keys").json()] == ["Agent"]
    assert [k["name"] for k in admin_client.get("/api/integration/keys").json()] == ["HA"]


def test_cross_kind_revoke_404s(admin_client):
    """Revoking an MCP key via the integration endpoint (or vice versa)
    must 404 — the kind filter keeps the id-spaces from crossing."""
    mcp_id = admin_client.post("/api/mcp/keys", json={"name": "Agent"}).json()["id"]
    integ_id = admin_client.post("/api/integration/keys", json={"name": "HA"}).json()["id"]

    assert admin_client.delete(f"/api/integration/keys/{mcp_id}").status_code == 404
    assert admin_client.delete(f"/api/mcp/keys/{integ_id}").status_code == 404
    # Both survive.
    assert len(admin_client.get("/api/mcp/keys").json()) == 1
    assert len(admin_client.get("/api/integration/keys").json()) == 1


# ── require_integration_org dependency ──────────────────────────────

def test_dependency_resolves_org(admin_client):
    raw = admin_client.post("/api/integration/keys", json={"name": "HA"}).json()["key"]
    user = _resolve(raw)
    assert user.org_id == ORG
    assert user.is_admin is False  # integration role is not admin


def test_dependency_rejects_missing_header():
    with pytest.raises(HTTPException) as ei:
        _resolve(None)
    assert ei.value.status_code == 401


def test_dependency_rejects_unknown_key():
    with pytest.raises(HTTPException) as ei:
        _resolve("osi_deadbeefdeadbeefdeadbeefdeadbeef")
    assert ei.value.status_code == 401


def test_dependency_rejects_revoked_key(admin_client):
    created = admin_client.post("/api/integration/keys", json={"name": "HA"}).json()
    admin_client.delete(f"/api/integration/keys/{created['id']}")
    with pytest.raises(HTTPException) as ei:
        _resolve(created["key"])
    assert ei.value.status_code == 401


# ── Cross-auth boundary (the security guard) ────────────────────────

def test_integration_key_rejected_by_mcp_auth(admin_client):
    """An integration key (osi_) must NOT authenticate to the MCP tool
    surface — _resolve_org filters kind='mcp'."""
    raw = admin_client.post("/api/integration/keys", json={"name": "HA"}).json()["key"]
    with pytest.raises(ToolError):
        _resolve_org({"authorization": f"Bearer {raw}"})


def test_mcp_key_rejected_by_integration_auth(admin_client):
    """An MCP key (osc_) must NOT authenticate to the integration surface —
    require_integration_org filters kind='integration'."""
    raw = admin_client.post("/api/mcp/keys", json={"name": "Agent"}).json()["key"]
    with pytest.raises(HTTPException) as ei:
        _resolve(raw)
    assert ei.value.status_code == 401
