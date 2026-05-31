"""ConnectionManager connection-lifecycle regression tests.

The reconnect race: when a node reconnects to the same process, `connect()`
replaces the old socket with the new one. The OLD socket's receive loop then
exits and calls `disconnect(node_id, old_ws)`. That must NOT evict the new,
live connection — the bug it guards against left a node that kept heartbeating
(DB shows it online) but reported `is_connected() == False`, so every WS
command (snapshot / view / recording) failed until the node reconnected again.
"""

import asyncio

from app.api.ws import ConnectionManager


class _FakeWS:
    """Minimal stand-in: connect() awaits .close() on the replaced socket."""

    def __init__(self, name: str) -> None:
        self.name = name
        self.closed = False

    async def close(self, code: int = 1000, reason: str = "") -> None:
        self.closed = True

    def __repr__(self) -> str:
        return f"_FakeWS({self.name})"


def test_stale_disconnect_does_not_evict_new_connection():
    async def scenario():
        mgr = ConnectionManager()
        ws1, ws2 = _FakeWS("ws1"), _FakeWS("ws2")

        await mgr.connect("node_a", ws1)
        assert mgr.is_connected("node_a")

        # Node reconnects: ws2 replaces ws1 (connect() closes the old one).
        await mgr.connect("node_a", ws2)
        assert ws1.closed is True
        assert mgr._connections["node_a"] is ws2

        # The OLD socket's receive loop now exits and disconnects with ITS ws.
        mgr.disconnect("node_a", ws1)

        # The new connection must survive — this is the regression.
        assert mgr.is_connected("node_a") is True
        assert mgr._connections["node_a"] is ws2

        # When the CURRENT socket disconnects, it IS torn down.
        mgr.disconnect("node_a", ws2)
        assert mgr.is_connected("node_a") is False

    asyncio.run(scenario())


def test_disconnect_without_ws_still_tears_down():
    """Back-compat: disconnect(node_id) with no ws still removes the
    connection (the identity guard only engages when a ws is passed)."""
    async def scenario():
        mgr = ConnectionManager()
        await mgr.connect("node_b", _FakeWS("ws1"))
        mgr.disconnect("node_b")  # legacy call shape, no ws
        assert mgr.is_connected("node_b") is False

    asyncio.run(scenario())


def test_stale_disconnect_preserves_live_connections_pending_commands():
    """A stale (old-socket) disconnect must not cancel the live connection's
    in-flight command futures."""
    async def scenario():
        mgr = ConnectionManager()
        ws1, ws2 = _FakeWS("ws1"), _FakeWS("ws2")
        await mgr.connect("node_c", ws1)
        await mgr.connect("node_c", ws2)

        loop = asyncio.get_running_loop()
        fut = loop.create_future()
        mgr._pending_commands["corr-1"] = ("node_c", fut)

        mgr.disconnect("node_c", ws1)  # stale — early-returns

        assert not fut.cancelled()
        assert "corr-1" in mgr._pending_commands
        fut.cancel()  # tidy up the dangling future

    asyncio.run(scenario())


def test_heartbeat_closes_session_on_node_not_found(monkeypatch):
    """The node-not-found early return in _handle_heartbeat must close its DB
    session, not leak it to GC. Heartbeats fire every ~30s per node forever,
    so a path that skips close() accumulates connections under load."""
    import app.api.ws as ws_mod

    class _FakeQuery:
        def filter_by(self, **kwargs):
            return self

        def filter(self, *args):
            return self

        def first(self):
            return None  # node not found → early return path

    class _FakeSession:
        def __init__(self):
            self.closed = False

        def query(self, *args):
            return _FakeQuery()

        def rollback(self):
            pass

        def close(self):
            self.closed = True

    fake = _FakeSession()
    monkeypatch.setattr(ws_mod, "SessionLocal", lambda: fake)

    asyncio.run(ws_mod._handle_heartbeat("ghost_node", 1, "org_x", {}))
    assert fake.closed is True

