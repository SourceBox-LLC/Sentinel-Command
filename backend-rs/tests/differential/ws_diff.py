#!/usr/bin/env python3
"""The CameraNode socket, which no request/response differential can see.

`/ws/node` is not a route that answers and finishes. It is a handshake,
then a conversation, and the parts worth comparing are spread across
both: how a bad credential is refused, what a heartbeat is answered
with, what an unknown frame gets back, and whether the rows a heartbeat
writes are the same rows.

Writing this found a real difference on its first run. Starlette's
`websocket.close(code=…)` *before* `accept()` does not upgrade and then
send a close frame — there is no connection yet, so uvicorn answers the
HTTP request with a bare **403** and the code is never transmitted. The
port upgraded first and closed with 4001, so a client saw 101 where it
should have seen 403. Nothing else in the tree would have noticed.

One thing is deliberately NOT compared: the headers on that 403. Rust's
rejection goes through the same tower stack as every other response and
so carries the security set and a request id; Python's is answered by
uvicorn below the ASGI app, and carries neither. Having them is the
point of that middleware, so this is a divergence worth keeping rather
than a defect to match.

Usage: ws_diff.py <node_key> [-v]
"""
from __future__ import annotations

import asyncio
import json
import pathlib
import socket
import subprocess
import sys

import websockets

HERE = pathlib.Path(__file__).resolve().parent
PG_CONTAINER = "cc-schema-test"

# The rows a heartbeat touches. Compared as a set, because the ack
# alone says nothing about whether the write landed — and the write is
# the whole point of a heartbeat.
WATCHED = ("camera_nodes", "cameras", "notifications")

PORTS = {"python": 8001, "rust": 8000}

# The node the fixture gives a real key hash to.
NODE = "node-aaaa1111"

# A node id that does not exist, for the cases that only need to reach
# the connect throttle — which runs *before* authentication, so it
# spends a slot for a rejected attempt too. Keeping those attempts off
# the real node leaves its ten-per-minute budget for the functional
# cases, which matters because the throttle is in memory and a tier
# restart is the only thing that clears it.
THROTTLE_NODE = "node-throttle-probe"

VERBOSE = "-v" in sys.argv


def psql(sql, *, stdin=None):
    argv = ["docker", "exec", "-i", PG_CONTAINER, "psql", "-U", "cc", "-d", "cc"]
    argv += ["-q"] if stdin else ["-tAq", "-c", sql]
    done = subprocess.run(argv, input=stdin, capture_output=True, text=True,
                          check=True, timeout=120)
    return done.stdout.strip()


def reseed():
    """Back to the fixture, so each tier's heartbeat starts level."""
    psql("", stdin=(HERE / "seed_cameras.sql").read_text())


def rows():
    """The watched tables, with the columns a clock moves stripped.

    `last_seen`, `version_checked_at` and `created_at` are written to
    "now" by the heartbeat itself, so they differ between the two runs
    by however long the first took — which is a stopwatch, not a port.
    """
    parts = ", ".join(
        f"'{t}', COALESCE((SELECT json_agg(x) FROM "
        f"(SELECT * FROM {t} ORDER BY 1) x), '[]'::json)"
        for t in WATCHED
    )
    data = json.loads(psql(f"SELECT json_build_object({parts})"))
    moving = {"last_seen", "version_checked_at", "created_at", "updated_at", "timestamp"}
    return {
        table: [
            {k: ("<moves>" if k in moving and v is not None else v) for k, v in row.items()}
            for row in (rows or [])
        ]
        for table, rows in data.items()
    }


def compare(label, values, results):
    same = values["rust"] == values["python"]
    results.append((label, same))
    if same:
        if VERBOSE:
            print(f"  same      {label}")
            print(f"            {values['rust']!r}")
    else:
        print(f"  DIFFERENT {label}")
        print(f"            rust  : {values['rust']!r}")
        print(f"            python: {values['python']!r}")


def handshake_status(port, query):
    """The status line of a refused upgrade, spoken by hand.

    A WebSocket client library raises on a refusal and hides what came
    back, and the status is the whole point of these cases.
    """
    sock = socket.create_connection(("127.0.0.1", port), timeout=5)
    sock.sendall(
        f"GET /ws/node{query} HTTP/1.1\r\n"
        f"Host: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
        f"Sec-WebSocket-Version: 13\r\n\r\n".encode()
    )
    sock.settimeout(3)
    data = b""
    try:
        while b"\r\n\r\n" not in data:
            chunk = sock.recv(4096)
            if not chunk:
                break
            data += chunk
    except socket.timeout:
        pass
    sock.close()
    return data.split(b"\r\n", 1)[0].decode("latin-1")


async def converse(port, key, node, frames, headers=True):
    """Open a socket, send each frame, collect each reply."""
    if headers:
        url = f"ws://127.0.0.1:{port}/ws/node"
        extra = {"X-Node-API-Key": key, "X-Node-Id": node}
    else:
        url = f"ws://127.0.0.1:{port}/ws/node?api_key={key}&node_id={node}"
        extra = {}
    replies = []
    try:
        async with websockets.connect(
            url, additional_headers=extra, open_timeout=5
        ) as ws:
            for frame in frames:
                await ws.send(json.dumps(frame))
                try:
                    reply = json.loads(await asyncio.wait_for(ws.recv(), 5))
                except TimeoutError:
                    replies.append("<no reply>")
                    continue
                # The ack carries a server clock reading, which moves.
                if isinstance(reply.get("payload"), dict):
                    reply["payload"].pop("timestamp", None)
                replies.append(reply)
    except Exception as exc:  # noqa: BLE001 — the refusal is the result
        return {"refused": type(exc).__name__}
    return {"replies": replies}


async def both(key, node, frames, headers=True):
    return {
        name: await converse(port, key, node, frames, headers)
        for name, port in PORTS.items()
    }


async def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    key = sys.argv[1]
    results: list[tuple[str, bool]] = []

    print("the handshake")
    for label, query in [
        ("no credentials", ""),
        ("only a node id", f"?node_id={NODE}"),
        ("only a key", f"?api_key={key}"),
        ("a wrong key", f"?api_key=nope&node_id={NODE}"),
        ("an unknown node", f"?api_key={key}&node_id=node-does-not-exist"),
    ]:
        compare(
            f"refused: {label}",
            {n: handshake_status(p, query) for n, p in PORTS.items()},
            results,
        )

    print("a heartbeat")
    heartbeat = {
        "type": "heartbeat",
        "id": "hb-1",
        "payload": {
            "node_version": "0.1.0",
            "cameras": [
                {"camera_id": "cam-live", "status": "online"},
                # A camera in a failed state records its reason; a
                # healthy one clears whatever was there.
                {"camera_id": "cam-stale", "status": "failed",
                 "last_error": "ffmpeg exited 1"},
                # Not this node's camera: the update must skip it.
                {"camera_id": "cam-theirs", "status": "offline"},
                # No camera_id at all.
                {"status": "online"},
            ],
        },
    }
    # Each tier gets the same fixture, so the rows can be compared as
    # well as the ack.
    acks, effects = {}, {}
    for name, port in PORTS.items():
        reseed()
        acks[name] = await converse(port, key, NODE, [heartbeat])
        effects[name] = rows()
    compare("ack, via headers", acks, results)
    for table in WATCHED:
        compare(
            f"rows written to {table}",
            {n: json.dumps(e[table], sort_keys=True) for n, e in effects.items()},
            results,
        )

    compare(
        "ack, via the deprecated query string",
        await both(key, NODE, [heartbeat], headers=False),
        results,
    )

    print("the frame types")
    # All of these on ONE socket per tier, not one each. The connect
    # throttle allows ten per node per minute and only a tier restart
    # clears it, so a harness that opened a socket per case could not
    # be run twice in a minute without refusing itself — which is
    # exactly what happened when it did.
    compare(
        "a conversation of odd frames",
        await both(key, NODE, [
            {"type": "nonsense", "id": "x"},
            # Not a string: Python stringifies whatever it got, so this
            # comes back as "5" and not as "None".
            {"type": 5, "id": "x"},
            {"id": "no-type-at-all"},
            {"type": None, "id": "null-type"},
            # A result for a command nobody issued is dropped in
            # silence, so the next frame's reply is the next thing
            # heard.
            {"type": "command_result", "id": "never-issued", "payload": {"ok": True}},
            {"type": "nonsense", "id": "after"},
        ]),
        results,
    )

    print(f"the connect throttle, on {THROTTLE_NODE}")
    # Eleven attempts: the first ten are refused by auth, the eleventh
    # by the throttle — and both refusals are a 403, so what this
    # actually pins is that the *count* agrees. A port that throttled
    # at a different number would answer the eleventh differently only
    # if the codes differed, which they do not; what would show is a
    # stack that stopped refusing.
    query = f"?api_key=nope&node_id={THROTTLE_NODE}"
    statuses = {}
    for name, port in PORTS.items():
        statuses[name] = [handshake_status(port, query) for _ in range(11)]
    compare("eleven attempts", statuses, results)

    same = sum(1 for _, ok in results if ok)
    differing = len(results) - same
    print()
    print(f"{same}/{len(results)} identical ({len(results)} compared, {differing} differing)")
    return 1 if differing else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
