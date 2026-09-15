"""HTTP differential: the Rust tier vs the Python it replaces.

Sends identical requests to both stacks — which are pointed at one
Postgres — and compares status code and JSON body.

Bodies are compared structurally, not as text: Python's json separators
differ from serde_json's, and neither list route has an ORDER BY (both
emit SQLAlchemy's unordered `filter_by(...).all()`), so list responses
are compared as multisets keyed on a stable field rather than by
position. Anything order-dependent would be a flake, not a finding.

Usage:
    http_diff.py <token> [-v]
"""

import json
import sys
import urllib.error
import urllib.request

RUST = "http://127.0.0.1:8000"
PYTHON = "http://127.0.0.1:8001"

TOKEN = sys.argv[1]
VERBOSE = "-v" in sys.argv

# (method, path, expect_auth) — expect_auth False sends no credential.
CASES = [
    ("GET", "/api/cameras", True),
    ("GET", "/api/cameras", False),
    ("GET", "/api/camera-groups", True),
    ("GET", "/api/camera-groups", False),
    # every seeded camera, by id
    *[("GET", f"/api/cameras/{c}", True) for c in [
        "cam-live", "cam-stale", "cam-boundary", "cam-failed", "cam-failedold",
        "cam-restart", "cam-error", "cam-offline", "cam-neverseen", "cam-orphan",
        "cam-nocaps", "cam-emptycaps", "cam-spacecaps", "cam-nullstatus",
        "cam-capped", "cam-ts-whole", "cam-ts-tenth", "cam-ts-micro",
    ]],
    # cross-tenant: belongs to another org, must 404 not 200
    ("GET", "/api/cameras/cam-theirs", True),
    # nonexistent
    ("GET", "/api/cameras/does-not-exist", True),
    # url-encoded and awkward ids
    ("GET", "/api/cameras/../nodes", True),
    ("GET", "/api/cameras/cam%20space", True),
    ("GET", "/api/cameras/", True),
    # --- settings ----------------------------------------------------
    ("GET", "/api/settings", True),
    ("GET", "/api/settings", False),
    ("GET", "/api/settings/notifications", True),
    ("GET", "/api/settings/motion-ingestion", True),

    # --- audit logs: pagination, filters and every 422 shape ----------
    ("GET", "/api/audit-logs", True),
    ("GET", "/api/audit-logs", False),
    *[("GET", f"/api/audit-logs?{q}", True) for q in [
        "limit=1", "limit=5", "limit=500", "limit=3&offset=10",
        "offset=0", "offset=1000000", "offset=999999",
        "event=camera_created", "event=node_registered", "event=nope", "event=",
        # the LIKE-escaping cases: an underscore and a percent that the
        # caller typed literally and must not be treated as wildcards
        "username=clerk_user_alpha", "username=beta%25user", "username=_",
        "username=%25", "username=CLERK_USER_ALPHA", "username=nobody",
        "event=camera_created&username=clerk_user_alpha&limit=2",
        # parsing edges measured against the running service
        "limit=1_000", "limit=5.0", "limit=%205%20", "limit=05", "limit=1&limit=2",
        # every validation failure
        "limit=0", "limit=501", "limit=abc", "limit=", "limit=5.5", "limit=1e3",
        "limit=0x10", "limit=true", "offset=-1", "offset=1000001",
        "format=xml", "format=json", "offset=-1&limit=0", "limit=0&offset=-1",
        "unknown=x",
    ]],
    # csv still belongs to Python; validation must still run in Rust first
    ("GET", "/api/audit-logs?format=csv&limit=2", True),
    ("GET", "/api/audit-logs?format=csv&limit=0", True),

    # --- stream access logs (plan-gated on the "admin" feature) -------
    ("GET", "/api/audit/stream-logs", True),
    ("GET", "/api/audit/stream-logs", False),
    *[("GET", f"/api/audit/stream-logs?{q}", True) for q in [
        "limit=5", "limit=3&offset=7", "offset=140",
        "camera_id=cam-1", "camera_id=cam-5", "camera_id=nope",
        # NOT escaped on this route, unlike its two siblings: an
        # underscore here really is a LIKE wildcard, and the port has to
        # reproduce that rather than tidy it up.
        "user_id=user_1", "user_id=user%5F1", "user_id=_", "user_id=%25",
        "user_id=USER_1", "user_id=example.com", "user_id=nobody",
        "camera_id=cam-2&user_id=user_2&limit=4",
        "limit=0", "limit=501", "offset=-1", "format=xml",
    ]],
    ("GET", "/api/audit/stream-logs?format=csv&limit=2", True),
    ("GET", "/api/audit/stream-logs/stats", True),
    ("GET", "/api/audit/stream-logs/stats", False),
    *[("GET", f"/api/audit/stream-logs/stats?{q}", True) for q in [
        "days=1", "days=7", "days=30", "days=0",
        # no `ge` on this one in the Python, so a negative window is
        # accepted and simply returns zeroes
        "days=-5", "days=31", "days=abc",
    ]],

    # --- motion ------------------------------------------------------
    ("GET", "/api/motion/events", True),
    ("GET", "/api/motion/events", False),
    *[("GET", f"/api/motion/events?{q}", True) for q in [
        "hours=1", "hours=24", "hours=168", "limit=5", "limit=3&offset=4",
        "camera_id=cam-1", "camera_id=cam-4", "camera_id=nope",
        "hours=48&camera_id=cam-2&limit=6",
        "hours=0", "hours=169", "limit=0", "offset=-1", "hours=abc",
    ]],
    ("GET", "/api/motion/events/stats", True),
    *[("GET", f"/api/motion/events/stats?{q}", True) for q in [
        "hours=1", "hours=24", "hours=168", "hours=-5", "hours=169", "hours=abc",
    ]],

    # --- mcp activity: the DB-backed routes only ----------------------
    ("GET", "/api/mcp/activity/logs", True),
    ("GET", "/api/mcp/activity/logs", False),
    *[("GET", f"/api/mcp/activity/logs?{q}", True) for q in [
        "limit=5", "limit=4&offset=3",
        "tool_name=tool_1", "tool_name=tool_3", "tool_name=nope",
        "status=ok", "status=error", "status=nope",
        # escaped on this route: the underscore and percent are literal
        "key_name=key_alpha_one", "key_name=key%25beta", "key_name=_", "key_name=%25",
        "tool_name=tool_2&status=error&limit=3",
        "limit=0", "limit=501", "offset=-1", "format=xml",
    ]],
    ("GET", "/api/mcp/activity/logs?format=csv&limit=2", True),
    ("GET", "/api/mcp/activity/logs/stats", True),
    *[("GET", f"/api/mcp/activity/logs/stats?{q}", True) for q in [
        "days=1", "days=7", "days=30", "days=-5", "days=31", "days=abc",
    ]],
    # these three read an in-memory tracker in the Python process and
    # must still be proxied, not answered from an empty Rust one
    ("GET", "/api/mcp/activity/recent", True),
    ("GET", "/api/mcp/activity/sessions", True),
    ("GET", "/api/mcp/activity/stats", True),

    # --- nodes: only the single-node read is ported -------------------
    ("GET", "/api/nodes/node-aaaa1111", True),
    ("GET", "/api/nodes/node-bbbb2222", True),
    ("GET", "/api/nodes/node-aaaa1111", False),
    # another tenant's node must 404, not leak
    ("GET", "/api/nodes/node-cccc3333", True),
    ("GET", "/api/nodes/does-not-exist", True),
    # these stay with Python; they read in-process state
    ("GET", "/api/nodes", True),
    ("GET", "/api/nodes/plan", True),
    ("GET", "/api/nodes/ws-status", True),

    # methods Rust has NOT ported on a path it HAS — these must still
    # reach Python rather than being answered with 405 by axum.
    ("POST", "/api/cameras", True),
    ("DELETE", "/api/cameras/cam-live", True),
    ("POST", "/api/camera-groups", True),
    ("PUT", "/api/cameras/cam-live", True),
]


def fetch(base, method, path, with_auth):
    req = urllib.request.Request(base + path, method=method)
    if with_auth:
        req.add_header("Authorization", f"Bearer {TOKEN}")
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception as e:  # noqa: BLE001
        return None, str(e).encode()


def normalise(body):
    """Parse JSON if possible; sort lists by a stable key."""
    try:
        v = json.loads(body)
    except Exception:  # noqa: BLE001
        return body.decode("utf-8", "replace")
    if isinstance(v, list):
        def key(item):
            if isinstance(item, dict):
                for k in ("camera_id", "id", "name"):
                    if k in item:
                        return (0, str(item[k]))
            return (1, json.dumps(item, sort_keys=True))
        return sorted(v, key=key)
    return v


def check_coverage():
    """Fail if the fixture has aged out of the interesting states.

    `effective_status` turns a camera offline 90 seconds after its last
    heartbeat. A fixture seeded once and reused later therefore exercises
    only the offline branch — and an earlier run of this harness reported
    31/31 identical while doing exactly that, testing neither a live
    camera nor the last_error surfacing. Re-seed, then run.
    """
    status, body = fetch(RUST, "GET", "/api/cameras", True)
    if status != 200:
        print(f"COVERAGE: /api/cameras returned {status}, expected 200")
        return False
    cams = json.loads(body)
    live = [c for c in cams if c["status"] not in ("offline", None)]
    errs = [c for c in cams if c["last_error"]]
    stale = [c for c in cams if c["status"] == "offline"]
    fracs = [c["last_seen"] for c in cams if c["last_seen"] and "." in c["last_seen"]]
    whole = [c["last_seen"] for c in cams
             if c["last_seen"] and "." not in c["last_seen"]]

    print(f"coverage: {len(cams)} cameras, {len(live)} live, {len(stale)} offline, "
          f"{len(errs)} surfacing last_error, "
          f"{len(fracs)} fractional / {len(whole)} whole-second timestamps")

    ok = True
    for label, got, want in [
        ("live cameras", len(live), 3),
        ("offline cameras", len(stale), 3),
        ("cameras surfacing last_error", len(errs), 3),
        ("fractional timestamps", len(fracs), 1),
        ("whole-second timestamps", len(whole), 1),
    ]:
        if got < want:
            print(f"COVERAGE TOO THIN: {label} = {got}, need >= {want}. Re-seed first.")
            ok = False
    return ok


def main():
    if not check_coverage():
        print("\nrun seed_cameras.sql against the test database, then re-run")
        return 2

    bad = 0
    rate_limited = []
    for method, path, auth in CASES:
        rs_status, rs_body = fetch(RUST, method, path, auth)
        py_status, py_body = fetch(PYTHON, method, path, auth)
        rs, py = normalise(rs_body), normalise(py_body)

        same = (rs_status == py_status) and (rs == py)
        label = f"{method} {path}" + ("" if auth else "  (no auth)")

        # A 429 on either side means the run itself exhausted a limit, not
        # that the port is wrong. Counting it as a diff would be a false
        # positive; counting it as a pass would hide a real one.
        if 429 in (rs_status, py_status) and not path.startswith("/api/_ratelimit"):
            rate_limited.append(label)
            continue
        if same:
            if VERBOSE:
                print(f"  ok      {label:<46} {rs_status}")
        else:
            bad += 1
            print(f"  DIFFER  {label:<46} rust={rs_status} python={py_status}")
            if rs_status == py_status:
                print(f"            rust  : {json.dumps(rs, sort_keys=True)[:300]}")
                print(f"            python: {json.dumps(py, sort_keys=True)[:300]}")

    if rate_limited:
        print(f"\nINCONCLUSIVE: {len(rate_limited)} case(s) hit a rate limit and were "
              f"not compared, e.g. {rate_limited[0]}")
        print("Flush the limiter (docker exec cc-redis-test redis-cli FLUSHDB) and re-run.")
        return 3

    print(f"\n{len(CASES) - bad}/{len(CASES)} identical, {bad} differing")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
