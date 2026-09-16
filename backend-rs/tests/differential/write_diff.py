"""Side-effect differential for mutating routes.

Response diffing is not enough for a write. A handler can return exactly
the right JSON and still write the wrong row, skip an audit entry, or
leave `updated_at` untouched. So each case runs twice against a
freshly-reseeded database — once per stack — and compares the response
*and* the resulting table contents.

    reseed -> request to python -> snapshot
    reseed -> request to rust   -> snapshot
    compare responses, compare snapshots

Normalisation is the whole difficulty. Two things legitimately differ
between the runs and must not be reported:

* wall-clock timestamps written as "now". Any timestamp within
  RECENT_WINDOW of the request is replaced with "<recent>". A timestamp
  that is *supposed* to be preserved (an older created_at) falls outside
  the window and is still compared exactly, so "handler wrongly reset
  created_at" is still caught.
* sequence values. The reseed restarts every sequence, so ids are
  deterministic and are compared as-is.

Usage: write_diff.py <token> [-v]
"""

import json
import re
import subprocess
import sys
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
RUST = "http://127.0.0.1:8000"
PYTHON = "http://127.0.0.1:8001"
PG_CONTAINER = "cc-schema-test"

TOKEN = sys.argv[1]
VERBOSE = "-v" in sys.argv

# Timestamps this close to the request are "now" and get normalised.
RECENT_WINDOW = timedelta(minutes=10)

# Tables whose contents are compared after each write. Deliberately
# includes tables a case is not expected to touch: a handler that writes
# a stray audit row, or fails to write an expected one, is exactly the
# kind of bug response diffing misses.
WATCHED = ["incidents", "incident_evidence", "audit_log", "settings",
           "camera_groups", "cameras", "mcp_api_keys"]

# (name, method, path, body) — body None means no request body.
CASES = [
    # --- PATCH: status transitions ------------------------------------
    ("ack open incident", "PATCH", "/api/incidents/1", {"status": "acknowledged"}),
    ("resolve open incident", "PATCH", "/api/incidents/1", {"status": "resolved"}),
    ("dismiss open incident", "PATCH", "/api/incidents/1", {"status": "dismissed"}),
    # already resolved: must NOT re-stamp resolved_at/resolved_by
    ("re-resolve resolved", "PATCH", "/api/incidents/3", {"status": "resolved"}),
    ("resolved -> dismissed", "PATCH", "/api/incidents/3", {"status": "dismissed"}),
    # reopening clears the resolution
    ("reopen resolved", "PATCH", "/api/incidents/3", {"status": "open"}),
    ("reopen an open one", "PATCH", "/api/incidents/1", {"status": "open"}),
    # --- PATCH: other fields ------------------------------------------
    ("severity only", "PATCH", "/api/incidents/1", {"severity": "critical"}),
    ("summary only", "PATCH", "/api/incidents/1", {"summary": "rewritten"}),
    ("report only", "PATCH", "/api/incidents/1", {"report": "a full report"}),
    ("empty report", "PATCH", "/api/incidents/1", {"report": ""}),
    ("several fields", "PATCH", "/api/incidents/2",
     {"status": "resolved", "severity": "low", "summary": "s", "report": "r"}),
    ("empty patch", "PATCH", "/api/incidents/1", {}),
    # --- PATCH: rejections --------------------------------------------
    ("bad status", "PATCH", "/api/incidents/1", {"status": "nonsense"}),
    ("bad severity", "PATCH", "/api/incidents/1", {"severity": "nonsense"}),
    ("null status is a no-op", "PATCH", "/api/incidents/1", {"status": None}),
    ("unknown field ignored", "PATCH", "/api/incidents/1", {"nope": 1}),
    ("missing incident", "PATCH", "/api/incidents/9999", {"status": "open"}),
    ("another tenant's incident", "PATCH", "/api/incidents/4", {"status": "open"}),
    ("non-integer id", "PATCH", "/api/incidents/abc", {"status": "open"}),
    # --- DELETE --------------------------------------------------------
    ("delete incident with evidence", "DELETE", "/api/incidents/1", None),
    ("delete incident without evidence", "DELETE", "/api/incidents/2", None),
    ("delete missing", "DELETE", "/api/incidents/9999", None),
    ("delete another tenant's", "DELETE", "/api/incidents/4", None),
    ("delete non-integer id", "DELETE", "/api/incidents/abc", None),

    # --- camera groups -------------------------------------------------
    ("create group", "POST", "/api/camera-groups", {"name": "New Group"}),
    ("create group, all fields", "POST", "/api/camera-groups",
     {"name": "Full", "color": "#ff0000", "icon": "X"}),
    ("create group, explicit nulls", "POST", "/api/camera-groups",
     {"name": "Nulls", "color": None, "icon": None}),
    ("create duplicate name", "POST", "/api/camera-groups", {"name": "Outdoor"}),
    # another org already has "Their Group" — the uniqueness check is
    # per-org, so this must succeed
    ("create name another org has", "POST", "/api/camera-groups", {"name": "Their Group"}),
    ("create group, missing name", "POST", "/api/camera-groups", {}),
    ("create group, empty name", "POST", "/api/camera-groups", {"name": ""}),
    ("create group, name too long", "POST", "/api/camera-groups", {"name": "x" * 101}),
    # deleting a group must clear its members' group_id AND bump their
    # updated_at, which the data-sync tier selects on
    ("delete group with members", "DELETE", "/api/camera-groups/1", None),
    ("delete empty group", "DELETE", "/api/camera-groups/3", None),
    ("delete missing group", "DELETE", "/api/camera-groups/9999", None),
    ("delete another org's group", "DELETE", "/api/camera-groups/4", None),
    ("delete group, bad id", "DELETE", "/api/camera-groups/abc", None),

    # --- camera group assignment (group_id is a QUERY param) ----------
    ("assign camera to group", "PUT", "/api/cameras/cam-nocaps/group?group_id=1", None),
    ("assign to the same group", "PUT", "/api/cameras/cam-live/group?group_id=1", None),
    ("unassign (no param)", "PUT", "/api/cameras/cam-live/group", None),
    # falsy group_id takes the unassign path but is still echoed back
    ("unassign via group_id=0", "PUT", "/api/cameras/cam-live/group?group_id=0", None),
    ("assign to missing group", "PUT", "/api/cameras/cam-live/group?group_id=9999", None),
    ("assign to another org's group", "PUT", "/api/cameras/cam-live/group?group_id=4", None),
    ("assign missing camera", "PUT", "/api/cameras/nope/group?group_id=1", None),
    ("assign another org's camera", "PUT", "/api/cameras/cam-theirs/group?group_id=1", None),
    ("assign, non-integer group_id", "PUT", "/api/cameras/cam-live/group?group_id=abc", None),

    # --- settings writes (each also writes an audit row) --------------
    ("motion ingestion off", "POST", "/api/settings/motion-ingestion", {"enabled": False}),
    ("motion ingestion on", "POST", "/api/settings/motion-ingestion", {"enabled": True}),
    ("motion ingestion, absent key", "POST", "/api/settings/motion-ingestion", {}),
    # python truthiness: a non-empty string is enabled, "" is not
    ("motion ingestion, string", "POST", "/api/settings/motion-ingestion", {"enabled": "yes"}),
    ("motion ingestion, empty string", "POST", "/api/settings/motion-ingestion", {"enabled": ""}),
    ("motion ingestion, zero", "POST", "/api/settings/motion-ingestion", {"enabled": 0}),
    ("motion ingestion, number", "POST", "/api/settings/motion-ingestion", {"enabled": 7}),
    ("motion ingestion, null", "POST", "/api/settings/motion-ingestion", {"enabled": None}),
    # unchanged value must not bump the row's updated_at
    ("motion ingestion, no change", "POST", "/api/settings/motion-ingestion", {"enabled": True}),

    ("notifications all off", "POST", "/api/settings/notifications",
     {"motion_notifications": False, "camera_transition_notifications": False,
      "node_transition_notifications": False}),
    ("notifications all on", "POST", "/api/settings/notifications",
     {"motion_notifications": True, "camera_transition_notifications": True,
      "node_transition_notifications": True}),
    ("notifications, defaults", "POST", "/api/settings/notifications", {}),
    ("notifications, partial", "POST", "/api/settings/notifications",
     {"motion_notifications": False}),
    ("notifications, wrong type", "POST", "/api/settings/notifications",
     {"motion_notifications": "nope"}),

    # --- recording toggle ---------------------------------------------
    ("start recording", "POST", "/api/cameras/cam-stale/recording", {"recording": True}),
    ("stop recording", "POST", "/api/cameras/cam-live/recording", {"recording": False}),
    # cam-live is already recording: no change, so updated_at must not move
    ("start an already-recording cam", "POST", "/api/cameras/cam-live/recording",
     {"recording": True}),
    ("recording key absent", "POST", "/api/cameras/cam-live/recording", {}),
    ("recording truthy string", "POST", "/api/cameras/cam-stale/recording",
     {"recording": "yes"}),
    ("recording missing camera", "POST", "/api/cameras/nope/recording", {"recording": True}),
    ("recording another tenant's", "POST", "/api/cameras/cam-theirs/recording",
     {"recording": True}),

    # --- recording policy ---------------------------------------------
    ("policy: continuous on", "PATCH", "/api/cameras/cam-stale/recording-settings",
     {"continuous_24_7": True}),
    ("policy: scheduled with window", "PATCH", "/api/cameras/cam-stale/recording-settings",
     {"scheduled_recording": True, "scheduled_start": "08:30", "scheduled_end": "17:00"}),
    ("policy: clear the window", "PATCH", "/api/cameras/cam-failed/recording-settings",
     {"scheduled_start": "", "scheduled_end": ""}),
    ("policy: one toggle only", "PATCH", "/api/cameras/cam-failed/recording-settings",
     {"scheduled_recording": False}),
    ("policy: empty patch", "PATCH", "/api/cameras/cam-live/recording-settings", {}),
    # both modes at once is rejected on the RESULTING state, so this
    # catches "turn continuous on while scheduled is already on"
    ("policy: both modes in one patch", "PATCH", "/api/cameras/cam-stale/recording-settings",
     {"continuous_24_7": True, "scheduled_recording": True}),
    ("policy: continuous on over existing schedule", "PATCH",
     "/api/cameras/cam-failed/recording-settings", {"continuous_24_7": True}),
    ("policy: swap mode in one patch", "PATCH",
     "/api/cameras/cam-failed/recording-settings",
     {"continuous_24_7": True, "scheduled_recording": False}),
    ("policy: bad bool", "PATCH", "/api/cameras/cam-live/recording-settings",
     {"continuous_24_7": "nope"}),
    ("policy: start too long", "PATCH", "/api/cameras/cam-live/recording-settings",
     {"scheduled_start": "123456"}),
    ("policy: start wrong type", "PATCH", "/api/cameras/cam-live/recording-settings",
     {"scheduled_start": 5}),
    ("policy: missing camera", "PATCH", "/api/cameras/nope/recording-settings",
     {"continuous_24_7": True}),
    ("policy: another tenant's", "PATCH", "/api/cameras/cam-theirs/recording-settings",
     {"continuous_24_7": True}),
]

# Cases where the two stacks are known to differ, with a reason. Same
# contract as the claim differential's list: an unexpected divergence
# fails the run, and so does an expected one that has stopped diverging.
EXPECTED_DIVERGENCES = {
    # Python 500s on ANY custom pydantic validator: the validator raises
    # ValueError, pydantic v2 puts the exception object in ctx["error"],
    # and main.py's 422 handler calls JSONResponse on it, which
    # json.dumps cannot serialise. Rust returns the 422 the validator was
    # written to produce. See expected_divergences.md.
    "policy: bad HH:MM",
    "policy: single-digit hour",
}

CASES += [
    # --- local auth ----------------------------------------------------
    ("login, correct credentials", "POST", "/api/auth/local/login",
     {"username": "admin", "password": "correct horse battery staple"}),
    ("login, wrong password", "POST", "/api/auth/local/login",
     {"username": "admin", "password": "wrong"}),
    # a wrong username must not short-circuit — same message, and the
    # argon2 verify still runs so the timing does not leak the username
    ("login, wrong username", "POST", "/api/auth/local/login",
     {"username": "nobody", "password": "correct horse battery staple"}),
    ("login, both wrong", "POST", "/api/auth/local/login",
     {"username": "nobody", "password": "wrong"}),
    ("login, empty strings", "POST", "/api/auth/local/login",
     {"username": "", "password": ""}),
    ("login, missing password", "POST", "/api/auth/local/login", {"username": "admin"}),
    ("login, missing both", "POST", "/api/auth/local/login", {}),
    ("login, wrong types", "POST", "/api/auth/local/login",
     {"username": 5, "password": True}),
    ("login, unicode password", "POST", "/api/auth/local/login",
     {"username": "admin", "password": "\u00e9\u00e9\u00e9"}),
    ("refresh, garbage token", "POST", "/api/auth/local/refresh", {"token": "not-a-jwt"}),
    ("refresh, empty token", "POST", "/api/auth/local/refresh", {"token": ""}),
    ("refresh, missing token", "POST", "/api/auth/local/refresh", {}),

    # --- revoking an integration key ----------------------------------
    ("revoke integration key", "DELETE", "/api/integration/keys/5", None),
    ("revoke the other one", "DELETE", "/api/integration/keys/6", None),
    # kind scoping: an MCP key id here must 404 rather than cross surfaces
    ("revoke an MCP key via integration", "DELETE", "/api/integration/keys/1", None),
    ("revoke an already-revoked key", "DELETE", "/api/integration/keys/4", None),
    ("revoke missing key", "DELETE", "/api/integration/keys/9999", None),
    ("revoke another tenant's key", "DELETE", "/api/integration/keys/10", None),
    ("revoke, non-integer id", "DELETE", "/api/integration/keys/abc", None),

    ("policy: bad HH:MM", "PATCH", "/api/cameras/cam-live/recording-settings",
     {"scheduled_start": "25:00"}),
    ("policy: single-digit hour", "PATCH", "/api/cameras/cam-live/recording-settings",
     {"scheduled_start": "8:30"}),
]


def psql(sql):
    return subprocess.run(
        ["docker", "exec", "-i", PG_CONTAINER, "psql", "-U", "cc", "-d", "cc", "-tAq", "-c", sql],
        capture_output=True, text=True, check=False,
    ).stdout


# The shared fixture seeds several timestamps relative to now() so the
# READ differential can exercise live-vs-offline cameras. That makes the
# snapshot non-deterministic here: the two runs reseed a second or two
# apart, and any seeded timestamp older than RECENT_WINDOW is compared
# literally and differs by exactly that delta.
#
# No write case depends on camera liveness, so those columns are pinned
# to fixed values after seeding. Columns a write actually touches are
# left alone.
FREEZE = """
UPDATE cameras SET last_seen = timestamp '2026-01-01 00:00:00'
 WHERE last_seen IS NOT NULL;
UPDATE camera_nodes SET last_seen = timestamp '2026-01-01 00:00:00'
 WHERE last_seen IS NOT NULL;
"""


def reseed():
    seed = (HERE / "seed_cameras.sql").read_text() + FREEZE
    subprocess.run(
        ["docker", "exec", "-i", PG_CONTAINER, "psql", "-U", "cc", "-d", "cc", "-q"],
        input=seed, capture_output=True, text=True, check=False,
    )


def snapshot():
    """Table contents as JSON, ordered so the comparison is stable."""
    out = {}
    for table in WATCHED:
        # row_to_json over an ordered select: deterministic, and it does
        # not need to know the column list.
        rows = psql(
            f"SELECT row_to_json(t) FROM (SELECT * FROM {table} ORDER BY 1) t"
        )
        out[table] = [json.loads(line) for line in rows.splitlines() if line.strip()]
    return out


TS_FORMATS = ("%Y-%m-%dT%H:%M:%S.%f", "%Y-%m-%dT%H:%M:%S")


JWT = re.compile(r"^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$")


def normalise(value, now):
    """Replace just-written timestamps with a token, recursively.

    A freshly signed session token differs between the two runs by its
    `iat`/`exp` alone, so only its shape is compared. The token is
    proven equivalent elsewhere: both stacks verify each other's, which
    is what the HTTP differential runs on.
    """
    if isinstance(value, dict):
        return {k: normalise(v, now) for k, v in value.items()}
    if isinstance(value, list):
        return [normalise(v, now) for v in value]
    if isinstance(value, str) and JWT.match(value):
        return "<jwt>"
    if isinstance(value, str) and 19 <= len(value) <= 26 and value[4] == "-":
        for fmt in TS_FORMATS:
            try:
                ts = datetime.strptime(value, fmt)
            except ValueError:
                continue
            if abs(now - ts) < RECENT_WINDOW:
                return "<recent>"
            return value
    return value


def fetch(base, method, path, body):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, method=method, data=data)
    req.add_header("Authorization", f"Bearer {TOKEN}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception as e:  # noqa: BLE001
        return None, str(e).encode()


def run_case(base, method, path, body):
    reseed()
    now = datetime.now(timezone.utc).replace(tzinfo=None)
    status, raw = fetch(base, method, path, body)
    try:
        parsed = json.loads(raw)
    except Exception:  # noqa: BLE001
        parsed = raw.decode("utf-8", "replace")
    return status, normalise(parsed, now), normalise(snapshot(), now)


def main():
    bad = 0
    for name, method, path, body in CASES:
        py_status, py_body, py_db = run_case(PYTHON, method, path, body)
        rs_status, rs_body, rs_db = run_case(RUST, method, path, body)

        status_same = py_status == rs_status
        body_same = py_body == rs_body
        db_same = py_db == rs_db

        identical = status_same and body_same and db_same
        if name in EXPECTED_DIVERGENCES:
            if identical:
                bad += 1
                print(f"  STALE   {name:<34} listed as an expected divergence "
                      f"but the two now agree")
            elif VERBOSE:
                print(f"  ok(div) {name:<34} differs as expected "
                      f"(rust={rs_status} python={py_status})")
            continue

        if identical:
            if VERBOSE:
                print(f"  ok      {name:<34} {method} {path} -> {rs_status}")
            continue

        bad += 1
        print(f"  DIFFER  {name:<34} {method} {path}")
        if not status_same:
            print(f"            status: rust={rs_status} python={py_status}")
        if not body_same:
            print(f"            body rust  : {json.dumps(rs_body, sort_keys=True)[:260]}")
            print(f"            body python: {json.dumps(py_body, sort_keys=True)[:260]}")
        if not db_same:
            for table in WATCHED:
                if py_db[table] != rs_db[table]:
                    print(f"            SIDE EFFECT differs in `{table}`:")
                    print(f"              rust  : {json.dumps(rs_db[table], sort_keys=True)[:240]}")
                    print(f"              python: {json.dumps(py_db[table], sort_keys=True)[:240]}")

    # A run where nothing was actually written proves nothing.
    reseed()
    seeded = int(psql("SELECT COUNT(*) FROM incidents").strip() or 0)
    ev = int(psql("SELECT COUNT(*) FROM incident_evidence").strip() or 0)
    print(f"\nfixture: {seeded} incidents, {ev} evidence rows")
    if seeded < 4 or ev < 1:
        print("FIXTURE TOO THIN — seed incidents and evidence before trusting this")
        return 2

    print(f"{len(CASES) - bad}/{len(CASES)} identical (response + side effects), {bad} differing")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
