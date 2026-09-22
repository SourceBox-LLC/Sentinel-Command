#!/usr/bin/env python3
"""The Server-Sent Events stream, which a request/response diff cannot see.

`GET /api/notifications/stream` never finishes. The HTTP differential
fetches a response and compares it; here there is no response to fetch,
only a socket that stays open and says something every so often. So this
speaks HTTP itself, holds the sockets, and compares what arrives on
them.

Three things are worth comparing, and only the first is visible to a
client that connects once:

* **the greeting** — status line, the headers the HTTP differential
  compares, and the first frame's bytes;
* **the keepalive** — a quiet stream still has to say something or an
  intermediary times it out. Both stacks wait 25 seconds; a port that
  waited 30 would look perfect for the first 25 and then start losing
  connections behind someone's proxy;
* **the cap** — the per-tier subscriber limit, its 429, and its body.
  This is the one that needs fifty sockets to reach, which is why no
  amount of single-request testing finds a cap off by one.

Streams are opened against both stacks at once and read together, so the
25-second wait is paid once rather than twice.

Usage: sse_diff.py <token> [-v]
"""
from __future__ import annotations

import json
import socket
import sys
import time

PORTS = {"rust": 8000, "python": 8001}
PATH = "/api/notifications/stream"

# The same set the HTTP differential compares, minus the ones that are
# random per request. `connection` is deliberately absent: it is
# hop-by-hop, hyper owns it, and it is not in COMPARED_HEADERS either.
COMPARED_HEADERS = (
    "content-type",
    "cache-control",
    "x-accel-buffering",
    "x-content-type-options",
    "x-frame-options",
    "referrer-policy",
    "permissions-policy",
    "access-control-allow-origin",
    "access-control-allow-credentials",
    "vary",
    "retry-after",
)

# `_TIMEOUT` in the stream generator is 25s on both sides. Allow the
# round trip on top before calling it a failure.
KEEPALIVE_WAIT = 25
KEEPALIVE_SLACK = 6

VERBOSE = "-v" in sys.argv


def open_stream(port: str | int, token: str, timeout: float = 5.0) -> socket.socket:
    """Connect and send the request, without reading the response."""
    sock = socket.create_connection(("127.0.0.1", int(port)), timeout=timeout)
    sock.sendall(
        f"GET {PATH} HTTP/1.1\r\n"
        f"Host: 127.0.0.1\r\n"
        f"Authorization: Bearer {token}\r\n"
        f"Accept: text/event-stream\r\n"
        f"\r\n".encode()
    )
    return sock


def read_head(sock: socket.socket, timeout: float = 5.0):
    """(status line, {header: value}, leftover bytes of the body)."""
    sock.settimeout(timeout)
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            break
        buf += chunk
    head, _, rest = buf.partition(b"\r\n\r\n")
    lines = head.decode("latin-1").split("\r\n")
    status = lines[0] if lines else ""
    headers = {}
    for line in lines[1:]:
        if ":" in line:
            name, _, value = line.partition(":")
            headers[name.strip().lower()] = value.strip()
    return status, headers, rest


def dechunk(raw: bytes) -> tuple[bytes, bytes]:
    """Strip HTTP chunked framing, returning `(payload, undecodable tail)`.

    The framing itself must NOT be compared. It carries the chunk length
    in hex, and hyper writes that hex in upper case where uvicorn writes
    it in lower: a 13-byte keepalive is `D\r\n` from one and `d\r\n`
    from the other. No HTTP client can see the difference — both decode
    to the same bytes — but a literal comparison of the raw stream calls
    it a port difference. The first frame happened to agree only
    because 52 bytes is `34`, which has no letters in it.
    """
    out = bytearray()
    while True:
        line_end = raw.find(b"\r\n")
        if line_end == -1:
            return bytes(out), raw
        try:
            size = int(raw[:line_end].split(b";")[0], 16)
        except ValueError:
            return bytes(out), raw
        if size == 0:
            return bytes(out), b""
        body_start = line_end + 2
        body_end = body_start + size
        if len(raw) < body_end + 2:
            return bytes(out), raw
        out += raw[body_start:body_end]
        raw = raw[body_end + 2:]


def read_frame(sock: socket.socket, have: bytes, timeout: float):
    """The next SSE frame's payload, with chunk framing removed.

    Returns `(frame, leftover)`, where leftover is the still-chunked
    remainder so the next call picks up where this one stopped.
    """
    deadline = time.monotonic() + timeout
    while True:
        payload, tail = dechunk(have)
        if b"\n\n" in payload:
            frame, _, rest = payload.partition(b"\n\n")
            # Re-chunk nothing: what is left is handed back as a plain
            # payload prefix, which `dechunk` leaves alone because it
            # will not parse as a length line.
            return frame + b"\n\n", rest + tail if not rest else tail
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return None, have
        sock.settimeout(remaining)
        try:
            chunk = sock.recv(4096)
        except socket.timeout:
            return None, have
        if not chunk:
            return None, have
        have += chunk


def read_body(sock: socket.socket, headers: dict, have: bytes, timeout: float) -> bytes:
    """A whole finite response body.

    Reading whatever happened to arrive with the headers is a race: the
    429 body came back in the same segment from one stack and a segment
    later from the other, which scored as a difference when both had
    sent exactly the same bytes.
    """
    length = headers.get("content-length")
    deadline = time.monotonic() + timeout
    if length is not None:
        want = int(length)
        while len(have) < want:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            sock.settimeout(remaining)
            try:
                chunk = sock.recv(4096)
            except socket.timeout:
                break
            if not chunk:
                break
            have += chunk
        return have[:want]
    # Chunked, or no length at all: read to the terminator or the
    # deadline, then decode.
    while b"0\r\n\r\n" not in have:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        sock.settimeout(remaining)
        try:
            chunk = sock.recv(4096)
        except socket.timeout:
            break
        if not chunk:
            break
        have += chunk
    return dechunk(have)[0]


def compare(label: str, values: dict[str, object], results: list) -> bool:
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
    return same


def case_greeting(token: str, results: list) -> None:
    print("the greeting")
    socks = {name: open_stream(port, token) for name, port in PORTS.items()}
    try:
        heads = {name: read_head(sock) for name, sock in socks.items()}
        compare("status", {n: h[0] for n, h in heads.items()}, results)
        compare(
            "headers",
            {
                n: json.dumps(
                    {k: v for k, v in h[1].items() if k in COMPARED_HEADERS},
                    sort_keys=True,
                )
                for n, h in heads.items()
            },
            results,
        )
        frames = {}
        for name, sock in socks.items():
            frame, _ = read_frame(sock, heads[name][2], timeout=5)
            frames[name] = frame
        compare("first frame", frames, results)
    finally:
        for sock in socks.values():
            sock.close()


def case_keepalive(token: str, results: list) -> None:
    print(f"the keepalive, after {KEEPALIVE_WAIT}s of quiet")
    socks = {name: open_stream(port, token) for name, port in PORTS.items()}
    try:
        leftovers = {}
        for name, sock in socks.items():
            _, _, rest = read_head(sock)
            _, rest = read_frame(sock, rest, timeout=5)
            leftovers[name] = rest
        # Both sockets are now parked on an idle stream. Read them
        # together so the wait is paid once.
        frames = {}
        for name, sock in socks.items():
            frame, _ = read_frame(sock, leftovers[name], KEEPALIVE_WAIT + KEEPALIVE_SLACK)
            frames[name] = frame
        compare("keepalive frame", frames, results)
    finally:
        for sock in socks.values():
            sock.close()


def case_cap(token: str, cap: int, results: list) -> None:
    print(f"the subscriber cap ({cap} for this plan)")
    held: dict[str, list[socket.socket]] = {name: [] for name in PORTS}
    try:
        for name, port in PORTS.items():
            for _ in range(cap):
                sock = open_stream(port, token)
                read_head(sock)
                held[name].append(sock)
        # One more than the cap, on each.
        over = {}
        for name, port in PORTS.items():
            sock = open_stream(port, token)
            try:
                status, headers, rest = read_head(sock)
                body = read_body(sock, headers, rest, timeout=3)
                over[name] = (status, headers.get("content-type"), body)
            finally:
                sock.close()
        compare("over-cap status", {n: v[0] for n, v in over.items()}, results)
        compare("over-cap content-type", {n: v[1] for n, v in over.items()}, results)
        compare("over-cap body", {n: v[2] for n, v in over.items()}, results)

        # And closing one frees exactly one slot, rather than the org
        # staying wedged at the cap until every stream goes.
        freed = {}
        for name, port in PORTS.items():
            held[name].pop().close()
            # The server notices the close when it next writes or reads;
            # give it a moment.
            time.sleep(0.3)
            sock = open_stream(port, token)
            try:
                status, _, _ = read_head(sock)
                freed[name] = status
            finally:
                sock.close()
        compare("status after freeing one slot", freed, results)
    finally:
        for socks in held.values():
            for sock in socks:
                sock.close()


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    token = sys.argv[1]

    results: list[tuple[str, bool]] = []
    case_greeting(token, results)
    # 50 is `self_host`'s `max_sse_subscribers`; the harness runs the
    # local-auth tier, where every org resolves to that plan.
    case_cap(token, 50, results)
    case_keepalive(token, results)

    same = sum(1 for _, ok in results if ok)
    differing = len(results) - same
    print()
    print(f"{same}/{len(results)} identical ({len(results)} compared, {differing} differing)")
    return 1 if differing else 0


if __name__ == "__main__":
    sys.exit(main())
