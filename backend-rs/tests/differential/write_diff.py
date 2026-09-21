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

* wall-clock timestamps written as "now", with or without an offset.
  Any timestamp within
  RECENT_WINDOW of the request is replaced with "<recent>". A timestamp
  that is *supposed* to be preserved (an older created_at) falls outside
  the window and is still compared exactly, so "handler wrongly reset
  created_at" is still caught.
* sequence values. The reseed restarts every sequence, so ids are
  deterministic and are compared as-is.

Usage: write_diff.py <token> [-v]
"""

import hashlib
import io
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
import zipfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
RUST = os.environ.get("RUST_URL", "http://127.0.0.1:8000")
PYTHON = os.environ.get("PYTHON_URL", "http://127.0.0.1:8001")
PG_CONTAINER = "cc-schema-test"
REDIS_CONTAINER = "cc-redis-test"

TOKEN = sys.argv[1]
# A non-admin caller. The read differential gained one after a mutation
# that leaks admin-only inbox rows scored 296/296; the writes had the
# same hole — every require_admin rejection on a mutating route was
# untested, which is the half where being wrong actually changes data.
MEMBER_TOKEN = sys.argv[2] if len(sys.argv) > 2 and not sys.argv[2].startswith("-") else None
VERBOSE = "-v" in sys.argv

# Timestamps this close to the request are "now" and get normalised.
RECENT_WINDOW = timedelta(minutes=10)

# Tables whose contents are compared after each write. Deliberately
# includes tables a case is not expected to touch: a handler that writes
# a stray audit row, or fails to write an expected one, is exactly the
# kind of bug response diffing misses.

# DIFF_ONLY=<substring> restricts the run to matching cases. For mutation
# runs, where a full sweep per injected bug costs minutes and only the
# slice's own cases can move. A filtered run says so in its result line,
# so a filtered green is never mistaken for a full one.
DIFF_ONLY = os.environ.get("DIFF_ONLY", "")

WATCHED = ["incidents", "incident_evidence", "audit_log", "settings",
           "camera_nodes", "stream_access_logs", "mcp_activity_logs",
           "camera_groups", "cameras", "mcp_api_keys", "sentinel_config",
           "email_outbox", "email_suppression", "processed_webhooks",
           "notifications", "user_notification_state",
           # sentinel_agent_keys is watched even though no case writes
           # to it deliberately: every agent-authenticated request
           # stamps last_used_at as a side effect, and a port that
           # skipped the stamp — or stamped the wrong row — would look
           # perfect in the response.
           "sentinel_runs", "sentinel_agent_keys"]

# The raw agent keys behind seed rows 1-3. Hashes are in
# seed_cameras.sql; these are the values a caller presents.
AGENT_KEYS = {
    "agent": "osa_00000000000000000000000000000001",
    "agent:revoked": "osa_00000000000000000000000000000002",
    "agent:theirs": "osa_00000000000000000000000000000003",
    "agent:unknown": "osa_ffffffffffffffffffffffffffffffff",
}

# (name, method, path, body) — body None means no request body.
# A 5th element "member" sends the non-admin token instead.
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
    # `group_id` has no bounds in the Python signature, so these all get
    # through validation and reach the query. 4294967297 is the one that
    # mattered: narrowed to the column's own width it wraps to 1 and
    # assigns the camera to a real group.
    ("assign, group_id past int4", "PUT", "/api/cameras/cam-live/group?group_id=4294967297", None),
    ("assign, group_id at int4 max", "PUT", "/api/cameras/cam-live/group?group_id=2147483647", None),
    ("assign, group_id past int4 negative", "PUT",
     "/api/cameras/cam-live/group?group_id=-4294967297", None),
    ("assign, group_id past i64", "PUT",
     "/api/cameras/cam-live/group?group_id=99999999999999999999", None),
    ("assign, group_id of 4300 digits", "PUT",
     "/api/cameras/cam-live/group?group_id=1" + "0" * 4300, None),

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
    # Python stores a JSON list audio_codec as psycopg's array text,
    # `{a,b}`, into a value written into an HLS CODECS attribute. Rust
    # refuses. See expected_divergences.md and PYTHON_BUGS.md #2.
    "codec: audio as a list",
}

CASES += [
    # --- a NON-ADMIN attempting every ported write ---------------------
    # require_admin must refuse, and nothing may reach the database. The
    # side-effect snapshot is the point: a handler that 403s *after*
    # writing would pass a response-only comparison.
    ("member: patch incident", "PATCH", "/api/incidents/1", {"status": "resolved"}, "member"),
    ("member: delete incident", "DELETE", "/api/incidents/1", None, "member"),
    ("member: create group", "POST", "/api/camera-groups", {"name": "Sneaky"}, "member"),
    ("member: delete group", "DELETE", "/api/camera-groups/1", None, "member"),
    ("member: assign group", "PUT", "/api/cameras/cam-live/group?group_id=1", None, "member"),
    ("member: toggle recording", "POST", "/api/cameras/cam-live/recording",
     {"recording": True}, "member"),
    ("member: recording policy", "PATCH", "/api/cameras/cam-live/recording-settings",
     {"continuous_24_7": True}, "member"),
    ("member: motion ingestion", "POST", "/api/settings/motion-ingestion",
     {"enabled": False}, "member"),
    ("member: notification settings", "POST", "/api/settings/notifications",
     {"motion_notifications": False}, "member"),
    ("member: revoke key", "DELETE", "/api/integration/keys/5", None, "member"),
    ("member: email prefs", "POST", "/api/notifications/email/preferences",
     {"email_motion": True}, "member"),
    # these two are require_view, so a member SHOULD succeed — the pair
    # proves the member token works rather than failing everything
    ("member: mark viewed", "POST", "/api/notifications/mark-viewed", None, "member"),
    ("member: clear all", "POST", "/api/notifications/clear-all", None, "member"),

    # --- notifications -------------------------------------------------
    # Each of these creates the read-state row on first touch — a write
    # on a GET, which the side-effect snapshot sees.
    ("mark viewed", "POST", "/api/notifications/mark-viewed", None),
    ("clear all", "POST", "/api/notifications/clear-all", None),
    ("email prefs, one toggle", "POST", "/api/notifications/email/preferences",
     {"email_motion": True}),
    ("email prefs, several", "POST", "/api/notifications/email/preferences",
     {"email_motion": True, "email_camera_offline": False, "email_member_audit": False}),
    ("email prefs, empty body", "POST", "/api/notifications/email/preferences", {}),
    # explicit null is a no-op, like an absent field
    ("email prefs, explicit null", "POST", "/api/notifications/email/preferences",
     {"email_motion": None}),
    # email_welcome is readable but NOT writable — the POST model has no
    # such field, so it must be ignored rather than stored
    ("email prefs, unwritable key", "POST", "/api/notifications/email/preferences",
     {"email_welcome": False}),
    ("email prefs, unknown key", "POST", "/api/notifications/email/preferences",
     {"nonsense": True}),
    ("email prefs, wrong type", "POST", "/api/notifications/email/preferences",
     {"email_motion": "nope"}),

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
    # --- the sentinel agent data plane --------------------------------
    #
    # /start claims a pending run; /complete reports a terminal outcome.
    # Both are agent-authenticated, and both must stamp last_used_at on
    # the key row that authenticated them.
    ("agent claims a pending run", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/start", None, "agent"),
    ("agent re-claims a running run", "POST",
     "/api/sentinel/runs/run0000000000000000000000000004/start", None, "agent"),
    ("agent claims a terminal run", "POST",
     "/api/sentinel/runs/run0000000000000000000000000006/start", None, "agent"),
    ("claim a missing run", "POST",
     "/api/sentinel/runs/nosuchrun/start", None, "agent"),
    # Cross-tenant: a scoped key must 404 rather than 403, so it cannot
    # be used to probe which run ids exist.
    ("claim another tenant's run", "POST",
     "/api/sentinel/runs/run0000000000000000000000000003/start", None, "agent"),
    ("claim with a revoked key", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/start", None, "agent:revoked"),
    ("claim with an unknown key", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/start", None, "agent:unknown"),
    ("claim with no key", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/start", None, "agent:none"),
    ("claim with a high-byte key", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/start", None, "agent:highbyte"),
    # A session token is not agent auth.
    ("claim with a session token", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/start", None, "admin"),

    ("complete no_action", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "summary": "nothing there"}, "agent"),
    ("complete incident", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "incident", "severity": "high", "incident_id": 1,
      "summary": "filed", "tool_call_count": 4}, "agent"),
    ("complete incident, critical", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "incident", "severity": "critical"}, "agent"),
    ("complete error", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "error", "summary": "blew up"}, "agent"),
    # severity and incident_id are dropped unless the outcome is
    # `incident`, even when the caller sends them.
    ("complete no_action with severity", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "severity": "high", "incident_id": 1}, "agent"),
    # An incident from another org must be refused: a leaked key could
    # otherwise plant a foreign deep-link in that org's run drawer.
    ("complete pointing at a foreign incident", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "incident", "severity": "low", "incident_id": 4}, "agent"),
    ("complete pointing at a missing incident", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "incident", "severity": "low", "incident_id": 9999}, "agent"),
    # Idempotency: a same-outcome retry is a no-op returning the stored
    # row, error -> real outcome is a one-way upgrade, and a real
    # outcome must not be downgraded to error.
    ("re-complete a terminal run", "POST",
     "/api/sentinel/runs/run0000000000000000000000000006/complete",
     {"outcome": "incident", "severity": "low", "summary": "changed"}, "agent"),
    ("upgrade error -> incident", "POST",
     "/api/sentinel/runs/run0000000000000000000000000005/complete",
     {"outcome": "incident", "severity": "medium", "summary": "actually found one"}, "agent"),
    ("upgrade error -> no_action", "POST",
     "/api/sentinel/runs/run0000000000000000000000000005/complete",
     {"outcome": "no_action"}, "agent"),
    ("downgrade incident -> error", "POST",
     "/api/sentinel/runs/run0000000000000000000000000006/complete",
     {"outcome": "error"}, "agent"),
    # Handler-level 400s, which run after validation.
    ("complete bad outcome", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "nope"}, "agent"),
    ("complete outcome with a quote", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "it's bad"}, "agent"),
    ("complete incident, no severity", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "incident"}, "agent"),
    ("complete incident, bad severity", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "incident", "severity": "catastrophic"}, "agent"),
    # Pydantic 422s, measured against the running service.
    ("complete, no outcome", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete", {}, "agent"),
    ("complete, null outcome", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": None}, "agent"),
    ("complete, outcome not a string", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": 5}, "agent"),
    ("complete, summary too long", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "summary": "x" * 8001}, "agent"),
    ("complete, summary at the cap", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "summary": "y" * 8000}, "agent"),
    ("complete, null summary", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "summary": None}, "agent"),
    ("complete, count not an int", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": "abc"}, "agent"),
    ("complete, count a whole float", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": 3.0}, "agent"),
    ("complete, count a fractional float", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": 2.7}, "agent"),
    ("complete, count a numeric string", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": "1_000"}, "agent"),
    ("complete, count a decimal string", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": "7.0"}, "agent"),
    ("complete, count in exponent form", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": "1e3"}, "agent"),
    # Past the column: SQLAlchemy binds the count as an `integer`, so
    # Postgres refuses it and the request is a 500 — not a row quietly
    # clamped to 2147483647. A large *negative* count is different:
    # `max(0, ...)` in the handler turns it into 0 first.
    ("complete, count past int4", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": 3000000000}, "agent"),
    ("complete, count past int4 as a string", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": "3000000000"}, "agent"),
    ("complete, count past i64 as a string", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": "99999999999999999999"}, "agent"),
    ("complete, count negative past i64", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": "-99999999999999999999"}, "agent"),
    ("complete, count of 4300 digits", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": "1" + "0" * 4300}, "agent"),
    ("complete, incident_id past int4", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "incident", "incident_id": 3000000000}, "agent"),
    ("complete, incident_id past i64", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "incident", "incident_id": "99999999999999999999"}, "agent"),
    # The same value on an outcome that does not store it: Python drops
    # `incident_id` before it reaches the column, so this is a 200 and
    # not the 500 the two cases above are.
    ("complete, incident_id past int4 but no incident", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "incident_id": 3000000000}, "agent"),
    ("complete, count a bool", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": True}, "agent"),
    ("complete, count null", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": None}, "agent"),
    ("complete, negative count", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": -5}, "agent"),
    ("complete, count a list", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_call_count": []}, "agent"),
    ("complete, trace not a list", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_trace": "x"}, "agent"),
    ("complete, trace items not dicts", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_trace": [1, "a"]}, "agent"),
    # The stored trace is capped twice: last fifty entries, and each
    # entry's fields cut to length with an ellipsis.
    ("complete with a trace", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action",
      "tool_trace": [{"tool": "get_camera", "args": {"camera_id": "cam-live"},
                      "result": "ok"}]}, "agent"),
    ("complete, trace over fifty entries", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action",
      "tool_trace": [{"tool": f"t{i}", "args": {"i": i}, "result": "r"}
                     for i in range(60)]}, "agent"),
    ("complete, trace with oversized fields", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action",
      "tool_trace": [{"tool": "n" * 300, "args": {"blob": "a" * 2000},
                      "result": "r" * 1500}]}, "agent"),
    ("complete, trace entries missing keys", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action", "tool_trace": [{}]}, "agent"),
    ("complete, trace with non-string fields", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action",
      "tool_trace": [{"tool": 7, "args": "not a dict", "result": None}]}, "agent"),
    ("complete, trace with non-ascii", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": "no_action",
      "tool_trace": [{"tool": "caf\u00e9", "args": {"n": "\u00e9"},
                      "result": "\U0001F3A5"}]}, "agent"),
    ("complete an empty body", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete", None, "agent"),
    ("complete another tenant's run", "POST",
     "/api/sentinel/runs/run0000000000000000000000000003/complete",
     {"outcome": "no_action"}, "agent"),
    ("complete a missing run", "POST",
     "/api/sentinel/runs/nosuchrun/complete", {"outcome": "no_action"}, "agent"),
    # Bad key AND bad body: auth is a dependency, so it must be refused
    # 401 before the body is ever validated. Getting the order backwards
    # would leak which fields a caller got wrong to someone who cannot
    # authenticate at all.
    ("bad key and bad body", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": 5, "summary": None}, "agent:unknown"),
    ("no key and bad body", "POST",
     "/api/sentinel/runs/run0000000000000000000000000001/complete",
     {"outcome": 5}, "agent:none"),

    # --- bodies that are valid JSON but not an object -----------------
    #
    # Two behaviours, depending on how the Python route reads its body.
    # A declared Pydantic model 422s before the handler runs: a list or
    # a string is `model_attributes_type`, and `null` is `missing`. A
    # handler that calls `await request.json()` itself has nothing in
    # front of it, so malformed JSON and a non-object body are both an
    # unhandled 500. parse_body accepted any JSON value, so every
    # model-body route read `[1]` as an object with no fields.
    *[(f"body shape: {label} {raw!r}", method, path, raw, who)
      for label, method, path, who in [
          ("create group", "POST", "/api/camera-groups", "admin"),
          ("notification settings", "POST", "/api/settings/notifications", "admin"),
          ("recording policy", "PATCH", "/api/cameras/cam-live/recording-settings", "admin"),
          ("patch incident", "PATCH", "/api/incidents/1", "admin"),
          ("email preferences", "POST", "/api/notifications/email/preferences", "admin"),
          ("login", "POST", "/api/auth/local/login", "agent:none"),
          ("refresh", "POST", "/api/auth/local/refresh", "agent:none"),
          ("complete a run", "POST",
           "/api/sentinel/runs/run0000000000000000000000000001/complete", "agent"),
          ("toggle recording", "POST", "/api/cameras/cam-live/recording", "admin"),
          ("motion ingestion", "POST", "/api/settings/motion-ingestion", "admin"),
      ]
      for raw in (b"[1]", b'"x"', b"null", b"{x", b"")],

    # --- decode order: malformed JSON is refused before auth -----------
    #
    # FastAPI decodes a declared model body before resolving any
    # dependency, and checks its shape after. So with no credentials,
    # `{x` is a 422 but `[1]` and an empty body are a 401. On the agent
    # routes the difference reaches the database: a malformed body never
    # runs the dependency that stamps the key's last_used_at.
    *[(f"body order: {label} {raw!r} as {who}", method, path, raw, who)
      for label, method, path, whos in [
          ("create group", "POST", "/api/camera-groups", ("agent:none", "member")),
          ("patch incident", "PATCH", "/api/incidents/1", ("agent:none",)),
          ("recording policy", "PATCH", "/api/cameras/cam-live/recording-settings", ("agent:none",)),
          ("complete a run", "POST",
           "/api/sentinel/runs/run0000000000000000000000000001/complete",
           ("agent:none", "agent:revoked")),
          # No declared model: the body is never decoded before auth.
          ("toggle recording", "POST", "/api/cameras/cam-live/recording", ("agent:none",)),
      ]
      for who in whos
      for raw in (b"{x", b"[1]", b"")],

    # --- CameraNode key routes: validate and codec --------------------
    #
    # Input no real CameraNode sends is here on purpose. These handlers
    # read their body with `await request.json()` and branch on Python
    # truthiness and `len()`, so a number where a string belongs is a
    # 500 (len(5) raises outside any try), a list is a 404 whose message
    # spells the list the way Python's str() does, and a mixed-type list
    # is a 500 because psycopg cannot bind it.
    *[(f"validate: {label}", "POST", "/api/nodes/validate", body, who)
      for label, body, who in [
          ("ok", {"node_id": "node-aaaa1111"}, "node:test-node-key"),
          ("no key", {"node_id": "node-aaaa1111"}, "agent:none"),
          ("empty key", {"node_id": "node-aaaa1111"}, "node:"),
          ("wrong key records the error", {"node_id": "node-aaaa1111"}, "node:wrong"),
          ("another org's node", {"node_id": "node-cccc3333"}, "node:test-node-key"),
          ("high-byte key hashed as UTF-8", {"node_id": "node-dddd4444"}, "node:node-key-\u00ff"),
          ("high-byte key against a raw-byte hash", {"node_id": "node-eeee5555"}, "node:node-key-\u00ff"),
          ("missing node", {"node_id": "nope"}, "node:test-node-key"),
          ("malformed json", b"{x", "node:test-node-key"),
          ("empty body", b"", "node:test-node-key"),
          ("list body", [1, 2], "node:test-node-key"),
          ("string body", "x", "node:test-node-key"),
          ("no node_id", {}, "node:test-node-key"),
          ("node_id 0", {"node_id": 0}, "node:test-node-key"),
          ("node_id false", {"node_id": False}, "node:test-node-key"),
          ("node_id empty list", {"node_id": []}, "node:test-node-key"),
          ("node_id int", {"node_id": 123}, "node:test-node-key"),
          ("node_id float", {"node_id": 1.5}, "node:test-node-key"),
          ("node_id true", {"node_id": True}, "node:test-node-key"),
          ("node_id dict", {"node_id": {"a": 1}}, "node:test-node-key"),
          ("node_id list", {"node_id": ["a"]}, "node:test-node-key"),
          ("node_id list with null", {"node_id": ["a", None]}, "node:test-node-key"),
          ("node_id list with a quote", {"node_id": ["it's"]}, "node:test-node-key"),
          ("node_id list of ints", {"node_id": [1, 2]}, "node:test-node-key"),
          ("node_id list of floats", {"node_id": [1.5]}, "node:test-node-key"),
          ("node_id int and float", {"node_id": [1, 1.5]}, "node:test-node-key"),
          ("node_id bool and int", {"node_id": [True, 1]}, "node:test-node-key"),
          ("node_id ragged ints", {"node_id": [[1], [2, 3]]}, "node:test-node-key"),
          ("node_id ragged strings", {"node_id": [["a"], "b"]}, "node:test-node-key"),
          ("node_id nested strings", {"node_id": [["a"], ["b"]]}, "node:test-node-key"),
          ("node_id list of dict", {"node_id": [{"a": 1}]}, "node:test-node-key"),
      ]],
    *[(f"codec: {label}", "POST", path, body, who)
      for label, path, body, who in [
          ("ok", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64001f"}, "node:test-node-key"),
          ("with audio", "/api/cameras/cam-live/codec",
           {"video_codec": "avc1.64001f", "audio_codec": "opus"}, "node:test-node-key"),
          ("no key", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64001f"}, "agent:none"),
          ("empty key", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64001f"}, "node:"),
          ("bad key", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64001f"}, "node:wrong"),
          ("another node's camera", "/api/cameras/cam-failed/codec", {"video_codec": "avc1.64001f"}, "node:test-node-key"),
          ("missing camera", "/api/cameras/nope/codec", {"video_codec": "avc1.64001f"}, "node:test-node-key"),
          ("missing camera and a bad body", "/api/cameras/nope/codec", b"{x", "node:test-node-key"),
          ("malformed json", "/api/cameras/cam-live/codec", b"{x", "node:test-node-key"),
          ("empty body", "/api/cameras/cam-live/codec", b"", "node:test-node-key"),
          ("null body", "/api/cameras/cam-live/codec", b"null", "node:test-node-key"),
          ("list body", "/api/cameras/cam-live/codec", [1], "node:test-node-key"),
          ("no video", "/api/cameras/cam-live/codec", {"audio_codec": "mp4a.40.2"}, "node:test-node-key"),
          ("video empty", "/api/cameras/cam-live/codec", {"video_codec": ""}, "node:test-node-key"),
          ("video 0", "/api/cameras/cam-live/codec", {"video_codec": 0}, "node:test-node-key"),
          ("video int", "/api/cameras/cam-live/codec", {"video_codec": 5}, "node:test-node-key"),
          ("video list", "/api/cameras/cam-live/codec", {"video_codec": ["a"]}, "node:test-node-key"),
          ("video list holding a newline", "/api/cameras/cam-live/codec", {"video_codec": ["\n"]}, "node:test-node-key"),
          ("video list of 65", "/api/cameras/cam-live/codec", {"video_codec": ["a"] * 65}, "node:test-node-key"),
          ("video dict keyed by newline", "/api/cameras/cam-live/codec", {"video_codec": {"\n": 1}}, "node:test-node-key"),
          ("video dict", "/api/cameras/cam-live/codec", {"video_codec": {"a": 1}}, "node:test-node-key"),
          ("video 65 chars", "/api/cameras/cam-live/codec", {"video_codec": "a" * 65}, "node:test-node-key"),
          ("video 55 chars overflows the column", "/api/cameras/cam-live/codec", {"video_codec": "a" * 55}, "node:test-node-key"),
          # len() counts characters. 40 "\u00e9" is 80 bytes: past 64 as bytes,
          # well within it as characters, so only a byte-counting port 400s it.
          ("video 40 accented chars", "/api/cameras/cam-live/codec", {"video_codec": "\u00e9" * 40}, "node:test-node-key"),
          ("audio 40 accented chars", "/api/cameras/cam-live/codec",
           {"video_codec": "avc1.64001f", "audio_codec": "\u00e9" * 40}, "node:test-node-key"),
          ("video newline", "/api/cameras/cam-live/codec", {"video_codec": "avc1\n"}, "node:test-node-key"),
          ("video carriage return", "/api/cameras/cam-live/codec", {"video_codec": "avc1\r"}, "node:test-node-key"),
          ("audio int", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64001f", "audio_codec": 7}, "node:test-node-key"),
          ("audio 0 takes the default", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64001f", "audio_codec": 0}, "node:test-node-key"),
          ("audio empty takes the default", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64001f", "audio_codec": ""}, "node:test-node-key"),
          ("audio empty list takes the default", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64001f", "audio_codec": []}, "node:test-node-key"),
          ("audio 65 chars", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64001f", "audio_codec": "a" * 65}, "node:test-node-key"),
          ("audio as a list", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64001f", "audio_codec": ["a", "b"]}, "node:test-node-key"),
          ("audio list holding a newline", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64001f", "audio_codec": ["\n"]}, "node:test-node-key"),
          ("audio format beats a bad video type", "/api/cameras/cam-live/codec", {"video_codec": ["a"], "audio_codec": "a" * 65}, "node:test-node-key"),
          # sanitize_video_codec: level below 2.0 is upgraded to 3.0
          ("level 1.0 upgraded", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64000a"}, "node:test-node-key"),
          ("negative level upgraded", "/api/cameras/cam-live/codec", {"video_codec": "avc1.6400-1"}, "node:test-node-key"),
          ("space-padded level upgraded", "/api/cameras/cam-live/codec", {"video_codec": "avc1.6400 1"}, "node:test-node-key"),
          ("uppercase level upgraded", "/api/cameras/cam-live/codec", {"video_codec": "avc1.64000A"}, "node:test-node-key"),
          ("level exactly 2.0 kept", "/api/cameras/cam-live/codec", {"video_codec": "avc1.640014"}, "node:test-node-key"),
          ("non-hex level kept", "/api/cameras/cam-live/codec", {"video_codec": "avc1.6400zz"}, "node:test-node-key"),
          ("ten characters kept", "/api/cameras/cam-live/codec", {"video_codec": "avc1.6400a"}, "node:test-node-key"),
          ("not avc1 kept", "/api/cameras/cam-live/codec", {"video_codec": "hvc1.1.6.L93"}, "node:test-node-key"),
          # node inheritance
          ("high-byte key, node already has a codec", "/api/cameras/cam-dddd/codec",
           {"video_codec": "avc1.64001f"}, "node:node-key-\u00ff"),
          ("high-byte key, the raw-byte node's camera", "/api/cameras/cam-eeee/codec",
           {"video_codec": "avc1.64001f"}, "node:node-key-\u00ff"),
          ("node with an empty codec inherits", "/api/cameras/cam-ffff/codec",
           {"video_codec": "avc1.64001f"}, "node:node-key-empty-codec"),
      ]],

    # --- node management ---------------------------------------------
    ("rotate key", "POST", "/api/nodes/node-aaaa1111/rotate-key", None),
    ("rotate another org's key", "POST", "/api/nodes/node-cccc3333/rotate-key", None),
    ("rotate a missing node's key", "POST", "/api/nodes/nope/rotate-key", None),
    ("member: rotate key", "POST", "/api/nodes/node-aaaa1111/rotate-key", None, "member"),
    ("anon: rotate key", "POST", "/api/nodes/node-aaaa1111/rotate-key", None, "agent:none"),
    ("create node", "POST", "/api/nodes", {"name": "Garage"}),
    ("create node, no name", "POST", "/api/nodes", {}),
    ("create node, null name", "POST", "/api/nodes", {"name": None}),
    ("create node, empty name", "POST", "/api/nodes", {"name": ""}),
    ("create node, 100-char name", "POST", "/api/nodes", {"name": "x" * 100}),
    ("create node, 101-char name", "POST", "/api/nodes", {"name": "x" * 101}),
    ("create node, int name", "POST", "/api/nodes", {"name": 5}),
    ("create node, list body", "POST", "/api/nodes", [1]),
    ("create node, malformed json", "POST", "/api/nodes", b"{x"),
    ("create node, empty body", "POST", "/api/nodes", b""),
    ("member: create node", "POST", "/api/nodes", {"name": "x"}, "member"),
    ("anon: create node, malformed json", "POST", "/api/nodes", b"{x", "agent:none"),
    ("wipe logs", "POST", "/api/settings/danger/wipe-logs", None),
    ("member: wipe logs", "POST", "/api/settings/danger/wipe-logs", None, "member"),
    ("anon: wipe logs", "POST", "/api/settings/danger/wipe-logs", None, "agent:none"),

    # --- Home Assistant recording switch -------------------------------
    *[(f"integration recording: {label}", "POST", path, body, who)
      for label, path, body, who in [
          ("on", "/api/integration/cameras/cam-live/recording", {"recording": True}, "bearer:osi_live_integration_key"),
          # cam-live is seeded recording already; setting the same value
          # emits no UPDATE, so updated_at must not move
          ("unchanged", "/api/integration/cameras/cam-stale/recording", {"recording": False}, "bearer:osi_live_integration_key"),
          ("off", "/api/integration/cameras/cam-live/recording", {"recording": False}, "bearer:osi_live_integration_key"),
          ("no field", "/api/integration/cameras/cam-live/recording", {}, "bearer:osi_live_integration_key"),
          ("truthy string", "/api/integration/cameras/cam-stale/recording", {"recording": "yes"}, "bearer:osi_live_integration_key"),
          ("zero", "/api/integration/cameras/cam-live/recording", {"recording": 0}, "bearer:osi_live_integration_key"),
          ("list body", "/api/integration/cameras/cam-live/recording", [1], "bearer:osi_live_integration_key"),
          ("malformed body", "/api/integration/cameras/cam-live/recording", b"{x", "bearer:osi_live_integration_key"),
          ("missing camera", "/api/integration/cameras/nope/recording", {"recording": True}, "bearer:osi_live_integration_key"),
          ("another org's camera", "/api/integration/cameras/cam-theirs/recording", {"recording": True}, "bearer:osi_live_integration_key"),
          ("revoked key", "/api/integration/cameras/cam-live/recording", {"recording": True}, "bearer:osi_revoked_integration"),
          ("mcp key", "/api/integration/cameras/cam-live/recording", {"recording": True}, "bearer:osc_mcp_kind_key"),
          ("no key", "/api/integration/cameras/cam-live/recording", {"recording": True}, "agent:none"),
          ("session token", "/api/integration/cameras/cam-live/recording", {"recording": True}, "admin"),
      ]],

    # --- Sentinel configuration ----------------------------------------
    #
    # PATCH applies fields in Pydantic's declaration order, not the
    # body's, so that order decides which 400 comes back and how the
    # audit's `changes` list reads. Unknown day keys are dropped rather
    # than rejected, camera_scope values go through Python's bool(), and
    # a field sent as null is skipped rather than written.
    *[(f"sentinel config: {label}", "PATCH", "/api/sentinel/config", body, who, setup)
      for label, body, who, setup in [
          ("disable", {"enabled": False}, "admin", None),
          ("motion off", {"motion_enabled": False}, "admin", None),
          ("incident off", {"incident_opened_enabled": False}, "admin", None),
          ("cooldown", {"motion_cooldown_min": 10}, "admin", None),
          ("cooldown at the floor", {"motion_cooldown_min": 1}, "admin", None),
          ("cooldown at the ceiling", {"motion_cooldown_min": 60}, "admin", None),
          ("cooldown below the floor", {"motion_cooldown_min": 0}, "admin", None),
          ("cooldown above the ceiling", {"motion_cooldown_min": 61}, "admin", None),
          ("cooldown not a number", {"motion_cooldown_min": "abc"}, "admin", None),
          ("cooldown as a numeric string", {"motion_cooldown_min": "10"}, "admin", None),
          ("schedule mode", {"schedule_mode": "scheduled"}, "admin", None),
          ("schedule mode off", {"schedule_mode": "off"}, "admin", None),
          ("invalid schedule mode", {"schedule_mode": "sometimes"}, "admin", None),
          ("schedule mode with a quote", {"schedule_mode": "it's"}, "admin", None),
          ("schedule mode not a string", {"schedule_mode": 5}, "admin", None),
          ("schedule start", {"schedule_start": "08:30"}, "admin", None),
          ("schedule start single digit", {"schedule_start": "8:30"}, "admin", None),
          ("schedule start space padded", {"schedule_start": " 8:30"}, "admin", None),
          ("schedule start out of range", {"schedule_start": "24:00"}, "admin", None),
          # Only the length check refuses these: "08:3" would otherwise
          # parse as 08 and 3, and "08:300" as a minute out of range —
          # a different message.
          ("schedule start too short", {"schedule_start": "08:3"}, "admin", None),
          ("schedule start too long", {"schedule_start": "08:300"}, "admin", None),
          ("schedule end out of range", {"schedule_end": "12:60"}, "admin", None),
          ("schedule end non-numeric", {"schedule_end": "aa:bb"}, "admin", None),
          ("active days", {"active_days": ["mon", "tue"]}, "admin", None),
          ("active days with an unknown key", {"active_days": ["mon", "xyz"]}, "admin", None),
          ("active days empty", {"active_days": []}, "admin", None),
          ("active days not a list", {"active_days": "mon"}, "admin", None),
          ("active days with a number", {"active_days": ["mon", 3]}, "admin", None),
          ("camera scope", {"camera_scope": {"cam-live": True}}, "admin", None),
          ("camera scope truthiness", {"camera_scope": {"cam-live": 1, "cam-stale": ""}}, "admin", None),
          ("camera scope not an object", {"camera_scope": ["cam-live"]}, "admin", None),
          ("several fields", {"enabled": False, "schedule_mode": "off", "motion_cooldown_min": 7}, "admin", None),
          ("a bad field after a good one", {"enabled": False, "schedule_mode": "nope"}, "admin", None),
          ("empty patch writes nothing", {}, "admin", None),
          ("explicit nulls are skipped", {"enabled": None, "schedule_mode": None}, "admin", None),
          ("unknown fields are ignored", {"nope": 1}, "admin", None),
          ("member", {"enabled": False}, "member", None),
          ("anon", {"enabled": False}, "agent:none", None),
          ("unlicensed", {"enabled": False}, "admin", "DELETE FROM settings WHERE org_id='self-host' AND key LIKE 'sentinel_license%';"),
      ]],
    # The gated view: 200, not 402, with plan_gated set, the reason, and
    # a cap of zero even though self_host nominally carries one. The
    # read differential has no per-case setup, so it is run from here.
    ("sentinel config: unlicensed GET", "GET", "/api/sentinel/config", None, "admin",
     "DELETE FROM settings WHERE org_id='self-host' AND key LIKE 'sentinel_license%';"),

    # --- GDPR Article 20 export -----------------------------------------
    # The archive is compared by contents, not bytes: member names in
    # order and each member's JSON. `exported_at` in the manifest and
    # the audit row's filename both carry today's date, which is the
    # same on both stacks.
    ("gdpr export", "POST", "/api/gdpr/export", None, "admin", None),
    ("gdpr export: member", "POST", "/api/gdpr/export", None, "member", None),
    ("gdpr export: anon", "POST", "/api/gdpr/export", None, "agent:none", None),
    # A table with nothing in it still gets a file, holding `[]`.
    ("gdpr export: an empty table", "POST", "/api/gdpr/export", None, "admin",
     "DELETE FROM motion_events; DELETE FROM sentinel_runs;"),
    # Cameras are exported by walking each node's cameras, not by the
    # camera's own org_id — so a row whose org_id disagrees with its
    # node's is still in the export, and one that belongs to no node of
    # this org is not.
    ("gdpr export: a camera whose org_id disagrees", "POST", "/api/gdpr/export", None, "admin",
     "UPDATE cameras SET org_id = 'other-org' WHERE camera_id = 'cam-live';"),
    # Two incidents with evidence, to pin the per-parent order the
    # relationship load produces.
    ("gdpr export: evidence order", "POST", "/api/gdpr/export", None, "admin",
     "INSERT INTO incident_evidence (id, incident_id, kind, text, camera_id, timestamp)"
     " VALUES (900, 2, 'observation', 'late', 'cam-live', timestamp '2026-06-02 00:00:00'),"
     "        (901, 1, 'observation', 'early', 'cam-live', timestamp '2026-06-01 00:00:00');"),

    # --- org timezone ---------------------------------------------------
    # The name is checked against `zoneinfo.available_timezones()`, which
    # is the tzdata package's list plus a walk of the system directories
    # — not a fixed list, and not a Rust crate's idea of one.
    *[(f"timezone: {label}", "POST", "/api/settings/timezone", body, who, None)
      for label, body, who in [
          ("ok", {"timezone": "America/Los_Angeles"}, "admin"),
          ("UTC", {"timezone": "UTC"}, "admin"),
          ("a link name", {"timezone": "US/Pacific"}, "admin"),
          ("Factory", {"timezone": "Factory"}, "admin"),
          ("surrounding whitespace", {"timezone": "  UTC  "}, "admin"),
          ("inner whitespace", {"timezone": "America/Los Angeles"}, "admin"),
          ("wrong case", {"timezone": "utc"}, "admin"),
          ("made up", {"timezone": "Mars/Olympus_Mons"}, "admin"),
          # A directory of the tzdata package, which `available_timezones`
          # does not list.
          ("a zone directory", {"timezone": "America"}, "admin"),
          ("posixrules", {"timezone": "posixrules"}, "admin"),
          ("a pruned directory", {"timezone": "right/UTC"}, "admin"),
          ("path traversal", {"timezone": "../etc/passwd"}, "admin"),
          ("absolute path", {"timezone": "/etc/passwd"}, "admin"),
          ("empty", {"timezone": ""}, "admin"),
          ("missing", {}, "admin"),
          ("null", {"timezone": None}, "admin"),
          ("not a string", {"timezone": 5}, "admin"),
          ("a list", {"timezone": ["UTC"]}, "admin"),
          ("member", {"timezone": "UTC"}, "member"),
          ("anon", {"timezone": "UTC"}, "agent:none"),
      ]],

    # --- runs list, whose "today" is midnight in the org's zone ---------
    # Read cases, here rather than in the read differential, because each
    # needs its own timezone row first. Kiritimati (+14) and Niue (-11)
    # put local midnight on either side of the UTC one, so a port that
    # used UTC midnight regardless reports a different `runs_today`.
    #
    # The runs are anchored to `current_date`, not to `now()`: each case
    # is seeded once per stack, so a `now()` here would give the two
    # passes different rows and the side-effect snapshot would differ on
    # every case. The odd minute offsets keep them clear of any zone's
    # local midnight, which is what the two stacks compute milliseconds
    # apart.
    *[(f"runs list: {label}", "GET", "/api/sentinel/runs", None, "admin",
       ("DELETE FROM settings WHERE org_id='self-host' AND key='timezone';"
        "INSERT INTO settings (id, org_id, key, value, updated_at) "
        f"VALUES (9001, 'self-host', 'timezone', '{tz}', timestamp '2026-06-01 00:00:00');"
        "INSERT INTO sentinel_runs (id, org_id, triggered_at, trigger_type, tool_call_count,"
        " outcome, summary, updated_at) VALUES"
        " (md5('h1'), 'self-host', current_date + interval '3 hours 17 minutes', 'motion', 0, 'no_action', '', timestamp '2026-06-01 00:00:00'),"
        " (md5('h2'), 'self-host', current_date + interval '9 hours 43 minutes', 'motion', 0, 'no_action', '', timestamp '2026-06-01 00:00:00'),"
        " (md5('h3'), 'self-host', current_date + interval '15 hours 7 minutes', 'motion', 0, 'no_action', '', timestamp '2026-06-01 00:00:00'),"
        " (md5('h4'), 'self-host', current_date + interval '21 hours 31 minutes', 'motion', 0, 'no_action', '', timestamp '2026-06-01 00:00:00'),"
        " (md5('h5'), 'self-host', current_date - interval '5 hours 23 minutes', 'motion', 0, 'no_action', '', timestamp '2026-06-01 00:00:00'),"
        " (md5('h6'), 'self-host', current_date - interval '13 hours 11 minutes', 'motion', 0, 'no_action', '', timestamp '2026-06-01 00:00:00');"))
      for label, tz in [
          ("UTC", "UTC"),
          ("Los Angeles", "America/Los_Angeles"),
          ("Kolkata, a half-hour zone", "Asia/Kolkata"),
          ("Kiritimati, +14", "Pacific/Kiritimati"),
          ("Niue, -11", "Pacific/Niue"),
          ("Havana, whose midnight repeats", "America/Havana"),
          ("Santiago, whose midnight is skipped", "America/Santiago"),
          # Not a zone: falls back to UTC rather than erroring.
          ("an unknown name", "Mars/Olympus_Mons"),
          ("an empty name", ""),
          ("a traversal attempt", "../etc/passwd"),
          # A directory of the tzdata package — the one name that is a
          # 500 rather than a fallback.
          ("a zone directory", "America"),
      ]],

    # Past the monthly cap, which is the only way to tell `max(0, cap -
    # used)` from a bare subtraction: the fixture's org has seven runs
    # against a cap of five hundred.
    ("runs list: past the monthly cap", "GET", "/api/sentinel/runs", None, "admin",
     "INSERT INTO sentinel_runs (id, org_id, triggered_at, trigger_type, tool_call_count,"
     " outcome, summary, updated_at) SELECT md5('cap' || g::text), 'self-host',"
     " date_trunc('month', now())::timestamp + (g || ' seconds')::interval, 'motion', 0,"
     " 'no_action', '', timestamp '2026-06-01 00:00:00' FROM generate_series(1, 501) g;"),

    # --- Run now --------------------------------------------------------
    *[(f"manual run: {label}", "POST", "/api/sentinel/runs/manual", body, who, setup)
      for label, body, who, setup in [
          ("ok", {"prompt": "check the door"}, "admin", None),
          ("no body fields", {}, "admin", None),
          ("with a camera", {"prompt": "look", "camera_id": "cam-live"}, "admin", None),
          ("camera that does not exist", {"prompt": "look", "camera_id": "nope"}, "admin", None),
          ("prompt at the cap", {"prompt": "x" * 2000}, "admin", None),
          ("prompt past the cap", {"prompt": "x" * 2001}, "admin", None),
          ("prompt null", {"prompt": None}, "admin", None),
          ("prompt not a string", {"prompt": 5}, "admin", None),
          ("camera_id not a string", {"camera_id": 5}, "admin", None),
          ("member", {"prompt": "x"}, "member", None),
          ("anon", {"prompt": "x"}, "agent:none", None),
          ("unlicensed", {"prompt": "x"}, "admin", "DELETE FROM settings WHERE org_id='self-host' AND key LIKE 'sentinel_license%';"),
          ("monthly cap reached", {"prompt": "x"}, "admin", "INSERT INTO sentinel_runs (id, org_id, triggered_at, trigger_type, tool_call_count, outcome, summary, updated_at) SELECT md5(g::text), 'self-host', now()::timestamp, 'motion', 0, 'no_action', '', now()::timestamp FROM generate_series(1, 500) g;"),
      ]],

    # --- Resend delivery webhook ---------------------------------------
    #
    # The Svix signature is the whole security boundary: an unverified
    # bounce could suppress any address. A bounce or complaint
    # suppresses its recipients — lowercased and stripped, duplicates
    # swallowed — and marks the originating outbox row, but only one
    # still 'sent'. A retried delivery is answered "duplicate".
    *[(f"resend: {label}", "POST", "/api/webhooks/resend", body, who)
      for label, body, who in [
          ("bounce", {"type": "email.bounced",
                      "data": {"email_id": "em_sent", "to": ["new@example.com"]}}, "svix:msg_1"),
          ("complaint, to as a string", {"type": "email.complained",
                      "data": {"email_id": "em_sent", "to": "angry@example.com"}}, "svix:msg_2"),
          ("bounce of an address already suppressed",
           {"type": "email.bounced", "data": {"to": ["already@example.com", "fresh@example.com"]}},
           "svix:msg_3"),
          ("bounce normalises the address",
           {"type": "email.bounced", "data": {"to": ["  Mixed@Example.COM "]}}, "svix:msg_4"),
          ("bounce with junk recipients",
           {"type": "email.bounced", "data": {"to": ["nope", 5, None, "ok@example.com"]}}, "svix:msg_5"),
          ("bounce with no recipients", {"type": "email.bounced", "data": {}}, "svix:msg_6"),
          ("bounce of a row still pending",
           {"type": "email.bounced", "data": {"email_id": "em_pending"}}, "svix:msg_7"),
          ("bounce of an unknown message", {"type": "email.bounced", "data": {"email_id": "em_nope"}}, "svix:msg_8"),
          ("bounce of another org's message",
           {"type": "email.bounced", "data": {"email_id": "em_theirs"}}, "svix:msg_9"),
          ("bounce with a numeric email_id",
           {"type": "email.bounced", "data": {"email_id": 5, "to": ["n@example.com"]}}, "svix:msg_10"),
          ("delivered", {"type": "email.delivered", "data": {"email_id": "em_sent"}}, "svix:msg_11"),
          ("an unknown event", {"type": "email.opened", "data": {"email_id": "em_sent"}}, "svix:msg_12"),
          ("no type", {"data": {"email_id": "em_sent"}}, "svix:msg_13"),
          ("a numeric type", {"type": 5, "data": {}}, "svix:msg_14"),
          ("a boolean type", {"type": True}, "svix:msg_15"),
          ("a list type", {"type": ["email.bounced"]}, "svix:msg_16"),
          ("bounce with data as a list", {"type": "email.bounced", "data": [1]}, "svix:msg_17"),
          ("bounce with data empty list", {"type": "email.bounced", "data": []}, "svix:msg_18"),
          ("delivered with data as a string", {"type": "email.delivered", "data": "x"}, "svix:msg_19"),
          ("an unknown event with data as a list", {"type": "email.opened", "data": [1]}, "svix:msg_20"),
          ("a retried delivery", {"type": "email.bounced", "data": {"to": ["dup@example.com"]}},
           "svix:msg_already_seen"),
          ("webhook-* header spelling", {"type": "email.bounced", "data": {"to": ["w@example.com"]}},
           "svix-webhook:msg_21"),
          ("one of two signatures valid", {"type": "email.delivered", "data": {}}, "svix-rotated:msg_22"),
          ("a bad signature", {"type": "email.bounced", "data": {"to": ["x@example.com"]}}, "svix-bad:msg_23"),
          ("a stale timestamp", {"type": "email.bounced", "data": {"to": ["x@example.com"]}}, "svix-stale:msg_24"),
          ("a future timestamp", {"type": "email.bounced", "data": {"to": ["x@example.com"]}}, "svix-future:msg_25"),
          ("no signature header", {"type": "email.bounced", "data": {"to": ["x@example.com"]}}, "svix-nosig:msg_26"),
          ("no svix headers at all", {"type": "email.bounced", "data": {"to": ["x@example.com"]}}, "agent:none"),
          ("signed malformed json", b"{x", "svix:msg_27"),
          ("signed list body", [1, 2], "svix:msg_28"),
          ("signed empty body", b"", "svix:msg_29"),
      ]],

    ("revoke integration key", "DELETE", "/api/integration/keys/5", None),
    # The audit row this writes carries the key's name in its details
    # JSON, and that name is non-ASCII on purpose — see seed row 11.
    ("revoke a key with a non-ascii name", "DELETE", "/api/integration/keys/11", None),
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


class FixtureError(RuntimeError):
    """The database did not end up in the state a case assumes."""


def docker_psql(args, *, input=None, what):
    """Run psql inside the test container, with a timeout and retries.

    `docker exec -i` occasionally hangs in a futex wait before the
    command ever reaches Postgres — observed once for 13 minutes, with
    nothing in pg_stat_activity and every other `docker exec` working.
    Without a timeout that stalls a whole mutation run silently. Three
    attempts, then a FixtureError rather than a hang.
    """
    last = None
    for attempt in range(3):
        try:
            r = subprocess.run(
                ["docker", "exec", "-i", PG_CONTAINER, "psql", "-U", "cc", "-d", "cc",
                 "-v", "ON_ERROR_STOP=1", *args],
                input=input, capture_output=True, text=True, check=False, timeout=120,
            )
        except subprocess.TimeoutExpired:
            last = f"{what}: docker exec timed out (attempt {attempt + 1}/3)"
            print(f"  WARNING {last}", flush=True)
            continue
        if r.returncode != 0 or "ERROR" in r.stderr:
            raise FixtureError(f"{what} failed ({r.returncode}): {r.stderr.strip()[:600]}")
        return r.stdout
    raise FixtureError(last or f"{what} failed")


def psql(sql):
    # ON_ERROR_STOP plus a checked exit, not `check=False`. A failed
    # query used to return empty stdout, which `snapshot()` read as an
    # empty table — so a query error on one side and a real empty table
    # on the other compared as a side-effect diff, and a query error on
    # both compared as agreement.
    return docker_psql(["-tAq", "-c", sql], what=f"query {sql[:80]!r}")


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
-- Anchored to a fixed base but kept DISTINCT per row: the inbox pages
-- with ORDER BY created_at DESC and no tiebreaker, so collapsing these
-- to one constant would trade a reseed-drift diff for a flaky one.
UPDATE notifications
   SET created_at = timestamp '2026-09-01 00:00:00' + (id || ' minutes')::interval;
-- The read differential seeds this one relative to now(), to land a
-- caller's unread count between the two display thresholds. Relative is
-- exactly wrong here: the two reseeds happen seconds apart and every
-- row that references it drifts. Third time this pattern has bitten —
-- cameras.last_seen, notifications.created_at, and now this.
UPDATE user_notification_state
   SET last_viewed_at = timestamp '2026-06-01 00:00:00'
 WHERE last_viewed_at IS NOT NULL;
-- Fourth time: the log tables are seeded relative to now() for the read
-- differential's time windows, and they only drifted into view here when
-- wipe-logs put them in WATCHED. Re-based by rank rather than shifted by
-- a now() delta — each seed statement gets its own now(), so a delta
-- would leave millisecond drift. Order and distinctness are preserved,
-- which is all a delete-and-compare needs.
UPDATE stream_access_logs s
   SET accessed_at = timestamp '2026-06-01 00:00:00' - (r.rn || ' seconds')::interval
  FROM (SELECT id, row_number() OVER (ORDER BY accessed_at DESC, id) AS rn
          FROM stream_access_logs) r
 WHERE s.id = r.id;
UPDATE mcp_activity_logs m
   SET timestamp = timestamp '2026-06-01 00:00:00' - (r.rn || ' seconds')::interval
  FROM (SELECT id, row_number() OVER (ORDER BY timestamp DESC, id) AS rn
          FROM mcp_activity_logs) r
 WHERE m.id = r.id;
-- Fifth time, and for the same reason one table at a time keeps finding
-- it: a relative timestamp only drifts into view once something puts
-- the column in front of the comparison. The GDPR export puts *every*
-- org-scoped table in front of it at once, so this is the rest of them.
UPDATE motion_events m
   SET timestamp = timestamp '2026-06-01 00:00:00' - (r.rn || ' seconds')::interval
  FROM (SELECT id, row_number() OVER (ORDER BY timestamp DESC, id) AS rn
          FROM motion_events) r
 WHERE m.id = r.id;
"""


def reseed():
    """Reset every table to the fixture, or raise.

    This used to run with `check=False` and its output captured, so a
    reseed that failed part-way — a lock timeout against a request the
    previous case left open, a fixture edit with a typo — was silent,
    and the next case ran on one side against a half-applied fixture.
    That reads exactly like a port bug on one case and is gone on the
    rerun: a flake. One such flake (1 case in 15 filtered runs, output
    not kept) is what prompted making this loud.
    """
    seed = (HERE / "seed_cameras.sql").read_text() + FREEZE
    docker_psql(["-q"], input=seed, what="reseed")
    # Rate-limit counters too. Many cases hit one route with one
    # credential, and past the limit both tiers 429 identically — which
    # scores as agreement while testing nothing. Every case starts with
    # an empty budget, and a 429 is reported as inconclusive below.
    r = subprocess.run(["docker", "exec", REDIS_CONTAINER, "redis-cli", "FLUSHDB"],
                       capture_output=True, text=True, check=False, timeout=60)
    if r.returncode != 0:
        raise FixtureError(f"could not flush rate limits: {r.stderr.strip()[:200]}")


# Rows a watched table holds that no case can compare, because
# something outside the request writes them on its own clock.
#
# `core/license_client.py`'s check-in is reconciled by a loop in
# `main.py` every 15 minutes. That interval is a literal, not an env
# var, so unlike the other background loops the harness cannot stretch
# it out of the way, and the Python is held unmodified. When it fires it
# rewrites these keys — in whichever of the two passes it lands in, and
# the other pass has only the seeded value, so one unrelated case per
# quarter of an hour reported a side effect and the direction flipped
# between runs.
#
# Excluding them is safe *today* because the Rust licence module only
# reads: `src/license.rs` has no writer at all, so there is nothing on
# the Rust side these rows could be hiding. The licence gate's actual
# behaviour is asserted through responses instead — the "unlicensed"
# cases above. When the background loops are ported and Rust starts
# writing these keys, this exclusion has to go and the two loops have to
# be compared some other way.
SNAPSHOT_FILTER = {
    "settings": " WHERE key NOT LIKE 'sentinel\\_license\\_%'",
}


def snapshot():
    """Table contents as JSON, ordered so the comparison is stable.

    One query for every watched table, not one `docker exec` per table.
    A case takes two snapshots per tier, and at eleven tables that was
    forty-four process launches a case — about eight minutes per
    mutation in a mutation run, which is slow enough that runs get
    stopped half-way, and a stopped run is how a mutation once got left
    in the source.
    """
    parts = [
        f"'{table}', COALESCE((SELECT json_agg(t) FROM "
        f"(SELECT * FROM {table}{SNAPSHOT_FILTER.get(table, '')} ORDER BY 1) t), '[]'::json)"
        for table in WATCHED
    ]
    raw = psql("SELECT json_build_object(" + ", ".join(parts) + ")")
    data = json.loads(raw)
    return {table: data[table] for table in WATCHED}


TS_FORMATS = ("%Y-%m-%dT%H:%M:%S.%f", "%Y-%m-%dT%H:%M:%S",
              # Offset-bearing, as `core/license_client.py` writes into
              # `settings.value`. Without these the licence heartbeat is
              # compared literally, and Python's 15-minute reconcile loop
              # — a hardcoded interval, so unlike the other loops the
              # harness cannot stretch it — rewrites those rows whenever
              # it happens to fire, in whichever pass it lands in. That
              # reported a side-effect difference on an unrelated case,
              # and flipped direction between runs depending on which
              # pass the timer caught. A stack that did *not* write still
              # holds the seeded value, which is old, so a real
              # difference is still a difference.
              "%Y-%m-%dT%H:%M:%S.%f%z", "%Y-%m-%dT%H:%M:%S%z")


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
    if isinstance(value, str) and 19 <= len(value) <= 32 and value[4] == "-":
        for fmt in TS_FORMATS:
            try:
                ts = datetime.strptime(value, fmt)
            except ValueError:
                continue
            # `now` is naive UTC; an offset-bearing value is compared in
            # the same terms.
            if ts.tzinfo is not None:
                ts = ts.astimezone(timezone.utc).replace(tzinfo=None)
            if abs(now - ts) < RECENT_WINDOW:
                return "<recent>"
            return value
    return value


def value_diff(python, rust, path="body", out=None, limit=12):
    """The paths at which two response bodies differ.

    Walks dicts and lists in step and reports leaves, so a difference
    deep inside an archive member reads as
    `body.<zip>.files.cameras.json[3].last_seen` rather than as two
    dumps that agree for their first 260 characters.
    """
    out = [] if out is None else out
    if len(out) >= limit:
        return out
    if isinstance(python, dict) and isinstance(rust, dict):
        for key in sorted(set(python) | set(rust)):
            if key not in python:
                out.append(f"{path}.{key}: only in rust = {short(rust[key])}")
            elif key not in rust:
                out.append(f"{path}.{key}: only in python = {short(python[key])}")
            elif python[key] != rust[key]:
                value_diff(python[key], rust[key], f"{path}.{key}", out, limit)
            if len(out) >= limit:
                break
        return out
    if isinstance(python, list) and isinstance(rust, list):
        if len(python) != len(rust):
            out.append(f"{path}: {len(python)} items in python, {len(rust)} in rust")
        for i, (a, b) in enumerate(zip(python, rust)):
            if a != b:
                value_diff(a, b, f"{path}[{i}]", out, limit)
            if len(out) >= limit:
                break
        return out
    out.append(f"{path}: python={short(python)} rust={short(rust)}")
    return out


def short(value, width=90):
    text = json.dumps(value, sort_keys=True) if not isinstance(value, str) else repr(value)
    return text if len(text) <= width else text[:width] + "..."


def row_diff(python_rows, rust_rows):
    """Name the rows and fields that differ, instead of a truncated dump.

    Rows are matched by their first column (the ORDER BY key). A flake
    once showed only that a sentinel_runs row differed somewhere past the
    240th character, which is no help at all.
    """
    def key(row):
        return next(iter(row.values())) if row else None
    py = {key(r): r for r in python_rows}
    rs = {key(r): r for r in rust_rows}
    out = []
    for k in sorted(set(py) | set(rs), key=str):
        if k not in rs:
            out.append(f"row {k!r} only in python")
        elif k not in py:
            out.append(f"row {k!r} only in rust")
        elif py[k] != rs[k]:
            for field in sorted(set(py[k]) | set(rs[k])):
                if py[k].get(field) != rs[k].get(field):
                    out.append(f"row {k!r} field {field}: python={py[k].get(field)!r} "
                               f"rust={rs[k].get(field)!r}"[:400])
    return out or ["(rows equal but order differs)"]


RESEND_SECRET = os.environ.get(
    "RESEND_WEBHOOK_SECRET", "whsec_aGFybmVzcy13ZWJob29rLXNlY3JldC0xMjM0NTY=")


def sign_svix(req, variant, msg_id, data):
    from datetime import timedelta  # noqa: PLC0415

    from svix.webhooks import Webhook  # noqa: PLC0415

    when = datetime.now(timezone.utc)
    if variant == "svix-stale":
        when -= timedelta(minutes=10)
    elif variant == "svix-future":
        when += timedelta(minutes=10)
    signature = Webhook(RESEND_SECRET).sign(msg_id, when, data.decode("utf-8", "replace"))
    if variant == "svix-bad":
        signature = "v1," + "A" * 43 + "="
    elif variant == "svix-rotated":
        # Two signatures, the first from a retired secret: any one
        # matching must be enough.
        signature = "v1," + "B" * 43 + "= " + signature
    prefix = "webhook" if variant == "svix-webhook" else "svix"
    req.add_header(f"{prefix}-id", msg_id)
    req.add_header(f"{prefix}-timestamp", str(int(when.timestamp())))
    if variant != "svix-nosig":
        req.add_header(f"{prefix}-signature", signature)


def fetch(base, method, path, body, who="admin"):
    # `bytes` is sent exactly as given — malformed JSON, an empty body,
    # a bare `null` — and anything else is JSON-encoded.
    if isinstance(body, bytes):
        data = body
    else:
        data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, method=method, data=data)
    if who in AGENT_KEYS:
        # The agent data plane authenticates on its own header and
        # ignores Authorization entirely, so no bearer token is sent —
        # a route that fell back to session auth would otherwise pass.
        req.add_header("X-Sentinel-Agent-Key", AGENT_KEYS[who])
    elif who == "agent:highbyte":
        # A header byte above 0x7F. Python decodes headers as latin-1
        # and used to feed the str straight to hmac.compare_digest,
        # which raises TypeError on non-ASCII — an unauthenticated
        # probe 500'd all three agent endpoints instead of 401ing.
        req.add_header("X-Sentinel-Agent-Key", "osa_\u00ff")
    elif who == "agent:none":
        pass
    elif who.startswith("svix"):
        # A Resend delivery, signed by the svix library itself at send
        # time — the timestamp is part of what is signed and must be
        # within five minutes. `svix:<id>` is a good delivery; the
        # variants each break one thing.
        variant, _, msg_id = who.partition(":")
        sign_svix(req, variant, msg_id, data or b"")
    elif who.startswith("bearer:"):
        req.add_header("Authorization", f"Bearer {who[len('bearer:'):]}")
    elif who.startswith("node:"):
        # A CameraNode API key. urllib encodes header values as latin-1,
        # so a key containing U+00FF goes out as the single byte 0xFF —
        # which is what makes the UTF-8-versus-raw hashing cases real.
        req.add_header("X-Node-API-Key", who[len("node:"):])
    else:
        token = MEMBER_TOKEN if who == "member" else TOKEN
        req.add_header("Authorization", f"Bearer {token}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, r.read(), {k.lower(): v for k, v in r.headers.items()}
    except urllib.error.HTTPError as e:
        return e.code, e.read(), {k.lower(): v for k, v in e.headers.items()}
    except Exception as e:  # noqa: BLE001
        return None, str(e).encode(), {}


def unpack_archive(raw, headers):
    """A ZIP response as something two stacks can be compared on.

    Not the bytes: Python's zlib and Rust's DEFLATE need not emit the
    same stream for the same input, and every member carries the local
    clock. What the export *means* is the member list, in order, and
    each member's JSON — so that is what is compared, together with the
    three headers that make it a download rather than a page.

    A member that does not parse as JSON is kept as text, so a malformed
    one shows up as itself rather than as a parse error.
    """
    out = {
        "content-disposition": headers.get("content-disposition"),
        "content-type": headers.get("content-type"),
        "cache-control": headers.get("cache-control"),
    }
    with zipfile.ZipFile(io.BytesIO(raw)) as zf:
        out["members"] = zf.namelist()
        files = {}
        for name in zf.namelist():
            payload = zf.read(name).decode("utf-8", "replace")
            try:
                files[name] = table_for_compare(name, json.loads(payload))
            except Exception:  # noqa: BLE001
                files[name] = payload
        out["files"] = files
    return {"<zip>": out}


def table_for_compare(name, rows):
    """One exported table, in the terms the two stacks can agree on.

    `export_org_data` issues `db.query(Model).filter_by(org_id=...)` with
    no `order_by`, so the row order inside a table is whatever the seq
    scan hands back — and the fixture's own FREEZE rewrites rows, which
    moves them. The order flips between runs *on the same stack*, so
    comparing it would be testing the storage engine rather than the
    port. The rows are compared as a set instead.

    What is not arbitrary is the grouping of the two cascade children:
    Python loads them per parent (`for inc in incidents: for ev in
    inc.evidence`), so each parent's rows are contiguous and the parents
    come in the order the parent table was exported. A port that
    replaced that with one flat join would lose it, so it is compared
    explicitly.
    """
    if not isinstance(rows, list) or not all(isinstance(r, dict) for r in rows):
        return rows
    out = {"rows": sorted(rows, key=lambda r: json.dumps(r, sort_keys=True))}
    parent = {"incident_evidence.json": "incident_id", "cameras.json": "node_id"}.get(name)
    if parent:
        # The sequence of parents, with runs collapsed: ["a", "b"] means
        # every row of a came before every row of b.
        seen = []
        for row in rows:
            if not seen or seen[-1] != row.get(parent):
                seen.append(row.get(parent))
        out["grouped_by"] = seen
    return out


def run_case(base, method, path, body, who="admin", setup=None):
    reseed()
    if setup:
        # Per-case SQL, run after the reseed and before the request.
        # Some states cannot be reached any other way: an unlicensed
        # install, or an org that has already spent its monthly run cap.
        docker_psql(["-q"], input=setup, what="case setup")
    now = datetime.now(timezone.utc).replace(tzinfo=None)
    status, raw, headers = fetch(base, method, path, body, who)
    if raw[:4] == b"PK\x03\x04":
        parsed = unpack_archive(raw, headers)
    else:
        try:
            parsed = json.loads(raw)
        except Exception:  # noqa: BLE001
            parsed = raw.decode("utf-8", "replace")
    subs = {**issued_secrets(parsed), **issued_ids(parsed)}
    return (status, substitute(normalise(parsed, now), subs),
            substitute(normalise(snapshot(), now), subs))


def issued_secrets(parsed):
    """Tokens for the random values a response hands out.

    Creating a node and rotating a key return a fresh uuid4 `api_key`,
    and creating a node also a random eight-hex `node_id`. They differ
    between the two tiers by construction, so they are replaced — but by
    *exact value*, not by pattern: the stored `api_key_hash` is replaced
    only if it really is sha256 of the key this response returned. A port
    that stored any other hash leaves a random literal behind, and the
    two tiers disagree.
    """
    subs = {}
    if not isinstance(parsed, dict) or not isinstance(parsed.get("api_key"), str):
        return subs
    key = parsed["api_key"]
    subs[key] = "<issued-api-key>"
    subs[hashlib.sha256(key.encode()).hexdigest()] = "<sha256-of-issued-key>"
    node_id = parsed.get("node_id")
    if isinstance(node_id, str) and re.fullmatch(r"[0-9a-f]{8}", node_id):
        subs[node_id] = "<issued-node-id>"
    return subs


def issued_ids(parsed):
    """A freshly minted run id — uuid4().hex — differs by construction.

    Seeded run ids spell "run0000...", which is not hex, so only a
    generated one matches.
    """
    if isinstance(parsed, dict) and re.fullmatch(r"[0-9a-f]{32}", str(parsed.get("id", ""))):
        return {parsed["id"]: "<issued-run-id>"}
    return {}


def substitute(value, subs):
    if not subs:
        return value
    if isinstance(value, dict):
        return {k: substitute(v, subs) for k, v in value.items()}
    if isinstance(value, list):
        return [substitute(v, subs) for v in value]
    if isinstance(value, str):
        # Longest first, so a node id inside "Node-<id>" or an audit
        # details string is caught without disturbing the rest.
        for secret in sorted(subs, key=len, reverse=True):
            value = value.replace(secret, subs[secret])
        return value
    return value


def main():
    try:
        return _main()
    except FixtureError as err:
        # Exit 2, the "do not trust this run" code the other guards use,
        # rather than a traceback that could be mistaken for a crash in
        # the harness logic itself.
        print(f"\nFIXTURE ERROR — this run proves nothing:\n  {err}")
        return 2


def _main():
    bad = 0
    wanted = [w for w in DIFF_ONLY.split("|") if w]
    cases = [c for c in CASES if not wanted or any(w in c[0] or w in c[2] for w in wanted)]
    for case in cases:
        name, method, path, body = case[0], case[1], case[2], case[3]
        who = case[4] if len(case) > 4 else "admin"
        setup = case[5] if len(case) > 5 else None
        py_status, py_body, py_db = run_case(PYTHON, method, path, body, who, setup)
        rs_status, rs_body, rs_db = run_case(RUST, method, path, body, who, setup)

        # Only the *limiter's* 429 is inconclusive. A route can answer
        # 429 deliberately — the Sentinel monthly cap does — and that is
        # a result to compare, not an exhausted budget.
        rate_limited_429 = 429 in (py_status, rs_status) and any(
            "rate_limit_exceeded" in json.dumps(b) for b in (py_body, rs_body)
        )
        if rate_limited_429:
            bad += 1
            print(f"  INCONCLUSIVE {name}: a 429 (rust={rs_status} python={py_status}) — "
                  f"the budget was exhausted, so nothing was compared")
            continue

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
            # Name the paths that differ, not two truncated dumps. A ZIP
            # export runs to hundreds of kilobytes and its one differing
            # field sat far past any sane truncation — the dumps printed
            # identical prefixes and said nothing.
            for line in value_diff(py_body, rs_body):
                print(f"            {line}")
        if not db_same:
            for table in WATCHED:
                if py_db[table] != rs_db[table]:
                    print(f"            SIDE EFFECT differs in `{table}`:")
                    for line in row_diff(py_db[table], rs_db[table]):
                        print(f"              {line}")

    # A run where nothing was actually written proves nothing.
    reseed()
    seeded = int(psql("SELECT COUNT(*) FROM incidents").strip() or 0)
    ev = int(psql("SELECT COUNT(*) FROM incident_evidence").strip() or 0)
    print(f"\nfixture: {seeded} incidents, {ev} evidence rows")
    if seeded < 4 or ev < 1:
        print("FIXTURE TOO THIN — seed incidents and evidence before trusting this")
        return 2

    # An empty WATCHED table compares equal to an empty WATCHED table,
    # so a side-effect check on a table nothing seeds proves nothing —
    # and a table name that no longer exists fails on both sides
    # identically, which reads exactly like agreement. Both are the same
    # failure the camera fixture had when it aged out and still scored
    # 31/31.
    empty = [t for t in WATCHED
             if not int(psql(f"SELECT COUNT(*) FROM {t}").strip() or 0)]
    # settings and user_notification_state start empty by design: they
    # are written BY the cases, and their emptiness at rest is the
    # baseline a stray write would break.
    empty = [t for t in empty if t not in ("settings", "user_notification_state")]
    if empty:
        print(f"WATCHED TABLES WITH NO ROWS: {', '.join(empty)} — a side-effect "
              f"comparison over an empty table is vacuous. Seed them or drop them.")
        return 2

    scope = f" [DIFF_ONLY={DIFF_ONLY!r}: {len(cases)} of {len(CASES)} cases]" if DIFF_ONLY else ""
    print(f"{len(cases) - bad}/{len(cases)} identical (response + side effects), {bad} differing{scope}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
