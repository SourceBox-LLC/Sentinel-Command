"""WebSocket auth credential resolution tests.

Pins the behaviour the v0.1.65 security fix introduced:

  - Preferred path: credentials in HTTP-upgrade headers
    (`X-Node-API-Key` / `X-Node-Id`).  URLs end up in many more log
    sinks than headers do (uvicorn access log, Fly platform access
    log, log-shipping pipeline export, browser referer chains), and
    headers don't.

  - Back-compat path: credentials in URL query string
    (`?api_key=…&node_id=…`).  Still accepted so pre-v0.1.65 CloudNode
    binaries keep working without a forced upgrade.  Logged as a
    deprecation warning each time so we can sunset the path once
    the install base has rolled forward.

If either path stops working, a CloudNode silently loses its WS
channel and the operator sees "node offline" without an obvious
cause — these tests catch that regression.
"""

import hashlib

import pytest

from app.core.database import SessionLocal
from app.models import CameraNode
from tests.conftest import TestSession


# ── Fixtures ────────────────────────────────────────────────────────


@pytest.fixture
def node_credentials():
    """Insert a CameraNode row + return the matching plaintext api_key + node_id.

    The DB stores the SHA-256 hash of the key; the WS handler computes
    the hash of whatever the client sent and compares.  We seed a known
    plaintext so we can drive the WS connect with it.
    """
    api_key = "nak_test_ws_auth_abc123"
    node_id = "nd_ws_test_001"
    org_id = "org_ws_test"

    session = TestSession()
    try:
        node = CameraNode(
            node_id=node_id,
            org_id=org_id,
            api_key_hash=hashlib.sha256(api_key.encode()).hexdigest(),
            name="test ws node",
            status="offline",
        )
        session.add(node)
        session.commit()
    finally:
        session.close()

    return {"api_key": api_key, "node_id": node_id, "org_id": org_id}


# ── Header path (the v0.1.65 default) ───────────────────────────────


def test_ws_auth_succeeds_via_headers(unauthenticated_client, node_credentials):
    """Preferred path: credentials in X-Node-API-Key + X-Node-Id headers.

    Verifies (a) the connect handshake succeeds, (b) the auth-failed
    paths (4001) and rate-limit paths (1013) don't fire.  Send a single
    well-formed heartbeat just to confirm we made it past auth into the
    inner message loop — if auth had failed, the WS would already be
    closed before we could send.
    """
    creds = node_credentials
    with unauthenticated_client.websocket_connect(
        "/ws/node",
        headers={
            "X-Node-API-Key": creds["api_key"],
            "X-Node-Id": creds["node_id"],
        },
    ) as ws:
        # Heartbeat doesn't need a real camera list; an empty payload
        # is fine for the auth check.
        ws.send_json({
            "type": "heartbeat",
            "id": "test-correlation",
            "payload": {"cameras": []},
        })
        ack = ws.receive_json()
        assert ack["type"] == "ack"
        assert ack["id"] == "test-correlation"


def test_ws_auth_rejects_bad_key_in_headers(unauthenticated_client, node_credentials):
    """Wrong api_key in header → 4001 close.

    The 4001 code is the app-specific "invalid credentials" signal we
    use across the WS endpoint — keep it stable so CloudNode's
    reconnect logic can branch on it (vs the 1013 throttle code).
    """
    from starlette.websockets import WebSocketDisconnect

    with pytest.raises(WebSocketDisconnect) as excinfo:
        with unauthenticated_client.websocket_connect(
            "/ws/node",
            headers={
                "X-Node-API-Key": "nak_wrong_key",
                "X-Node-Id": node_credentials["node_id"],
            },
        ) as ws:
            ws.receive_json()  # never arrives — close already happened
    assert excinfo.value.code == 4001


# ── Back-compat query-string path (pre-v0.1.65 nodes) ───────────────


def test_ws_auth_succeeds_via_query_string(unauthenticated_client, node_credentials, caplog):
    """Back-compat: credentials in the URL still authenticate.

    Pre-v0.1.65 CloudNode clients send `?api_key=…&node_id=…`.  The
    backend must keep accepting this OR every existing install breaks
    silently when this fix deploys.  Once we're confident the install
    base is past v0.1.65 we can flip this to expect a refusal; until
    then the path stays alive but logs a deprecation warning.
    """
    creds = node_credentials
    url = (
        f"/ws/node?api_key={creds['api_key']}&node_id={creds['node_id']}"
    )
    with caplog.at_level("WARNING", logger="app.api.ws"):
        with unauthenticated_client.websocket_connect(url) as ws:
            ws.send_json({
                "type": "heartbeat",
                "payload": {"cameras": []},
            })
            ack = ws.receive_json()
            assert ack["type"] == "ack"

    # Deprecation warning fires AFTER auth so invalid-cred probes don't
    # spam the log; pin the warning content so a future refactor that
    # silently drops it (and the sunset signal it provides) gets caught.
    assert any(
        "deprecated query-string" in record.message
        for record in caplog.records
    ), f"expected deprecation warning, got {[r.message for r in caplog.records]}"


def test_ws_auth_rejects_when_both_paths_missing(unauthenticated_client):
    """No credentials in either headers or query string → 4001.

    Same close code as 'wrong key' on purpose — a probe trying to
    detect 'is auth required' should get an indistinguishable response
    from 'auth required and you got it wrong'.
    """
    from starlette.websockets import WebSocketDisconnect

    with pytest.raises(WebSocketDisconnect) as excinfo:
        with unauthenticated_client.websocket_connect("/ws/node") as ws:
            ws.receive_json()
    assert excinfo.value.code == 4001


def test_ws_auth_header_overrides_query_string(unauthenticated_client, node_credentials):
    """When BOTH headers and query string are present, headers win.

    This is the upgrade-window safety property: imagine a CloudNode
    that still has the old URL builder running alongside the new
    header builder for some transitional reason.  The handler should
    treat the header as authoritative so we don't accidentally
    authenticate against the (potentially wrong, stale, leaked) URL
    credential.
    """
    creds = node_credentials
    # Query string is intentionally WRONG; headers are correct.
    url = "/ws/node?api_key=nak_garbage_value&node_id=nd_wrong"
    with unauthenticated_client.websocket_connect(
        url,
        headers={
            "X-Node-API-Key": creds["api_key"],
            "X-Node-Id": creds["node_id"],
        },
    ) as ws:
        ws.send_json({"type": "heartbeat", "payload": {"cameras": []}})
        ack = ws.receive_json()
        assert ack["type"] == "ack"
