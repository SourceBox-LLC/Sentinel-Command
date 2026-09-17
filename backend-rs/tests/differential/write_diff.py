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
import os
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
           "camera_groups", "cameras", "mcp_api_keys",
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


def psql(sql):
    # ON_ERROR_STOP plus a checked exit, not `check=False`. A failed
    # query used to return empty stdout, which `snapshot()` read as an
    # empty table — so a query error on one side and a real empty table
    # on the other compared as a side-effect diff, and a query error on
    # both compared as agreement.
    r = subprocess.run(
        ["docker", "exec", "-i", PG_CONTAINER, "psql", "-U", "cc", "-d", "cc",
         "-v", "ON_ERROR_STOP=1", "-tAq", "-c", sql],
        capture_output=True, text=True, check=False,
    )
    if r.returncode != 0:
        raise FixtureError(f"psql failed ({r.returncode}): {r.stderr.strip()[:400]}\n  sql: {sql[:200]}")
    return r.stdout


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
    r = subprocess.run(
        ["docker", "exec", "-i", PG_CONTAINER, "psql", "-U", "cc", "-d", "cc",
         "-v", "ON_ERROR_STOP=1", "-q"],
        input=seed, capture_output=True, text=True, check=False,
    )
    if r.returncode != 0 or "ERROR" in r.stderr:
        raise FixtureError(f"reseed failed ({r.returncode}): {r.stderr.strip()[:600]}")


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


def fetch(base, method, path, body, who="admin"):
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
    else:
        token = MEMBER_TOKEN if who == "member" else TOKEN
        req.add_header("Authorization", f"Bearer {token}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception as e:  # noqa: BLE001
        return None, str(e).encode()


def run_case(base, method, path, body, who="admin"):
    reseed()
    now = datetime.now(timezone.utc).replace(tzinfo=None)
    status, raw = fetch(base, method, path, body, who)
    try:
        parsed = json.loads(raw)
    except Exception:  # noqa: BLE001
        parsed = raw.decode("utf-8", "replace")
    return status, normalise(parsed, now), normalise(snapshot(), now)


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
    cases = [c for c in CASES if not DIFF_ONLY or DIFF_ONLY in c[0] or DIFF_ONLY in c[2]]
    for case in cases:
        name, method, path, body = case[0], case[1], case[2], case[3]
        who = case[4] if len(case) > 4 else "admin"
        py_status, py_body, py_db = run_case(PYTHON, method, path, body, who)
        rs_status, rs_body, rs_db = run_case(RUST, method, path, body, who)

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
