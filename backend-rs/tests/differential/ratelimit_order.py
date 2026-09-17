#!/usr/bin/env python3
"""Which requests count against a rate limit, and does the limit fire.

`ratelimit_parity.py` proves every ported route declares the same
budget as its Python decorator. It cannot see *when* the budget is
spent, and that turned out to differ:

    slowapi's @limiter.limit wraps the endpoint function, and FastAPI
    resolves every dependency — auth, and path/query/body validation —
    before calling it. So a request refused with 401, 403 or 422 never
    reaches the limiter and never spends a slot. An HTTPException raised
    *inside* the handler (a 404 for a missing id) does.

Measured against the running service: six member-token 403s against the
5/hour wipe-logs route, then an admin call — 200. Twenty-one 422s against
the 20/hour node create, then a valid one — 200. Six in-handler 404s
against the 5/minute rotate-key — the sixth was a 429.

A port that checks the limit first, as an extractor, spends a slot on
every refused request. On an admin route that means a member can
exhaust the whole org's budget and lock the admin out — in Rust, while
Python lets the admin through.

For each rate-limited ported route this sends, to each tier separately:

  A. LIMIT+1 requests refused by a dependency (member token, or no
     token, or a 422), then one request that reaches the handler. Python
     answers that last one normally; a limiter that runs too early 429s.
  B. LIMIT requests that reach the handler, then one more. Both tiers
     must 429 the last — which proves the limit is enforced at all, so
     moving the check later cannot quietly drop it.

Usage: ratelimit_order.py ADMIN_TOKEN MEMBER_TOKEN [route-substring]
"""
from __future__ import annotations

import http.client
import json
import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
TIERS = {"python": 8001, "rust": 8000}
REDIS = "cc-redis-test"
PG = "cc-schema-test"

ADMIN, MEMBER = sys.argv[1], sys.argv[2]
ONLY = sys.argv[3] if len(sys.argv) > 3 else ""

# (label, limit, refused, reaches_handler)
# Each request is (method, path, auth, body) with auth in
# {"admin", "member", None} and body a dict, bytes, or None.
ROUTES = [
    ("DELETE camera-groups/{id}", 60,
     [("DELETE", "/api/camera-groups/9999", "member", None),
      ("DELETE", "/api/camera-groups/abc", "admin", None)],
     ("DELETE", "/api/camera-groups/9999", "admin", None)),
    ("DELETE incidents/{id}", 60,
     [("DELETE", "/api/incidents/9999", "member", None),
      ("DELETE", "/api/incidents/abc", "admin", None)],
     ("DELETE", "/api/incidents/9999", "admin", None)),
    ("DELETE integration/keys/{id}", 30,
     [("DELETE", "/api/integration/keys/9999", "member", None),
      ("DELETE", "/api/integration/keys/abc", "admin", None)],
     ("DELETE", "/api/integration/keys/9999", "admin", None)),
    ("GET audit-logs", 120,
     [("GET", "/api/audit-logs", "member", None),
      ("GET", "/api/audit-logs?limit=abc", "admin", None)],
     ("GET", "/api/audit-logs?limit=1", "admin", None)),
    ("GET audit/stream-logs", 120,
     [("GET", "/api/audit/stream-logs", "member", None),
      ("GET", "/api/audit/stream-logs?limit=abc", "admin", None)],
     ("GET", "/api/audit/stream-logs?limit=1", "admin", None)),
    ("GET audit/stream-logs/stats", 60,
     [("GET", "/api/audit/stream-logs/stats", "member", None)],
     ("GET", "/api/audit/stream-logs/stats", "admin", None)),
    ("GET evidence blob", 120,
     [("GET", "/api/incidents/1/evidence/9999", "member", None),
      ("GET", "/api/incidents/1/evidence/abc", "admin", None)],
     ("GET", "/api/incidents/1/evidence/9999", "admin", None)),
    ("GET evidence playlist", 120,
     [("GET", "/api/incidents/1/evidence/9999/playlist.m3u8", "member", None),
      ("GET", "/api/incidents/1/evidence/abc/playlist.m3u8", "admin", None)],
     ("GET", "/api/incidents/1/evidence/9999/playlist.m3u8", "admin", None)),
    ("GET mcp/activity/logs", 120,
     [("GET", "/api/mcp/activity/logs", "member", None),
      ("GET", "/api/mcp/activity/logs?limit=abc", "admin", None)],
     ("GET", "/api/mcp/activity/logs?limit=1", "admin", None)),
    ("GET mcp/activity/logs/stats", 60,
     [("GET", "/api/mcp/activity/logs/stats", "member", None)],
     ("GET", "/api/mcp/activity/logs/stats", "admin", None)),
    ("GET install.sh", 30, [], ("GET", "/install.sh", None, None)),
    ("GET mcp-setup.sh", 30, [], ("GET", "/mcp-setup.sh", None, None)),
    ("GET mcp-setup.ps1", 30, [], ("GET", "/mcp-setup.ps1", None, None)),
    ("PATCH recording-settings", 30,
     [("PATCH", "/api/cameras/does-not-exist/recording-settings", "member",
       {"scheduled_recording": True}),
      ("PATCH", "/api/cameras/does-not-exist/recording-settings", "admin", b"{x"),
      ("PATCH", "/api/cameras/does-not-exist/recording-settings", "admin",
       {"scheduled_start": "xxxxxx"})],
     ("PATCH", "/api/cameras/does-not-exist/recording-settings", "admin",
      {"scheduled_recording": True})),
    ("PATCH incidents/{id}", 120,
     [("PATCH", "/api/incidents/9999", "member", {"status": "open"}),
      ("PATCH", "/api/incidents/abc", "admin", {"status": "open"})],
     ("PATCH", "/api/incidents/9999", "admin", {"status": "open"})),
    ("POST auth/local/login", 10,
     # Both 422 paths: an unparseable body, and a parseable one missing
     # its fields. A check misplaced between the two is caught only by
     # the second.
     [("POST", "/api/auth/local/login", None, b""),
      ("POST", "/api/auth/local/login", None, {})],
     ("POST", "/api/auth/local/login", None, {"username": "admin", "password": "wrong"})),
    ("POST auth/local/refresh", 30,
     [("POST", "/api/auth/local/refresh", None, b""),
      ("POST", "/api/auth/local/refresh", None, {})],
     ("POST", "/api/auth/local/refresh", None, {"token": "not-a-token"})),
    ("POST camera-groups", 20,
     [("POST", "/api/camera-groups", "member", {"name": "x"}),
      ("POST", "/api/camera-groups", "admin", {})],
     ("POST", "/api/camera-groups", "admin", {"name": "Outdoor"})),
    # No 422 form: toggle_recording reads its body with `await
    # request.json()` inside the function, so malformed JSON there is
    # raised after the limiter and IS counted.
    ("POST cameras/{id}/recording", 30,
     [("POST", "/api/cameras/does-not-exist/recording", "member", {"recording": True})],
     ("POST", "/api/cameras/does-not-exist/recording", "admin", {"recording": True})),
    ("POST settings/motion-ingestion", 30,
     [("POST", "/api/settings/motion-ingestion", "member", {"enabled": True})],
     ("POST", "/api/settings/motion-ingestion", "admin", {"enabled": True})),
    ("POST settings/notifications", 30,
     [("POST", "/api/settings/notifications", "member", {"motion_notifications": True}),
      ("POST", "/api/settings/notifications", "admin", {"motion_notifications": "maybe"})],
     ("POST", "/api/settings/notifications", "admin", {"motion_notifications": True})),
    # Node-key routes read everything inside the function, so there is
    # nothing Python refuses before its limiter: every request counts.
    ("POST nodes/validate", 10, [],
     ("POST", "/api/nodes/validate", "node:test-node-key", {"node_id": "nope"})),
    ("POST cameras/{id}/codec", 30, [],
     ("POST", "/api/cameras/nope/codec", "node:test-node-key", {"video_codec": "avc1.64001f"})),
    ("POST nodes/{id}/rotate-key", 5,
     [("POST", "/api/nodes/nope/rotate-key", "member", None)],
     ("POST", "/api/nodes/nope/rotate-key", "admin", None)),
    ("POST nodes", 20,
     [("POST", "/api/nodes", "member", {"name": "x"}),
      ("POST", "/api/nodes", "admin", {"name": 5}),
      ("POST", "/api/nodes", None, b"{x")],
     ("POST", "/api/nodes", "admin", {"name": "rate probe"})),
    ("POST settings/danger/wipe-logs", 5,
     [("POST", "/api/settings/danger/wipe-logs", "member", None)],
     ("POST", "/api/settings/danger/wipe-logs", "admin", None)),
    # group_id is a query parameter, not a body field.
    ("PUT cameras/{id}/group", 60,
     [("PUT", "/api/cameras/does-not-exist/group", "member", None),
      ("PUT", "/api/cameras/does-not-exist/group?group_id=abc", "admin", None)],
     ("PUT", "/api/cameras/does-not-exist/group", "admin", None)),
]


def flush():
    subprocess.run(["docker", "exec", REDIS, "redis-cli", "FLUSHDB"],
                   capture_output=True, check=True, timeout=60)


def reseed():
    seed = (HERE / "seed_cameras.sql").read_text()
    subprocess.run(["docker", "exec", "-i", PG, "psql", "-U", "cc", "-d", "cc",
                    "-v", "ON_ERROR_STOP=1", "-q"], input=seed, text=True,
                   capture_output=True, check=True, timeout=120)


def send(port, req):
    method, path, auth, body = req
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    headers = {}
    if auth == "admin":
        headers["Authorization"] = f"Bearer {ADMIN}"
    elif auth == "member":
        headers["Authorization"] = f"Bearer {MEMBER}"
    elif auth and auth.startswith("node:"):
        headers["X-Node-API-Key"] = auth[len("node:"):]
    data = None
    if body is not None:
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    conn.request(method, path, body=data, headers=headers)
    resp = conn.getresponse()
    resp.read()
    return resp.status


def scenario_a(port, limit, refused, handler):
    """Refused requests, then one that reaches the handler."""
    results = {}
    for form in refused:
        flush()
        statuses = [send(port, form) for _ in range(limit + 1)]
        results[f"{form[2] or 'anon'} {form[0]} {form[1]}"] = (
            sorted(set(statuses)), send(port, handler))
    return results


def scenario_b(port, limit, handler):
    """LIMIT handler-reaching requests, then one more."""
    flush()
    statuses = [send(port, handler) for _ in range(limit)]
    return sorted(set(statuses)), send(port, handler)


def main() -> int:
    reseed()
    bad = 0
    ran = 0
    for label, limit, refused, handler in ROUTES:
        if ONLY and ONLY not in label:
            continue
        ran += 1
        out = {}
        for tier, port in TIERS.items():
            reseed()
            out[tier] = (scenario_a(port, limit, refused, handler),
                         scenario_b(port, limit, handler))
        py_a, py_b = out["python"]
        rs_a, rs_b = out["rust"]
        before = bad

        for key in py_a:
            if py_a[key] != rs_a[key]:
                bad += 1
                print(f"  DIFFER  {label}: {limit + 1} x [{key}] then a real call\n"
                      f"            python {py_a[key][0]} -> {py_a[key][1]}\n"
                      f"            rust   {rs_a[key][0]} -> {rs_a[key][1]}")
            elif py_a[key][1] == 429:
                # Python itself counted the refusals — the case does not
                # test what it claims to, so it cannot be trusted green.
                bad += 1
                print(f"  BADCASE {label}: [{key}] is counted by Python too")
        if py_b != rs_b:
            bad += 1
            print(f"  DIFFER  {label}: {limit} real calls then one more\n"
                  f"            python {py_b}\n            rust   {rs_b}")
        elif py_b[1] != 429:
            bad += 1
            print(f"  NOLIMIT {label}: {limit + 1} calls and no 429 from either tier")
        if bad == before:
            print(f"  ok      {label}")
    reseed()
    flush()
    if not ran:
        print("REFUSING: no route matched")
        return 2
    print(f"\n{ran} route(s) checked, {bad} problem(s)")
    # The shape every other harness reports in, so mutate.py can score it.
    print(f"{ran - min(bad, ran)}/{ran} identical, {bad} differing")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
