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
    for method, path, auth in CASES:
        rs_status, rs_body = fetch(RUST, method, path, auth)
        py_status, py_body = fetch(PYTHON, method, path, auth)
        rs, py = normalise(rs_body), normalise(py_body)

        same = (rs_status == py_status) and (rs == py)
        label = f"{method} {path}" + ("" if auth else "  (no auth)")
        if same:
            if VERBOSE:
                print(f"  ok      {label:<46} {rs_status}")
        else:
            bad += 1
            print(f"  DIFFER  {label:<46} rust={rs_status} python={py_status}")
            if rs_status == py_status:
                print(f"            rust  : {json.dumps(rs, sort_keys=True)[:300]}")
                print(f"            python: {json.dumps(py, sort_keys=True)[:300]}")

    print(f"\n{len(CASES) - bad}/{len(CASES)} identical, {bad} differing")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
