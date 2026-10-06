"""Streaming protocols through the proxy: WebSocket and Server-Sent Events.

Neither fits the request/response differential, and both were broken by
the proxy in ways nothing else would have caught:

* `/ws/node` is how every CameraNode connects. `Connection` and `Upgrade`
  are hop-by-hop headers, so the proxy stripped them — correct for an
  ordinary request — and Python saw a plain GET to a WebSocket-only route
  and answered 404. The entire node fleet would have failed to connect.
* `/api/motion/events/stream` and the Home Assistant integration are SSE.
  The proxy collected the response body before returning it, so an
  endpoint that never ends never responded. Measured: Python emitted its
  first event immediately, Rust emitted nothing for the full timeout.

Both now work, and this pins them.

Usage: streaming_diff.py            (exits non-zero on a difference)
"""

import asyncio
import json
import sys

import websockets

NODE_HEADERS = {"X-Node-API-Key": "test-node-key", "X-Node-Id": "node-aaaa1111"}
STACKS = [("python", 8001), ("rust", 8000)]


async def websocket_probe(port):
    """Open an authenticated node socket and round-trip one message."""
    url = f"ws://127.0.0.1:{port}/ws/node"
    try:
        ws = await asyncio.wait_for(
            websockets.connect(url, additional_headers=NODE_HEADERS, open_timeout=10),
            timeout=12,
        )
    except Exception as e:  # noqa: BLE001
        return {"handshake": f"FAILED {type(e).__name__}", "ack": None}
    try:
        await ws.send(json.dumps({"type": "heartbeat", "cameras": []}))
        reply = json.loads(await asyncio.wait_for(ws.recv(), timeout=10))
        # The timestamp moves between runs; the shape is what matters.
        return {"handshake": "101", "ack": reply.get("type"),
                "payload_keys": sorted(reply.get("payload", {}).keys())}
    except Exception as e:  # noqa: BLE001
        return {"handshake": "101", "ack": f"FAILED {type(e).__name__}"}
    finally:
        await ws.close()


async def sse_probe(port, token):
    """Read the first SSE event, with a deadline well under the stream's life."""
    reader = writer = None
    try:
        reader, writer = await asyncio.wait_for(
            asyncio.open_connection("127.0.0.1", port), timeout=5)
        req = (
            f"GET /api/motion/events/stream HTTP/1.1\r\n"
            f"Host: 127.0.0.1:{port}\r\n"
            f"Authorization: Bearer {token}\r\n"
            f"Accept: text/event-stream\r\n"
            f"Connection: close\r\n\r\n"
        )
        writer.write(req.encode())
        await writer.drain()

        # A buffering proxy fails right here: it holds everything until
        # the stream ends, and this stream does not end. Read until the
        # first event rather than a fixed byte count — the headers alone
        # can fill a small buffer, which made an earlier version of this
        # probe report "no event" for both stacks and prove nothing.
        text = ""
        first_event = None
        deadline = asyncio.get_running_loop().time() + 6
        while first_event is None:
            remaining = deadline - asyncio.get_running_loop().time()
            if remaining <= 0:
                break
            chunk = await asyncio.wait_for(reader.read(4096), timeout=remaining)
            if not chunk:
                break
            text += chunk.decode("utf-8", "replace")
            first_event = next(
                (ln for ln in text.splitlines() if ln.startswith("data:")), None)
        status = text.split("\r\n", 1)[0] if text else "<nothing>"
        return {"status": status, "first_event_seen": first_event is not None,
                "event_type": json.loads(first_event[5:]).get("type")
                if first_event else None}
    except asyncio.TimeoutError:
        return {"status": "<timeout>", "first_event_seen": False, "event_type": None}
    except Exception as e:  # noqa: BLE001
        return {"status": f"FAILED {type(e).__name__}", "first_event_seen": False,
                "event_type": None}
    finally:
        if writer is not None:
            writer.close()


async def main():
    token = sys.argv[1]
    bad = 0

    for label, probe in [("websocket /ws/node", websocket_probe),
                         ("sse /api/motion/events/stream", None)]:
        results = {}
        for name, port in STACKS:
            results[name] = (await probe(port)) if probe else (await sse_probe(port, token))
        same = results["python"] == results["rust"]
        mark = "ok    " if same else "DIFFER"
        print(f"  {mark}  {label}")
        for name in ("python", "rust"):
            print(f"            {name:<7} {json.dumps(results[name], sort_keys=True)}")
        if not same:
            bad += 1

    # A run where the handshake never succeeded proves nothing: both
    # stacks failing identically would otherwise read as a pass.
    ws_rust = await websocket_probe(8000)
    if ws_rust.get("handshake") != "101":
        print("\nCOVERAGE: no successful WebSocket handshake against Rust, so the "
              "comparison above proves nothing.\n"
              "  Two causes look identical here: the proxy is not tunnelling the "
              "upgrade (check that\n"
              "  Connection/Upgrade survive proxy.rs), or the fixture is stale "
              "(node-aaaa1111 must\n"
              "  carry sha256('test-node-key')). Check the Python log for a "
              "[WS] line to tell them apart.")
        return 2
    sse_rust = await sse_probe(8000, token)
    if not sse_rust["first_event_seen"]:
        print("\nCOVERAGE: no SSE event arrived from Rust within the deadline, so the "
              "comparison above proves nothing.\n"
              "  The usual cause is the proxy collecting the response body instead of "
              "streaming it —\n"
              "  a stream that never ends then never responds.")
        return 2

    print(f"\n{2 - bad}/2 streaming protocols identical")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
