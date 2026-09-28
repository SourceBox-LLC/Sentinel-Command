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

import base64
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
import zipfile

from diffutil import value_diff
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

# Past-due anchors for the grace-countdown cases, fixed at import so
# both passes of a case seed the identical value. Seven days is
# PAYMENT_GRACE_DAYS.
_PAST_DUE_NOW = datetime.now(timezone.utc)
PAST_DUE_DAYS_LEFT = (_PAST_DUE_NOW - timedelta(days=2)).isoformat()
PAST_DUE_HOURS_LEFT = (_PAST_DUE_NOW - timedelta(days=6, hours=20)).isoformat()
PAST_DUE_EXPIRED = (_PAST_DUE_NOW - timedelta(days=9)).isoformat()


# Debounce anchors, fixed at import for the same reason as the past-due
# ones above: computing `now() - interval '1 hour'` inside the setup SQL
# runs it once per pass, and the two passes are a second apart — so the
# seeded value itself differed and the case reported a port bug. Both
# are recent enough to still be inside their window when the request
# lands, minutes later.
DISK_ANCHOR_RECENT = (
    _PAST_DUE_NOW - timedelta(hours=1)).replace(tzinfo=None).isoformat(timespec="seconds")
PLAN_LIMIT_ANCHOR_RECENT = (
    _PAST_DUE_NOW - timedelta(minutes=5)).replace(tzinfo=None).isoformat(timespec="seconds")


# A four-hour window centred on now, in UTC, fixed at import so both
# passes seed the identical literal.
#
# Every other recording case is deliberately time-independent, because a
# window computed per pass would straddle a minute boundary. That left
# the timezone itself unverifiable: with no window in play, UTC and
# America/Los_Angeles give the same answer for every camera, and a port
# that ignored the org's zone entirely would pass. This window is inside
# the current UTC hour and nowhere near the local hour seven zones away,
# so the two disagree — deterministically, since the literal is fixed
# and the passes are a second apart, not two hours.
WINDOW_START = (_PAST_DUE_NOW - timedelta(hours=2)).strftime("%H:%M")
WINDOW_END = (_PAST_DUE_NOW + timedelta(hours=2)).strftime("%H:%M")


def window_setup(tz: str) -> str:
    """Put `cam-stale` inside a window that is live in UTC, under the
    named org timezone.

    The zone is always named explicitly. The fixture already gives this
    org `America/Los_Angeles`, so "leave it alone" would silently mean
    "test Los Angeles" — and the case saying it tested UTC would have
    been testing the same thing as the case beside it.

    It is set with `pref`, which clears the key first, rather than a
    bare INSERT: `(org_id, key)` is not unique, and a second row made
    both tiers pick one at random and disagree about half the time, in
    both directions. See PYTHON_BUGS.md #12.
    """
    return (
        "UPDATE cameras SET scheduled_recording = true,"
        f" scheduled_start = '{WINDOW_START}', scheduled_end = '{WINDOW_END}'"
        " WHERE camera_id = 'cam-stale';"
    ) + pref("timezone", tz)


def past_due_setup(stamp: str) -> str:
    """Mark the org past due, with `payment_past_due_at` set to `stamp`."""
    return (
        "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
        " (9201,'self-host','payment_past_due','true',timestamp '2026-06-01'),"
        f" (9202,'self-host','payment_past_due_at','{stamp}',timestamp '2026-06-01');"
    )


# The raw agent keys behind seed rows 1-3. Hashes are in
# seed_cameras.sql; these are the values a caller presents.
AGENT_KEYS = {
    "agent": "osa_00000000000000000000000000000001",
    "agent:revoked": "osa_00000000000000000000000000000002",
    "agent:theirs": "osa_00000000000000000000000000000003",
    "agent:unknown": "osa_ffffffffffffffffffffffffffffffff",
}

# Unsubscribe tokens, minted the way `core/email_unsubscribe.py` does:
# the signing key is derived from the deployment secret with a fixed
# domain label, never used raw. Built here rather than fetched from a
# rendered email so a case can ask for a token that is expired, or
# signed with the wrong key, which no email would ever contain.
UNSUB_LABEL = b"sentinel-email-unsubscribe-v1"


def unsub_token(kind="camera_offline", rcpt="admin@example.com",
                org_id="self-host", age=0, secret=None):
    import hmac as _hmac  # noqa: PLC0415

    import jwt as _jwt  # noqa: PLC0415

    base = secret or os.environ.get(
        "APP_SECRET_KEY", "differential-test-secret-not-a-real-key")
    derived = _hmac.new(base.encode(), UNSUB_LABEL, hashlib.sha256).hexdigest()
    issued = int(time.time()) - age
    return _jwt.encode(
        {"org_id": org_id, "kind": kind, "rcpt": rcpt, "iat": issued,
         "exp": issued + 400 * 24 * 3600, "sub": "email-unsubscribe"},
        derived, algorithm="HS256",
    )


# A per-org notification preference, written the way the settings
# table stores it — the string "true" or "false", never a boolean.
def pref(key, value):
    return (
        f"DELETE FROM settings WHERE org_id='self-host' AND key='{key}';"
        f"INSERT INTO settings (org_id, key, value, updated_at)"
        f" VALUES ('self-host', '{key}', '{value}', now()::timestamp);"
    )


# Pro Plus caps cameras at 200 against the fixture's 21, so a case
# using this reaches the *creation* path in registration. Without it
# every reported camera is refused: the free cap is five.
PRO_PLUS = pref("org_plan", "pro_plus")


# A `sentinel_config` row for `self-host` with every gate open, and
# keyword overrides to close one at a time. Values are SQL literals
# because they go straight into the setup statement.
def sentinel_on(**over):
    fields = {
        "enabled": "true",
        "motion_enabled": "true",
        "incident_opened_enabled": "true",
        "motion_cooldown_min": "0",
        "schedule_mode": "'always'",
        "schedule_start": "'00:00'",
        "schedule_end": "'24:00'",
        "active_days": """'["mon","tue","wed","thu","fri","sat","sun"]'""",
        "camera_scope": "'{}'",
    }
    fields.update(over)
    columns = ", ".join(fields)
    values = ", ".join(fields.values())
    return (
        "DELETE FROM sentinel_config WHERE org_id='self-host';"
        f"INSERT INTO sentinel_config (org_id, {columns}, created_at, updated_at)"
        f" VALUES ('self-host', {values}, now()::timestamp, now()::timestamp);"
    )


# Five hundred runs already spent this month — the per-plan cap for
# `self_host`.
CAP_FILLER = (
    "INSERT INTO sentinel_runs (id, org_id, triggered_at, trigger_type,"
    " tool_call_count, outcome, summary, updated_at)"
    " SELECT md5(g::text), 'self-host', now()::timestamp, 'motion', 0, 'no_action',"
    " '', now()::timestamp FROM generate_series(1, 500) g;"
)


# (name, method, path, body) — body None means no request body.
# A 5th element "member" sends the non-admin token instead, and "none"
# sends no Authorization header at all.
CASES = [
    # --- POST: a member asking for admin ------------------------------
    # Audience "admin", so this is also the one write case whose
    # notification a viewer must never see on the SSE stream.
    ("promotion: a member asks", "POST",
     "/api/notifications/request-admin-promotion", None, "member"),
    # An admin asking would otherwise notify themselves that they had
    # requested their own access.
    ("promotion: an admin asks", "POST",
     "/api/notifications/request-admin-promotion", None),

    # --- GET: the unsubscribe link ------------------------------------
    # A GET that writes: an `email_suppression` row and an audit entry.
    # Public, so no token is sent — the signed one in the URL is the
    # authority, and these are the only cases in the file that carry
    # their own credential.
    ("unsub: valid link", "GET",
     "/api/notifications/email/unsubscribe?t=" + unsub_token(), None, "none"),
    # Idempotent: the address is already suppressed by the seed, so a
    # second click adds no row and still renders the same page.
    ("unsub: already suppressed", "GET",
     "/api/notifications/email/unsubscribe?t=" + unsub_token(
         rcpt="already@example.com"), None, "none"),
    # The kind reaches the page copy, underscores turned to spaces.
    ("unsub: another kind", "GET",
     "/api/notifications/email/unsubscribe?t=" + unsub_token(kind="motion_digest"),
     None, "none"),
    # An address with no domain takes the other masking branch.
    ("unsub: malformed address", "GET",
     "/api/notifications/email/unsubscribe?t=" + unsub_token(rcpt="not-an-address"),
     None, "none"),
    ("unsub: bad signature", "GET",
     "/api/notifications/email/unsubscribe?t=" + unsub_token(secret="wrong-secret"),
     None, "none"),
    ("unsub: expired", "GET",
     "/api/notifications/email/unsubscribe?t=" + unsub_token(age=401 * 24 * 3600),
     None, "none"),
    ("unsub: not a token", "GET",
     "/api/notifications/email/unsubscribe?t=nonsense", None, "none"),
    ("unsub: empty token", "GET",
     "/api/notifications/email/unsubscribe?t=", None, "none"),
    ("unsub: no token at all", "GET",
     "/api/notifications/email/unsubscribe", None, "none"),

    # --- POST: filing one, and what the dispatcher does with it -------
    # The fixture gives `self-host` no sentinel_config row, so without a
    # setup every incident case stops at the dispatcher's first gate and
    # the rest of it — plan, licence, trigger, scope, schedule, cap — is
    # never reached by anything. These turn it on and then close one
    # gate at a time.
    *[(f"dispatch: {label}", "POST", "/api/incidents",
       {"title": "T", "summary": "S", "camera_id": "cam-live"}, "admin", setup)
      for label, setup in [
          ("every gate open", sentinel_on()),
          ("sentinel disabled", sentinel_on(enabled="false")),
          ("incident trigger off", sentinel_on(incident_opened_enabled="false")),
          # The camera is named in the body, and excluded by scope.
          ("camera out of scope", sentinel_on(camera_scope="""'{"cam-live": false}'""")),
          # A camera absent from the scope is still in scope.
          ("another camera out of scope", sentinel_on(camera_scope="""'{"cam-stale": false}'""")),
          ("schedule off", sentinel_on(schedule_mode="'off'")),
          # No day is today, whichever day it is — deterministic where a
          # window in hours would depend on when the harness ran.
          ("no active days", sentinel_on(schedule_mode="'scheduled'", active_days="'[]'")),
          ("every active day", sentinel_on(schedule_mode="'scheduled'")),
          ("unlicensed",
           sentinel_on() + "DELETE FROM settings WHERE org_id='self-host' AND key LIKE 'sentinel_license%';"),
          ("monthly cap reached", sentinel_on() + CAP_FILLER),
      ]],

    # --- POST: filing one ---------------------------------------------
    # The first ported route that emits a notification, so these are
    # also the first cases that compare `notifications` and
    # `email_outbox` rows written by Rust rather than forwarded to
    # Python — and, where the seeded Sentinel config allows it, a
    # `sentinel_runs` row from the dispatcher.
    ("file: minimal", "POST", "/api/incidents", {"title": "T", "summary": "S"}),
    ("file: every field", "POST", "/api/incidents",
     {"title": "Back door", "summary": "Someone at the gate", "severity": "high",
      "camera_id": "cam-live"}),
    # Severity drives the notification's own severity, and the split is
    # at high: low and medium are a warning, high and critical are not.
    ("file: low", "POST", "/api/incidents",
     {"title": "T", "summary": "S", "severity": "low"}),
    ("file: critical", "POST", "/api/incidents",
     {"title": "T", "summary": "S", "severity": "critical"}),
    ("file: bad severity", "POST", "/api/incidents",
     {"title": "T", "summary": "S", "severity": "nonsense"}),
    ("file: null severity", "POST", "/api/incidents",
     {"title": "T", "summary": "S", "severity": None}),
    # Stripped before the emptiness check, so whitespace is not a title.
    ("file: padded", "POST", "/api/incidents", {"title": "  T  ", "summary": "  S  "}),
    ("file: whitespace title", "POST", "/api/incidents", {"title": "   ", "summary": "S"}),
    ("file: whitespace summary", "POST", "/api/incidents", {"title": "T", "summary": " "}),
    # Pydantic's bounds, which fire before the handler sees anything.
    ("file: no body fields", "POST", "/api/incidents", {}),
    ("file: empty title", "POST", "/api/incidents", {"title": "", "summary": "S"}),
    ("file: empty summary", "POST", "/api/incidents", {"title": "T", "summary": ""}),
    ("file: title too long", "POST", "/api/incidents",
     {"title": "x" * 201, "summary": "S"}),
    ("file: title at the bound", "POST", "/api/incidents",
     {"title": "x" * 200, "summary": "S"}),
    ("file: title not a string", "POST", "/api/incidents",
     {"title": 123, "summary": "S"}),
    ("file: null title", "POST", "/api/incidents", {"title": None, "summary": "S"}),
    ("file: every field wrong", "POST", "/api/incidents",
     {"title": 1, "summary": 2, "severity": 3, "camera_id": 4}),
    # The camera must be this org's. cam-theirs belongs to another.
    ("file: unknown camera", "POST", "/api/incidents",
     {"title": "T", "summary": "S", "camera_id": "nope"}),
    ("file: another tenant's camera", "POST", "/api/incidents",
     {"title": "T", "summary": "S", "camera_id": "cam-theirs"}),
    ("file: empty camera id", "POST", "/api/incidents",
     {"title": "T", "summary": "S", "camera_id": ""}),
    ("file: null camera id", "POST", "/api/incidents",
     {"title": "T", "summary": "S", "camera_id": None}),
    # Title and summary reach the email templates, so they have to
    # survive escaping in HTML and stay literal in the text part.
    ("file: markup in the title", "POST", "/api/incidents",
     {"title": "<b>&</b> \"q\" 'a'", "summary": "5 < 6 & 7 > 2"}),
    ("file: unicode", "POST", "/api/incidents",
     {"title": "Café — Ünïcødé 😀", "summary": "naïve façade"}),
    ("file: member cannot", "POST", "/api/incidents",
     {"title": "T", "summary": "S"}, "member"),

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

    # --- the plan panel's grace countdown --------------------------------
    # Read cases, here rather than in the read differential, because each
    # needs its own past-due settings. The countdown is
    # `timedelta.days`, which floors: twenty hours of grace left is zero
    # days, and an hour past is minus one, shown as zero.
    #
    # The timestamps are literals computed once, at import — not `now()`
    # in the setup SQL. Each case is seeded separately for each tier, so
    # a `now()` there gives the two passes different values and the
    # expiry they echo back differs by the reseed delta. Third time this
    # shape has bitten.
    *[(f"plan: {label}", "GET", "/api/nodes/plan", None, "admin",
       "DELETE FROM settings WHERE org_id='self-host'"
       " AND key IN ('payment_past_due','payment_past_due_at','plan_cancel_pending');"
       + setup)
      for label, setup in [
          ("not past due", ""),
          ("past due, days left", past_due_setup(PAST_DUE_DAYS_LEFT)),
          ("past due, hours left", past_due_setup(PAST_DUE_HOURS_LEFT)),
          ("past due, grace expired", past_due_setup(PAST_DUE_EXPIRED)),
          ("past due, a naive timestamp", past_due_setup("2026-09-01T00:00:00")),
          ("past due, an offset timestamp", past_due_setup("2026-09-01T00:00:00-05:00")),
          ("past due, a Z timestamp", past_due_setup("2026-09-01T00:00:00Z")),
          ("past due, an unparseable timestamp", past_due_setup("not a date")),
          ("past due, an empty timestamp", past_due_setup("")),
          ("past due, no timestamp at all",
           "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
           " (9201,'self-host','payment_past_due','true',timestamp '2026-06-01');"),
          ("cancellation pending",
           "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
           " (9203,'self-host','plan_cancel_pending','true',timestamp '2026-06-01');"),
      ]],

    # --- a node decommissioning itself -----------------------------------
    # The node asks, by key, and its cameras go with it. Watched tables
    # show both the cascade and the audit row that records who did it.
    ("decommission self", "POST", "/api/nodes/self/decommission", None, "node:test-node-key", None),
    ("decommission self, no key", "POST", "/api/nodes/self/decommission", None, "agent:none", None),
    ("decommission self, unknown key", "POST", "/api/nodes/self/decommission", None,
     "node:not-a-real-key", None),
    ("decommission self, a node with no cameras", "POST", "/api/nodes/self/decommission", None,
     "node:test-node-key", "DELETE FROM cameras WHERE node_id ="
     " (SELECT id FROM camera_nodes WHERE node_id = 'node-aaaa1111');"),

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

    # --- Deleting a node ----------------------------------------------
    # The cameras go with it, and so do their segment caches. Neither
    # node is connected here, so `wipe_data` fails and `node_wiped` is
    # false — which is the normal case for a node that has already gone
    # away, and the reason a failed wipe cannot block the delete.
    ("delete node with cameras", "DELETE", "/api/nodes/node-aaaa1111", None),
    ("delete node with a codec", "DELETE", "/api/nodes/node-dddd4444", None),
    ("delete another org's node", "DELETE", "/api/nodes/node-cccc3333", None),
    ("delete missing node", "DELETE", "/api/nodes/node-nope", None),
    ("member: delete node", "DELETE", "/api/nodes/node-aaaa1111", None, "member"),

    # --- Article 17 -----------------------------------------------------
    # Every org-scoped table emptied, and the only row left in
    # `audit_log` is the one this write puts there. No plan gate: it is
    # a legal obligation, unlike its paid-only `wipe-logs` sibling.
    ("full reset", "POST", "/api/settings/danger/full-reset", None),
    ("full reset on a free plan", "POST", "/api/settings/danger/full-reset", None,
     "admin", pref("org_plan", "free_org")),
    ("member: full reset", "POST", "/api/settings/danger/full-reset", None, "member"),

    # --- Motion, pushed over HTTP -------------------------------------
    # The reliable half of motion reporting — it works whether or not
    # the node's socket is up. Each accepted event writes a motion row,
    # a notification, and (once the org has Sentinel on) a run.
    ("motion: ok", "POST", "/api/cameras/cam-live/motion",
     {"score": 42}, "node:test-node-key"),
    ("motion: every field", "POST", "/api/cameras/cam-live/motion",
     {"score": 87, "segment_seq": 1234, "timestamp": "2026-09-01T10:00:00"},
     "node:test-node-key"),
    # The score is clamped, not rejected.
    ("motion: score above the range", "POST", "/api/cameras/cam-live/motion",
     {"score": 250}, "node:test-node-key"),
    ("motion: score below the range", "POST", "/api/cameras/cam-live/motion",
     {"score": -5}, "node:test-node-key"),
    # int() truncates toward zero before the clamp.
    ("motion: fractional score", "POST", "/api/cameras/cam-live/motion",
     {"score": 99.9}, "node:test-node-key"),
    ("motion: score as a string", "POST", "/api/cameras/cam-live/motion",
     {"score": "77"}, "node:test-node-key"),
    # Everything int() refuses drops the event rather than erroring.
    ("motion: score is not a number", "POST", "/api/cameras/cam-live/motion",
     {"score": "abc"}, "node:test-node-key"),
    ("motion: score is a list", "POST", "/api/cameras/cam-live/motion",
     {"score": []}, "node:test-node-key"),
    ("motion: no score at all", "POST", "/api/cameras/cam-live/motion",
     {"segment_seq": 1}, "node:test-node-key"),
    ("motion: null score", "POST", "/api/cameras/cam-live/motion",
     {"score": None}, "node:test-node-key"),
    # An offset is *dropped*, not applied: this is stored as 10:00:00.
    ("motion: timestamp with an offset", "POST", "/api/cameras/cam-live/motion",
     {"score": 10, "timestamp": "2026-09-01T10:00:00+05:00"}, "node:test-node-key"),
    ("motion: unparseable timestamp", "POST", "/api/cameras/cam-live/motion",
     {"score": 10, "timestamp": "not a date"}, "node:test-node-key"),
    ("motion: timestamp is not a string", "POST", "/api/cameras/cam-live/motion",
     {"score": 10, "timestamp": 5}, "node:test-node-key"),
    # Past the column: the whole event is lost, row and notification
    # together, because the Python's commit fails.
    ("motion: segment_seq past the column", "POST", "/api/cameras/cam-live/motion",
     {"score": 10, "segment_seq": 2147483648}, "node:test-node-key"),
    ("motion: segment_seq as a string", "POST", "/api/cameras/cam-live/motion",
     {"score": 10, "segment_seq": "9"}, "node:test-node-key"),
    # The camera has to be this node's, in this node's org.
    ("motion: another node's camera", "POST", "/api/cameras/cam-failed/motion",
     {"score": 10}, "node:test-node-key"),
    ("motion: another org's camera", "POST", "/api/cameras/cam-theirs/motion",
     {"score": 10}, "node:test-node-key"),
    ("motion: no such camera", "POST", "/api/cameras/cam-nope/motion",
     {"score": 10}, "node:test-node-key"),
    ("motion: no api key", "POST", "/api/cameras/cam-live/motion",
     {"score": 10}, "agent:none"),
    ("motion: wrong api key", "POST", "/api/cameras/cam-live/motion",
     {"score": 10}, "node:wrong"),
    # The per-org kill switch, answered 200 so the node does not retry.
    ("motion: ingestion disabled", "POST", "/api/cameras/cam-live/motion",
     {"score": 10}, "node:test-node-key",
     pref("motion_ingestion_enabled", "false")),
    # Anything that is not "true", case-insensitively, is off.
    ("motion: ingestion set to TRUE", "POST", "/api/cameras/cam-live/motion",
     {"score": 10}, "node:test-node-key",
     pref("motion_ingestion_enabled", "TRUE")),
    ("motion: ingestion set to nonsense", "POST", "/api/cameras/cam-live/motion",
     {"score": 10}, "node:test-node-key",
     pref("motion_ingestion_enabled", "yes")),
    # With Sentinel on and motion as a trigger, an accepted event also
    # queues a run — and the cooldown silences the email after the
    # first, which is the pair the digest loop exists for.
    ("motion: dispatches a run", "POST", "/api/cameras/cam-live/motion",
     {"score": 60}, "node:test-node-key", sentinel_on()),
    ("motion: out of scope for sentinel", "POST", "/api/cameras/cam-live/motion",
     {"score": 60}, "node:test-node-key",
     sentinel_on(camera_scope="""'{"cam-live": false}'""")),

    # --- The two gates, separately ------------------------------------
    # Both default on for this kind and the fixture writes neither, so
    # without these the "is it exactly the string true" comparison and
    # the independence of the two gates are reached by nothing.
    #
    # `mcp_key_revoked` is the kind to use: it is the only one these
    # cases emit that appears in *both* maps, so its inbox and email
    # toggles can be set against each other.
    ("gates: inbox off, email on", "DELETE", "/api/mcp/keys/1", None, "admin",
     pref("mcp_key_audit_notifications", "false") + pref("email_mcp_key_audit", "true")),
    ("gates: inbox on, email off", "DELETE", "/api/mcp/keys/2", None, "admin",
     pref("mcp_key_audit_notifications", "true") + pref("email_mcp_key_audit", "false")),
    ("gates: both off", "DELETE", "/api/mcp/keys/3", None, "admin",
     pref("mcp_key_audit_notifications", "false") + pref("email_mcp_key_audit", "false")),
    # Neither "true" nor absent: anything generous about the comparison
    # reads this as enabled.
    ("gates: a setting that is not a bool", "DELETE", "/api/mcp/keys/1", None, "admin",
     pref("email_mcp_key_audit", "yes") + pref("mcp_key_audit_notifications", "1")),

    # A title where bytes and characters part company. The notification
    # title is "Incident #N: " plus this, so it runs past the 200-char
    # column limit — and cutting 200 *bytes* of a two-byte-per-character
    # string keeps about half as much text.
    ("file: a title that is 200 characters of two-byte text", "POST", "/api/incidents",
     {"title": "é" * 200, "summary": "S"}),

    # --- POST: minting a key ------------------------------------------
    # All three mint a credential, return it exactly once, and fire an
    # admin notification — a new key is a security signal, and naming
    # the actor is what lets a recipient who *is* the actor recognise
    # their own action rather than suspecting a compromise.
    ("mint agent key", "POST", "/api/sentinel/agent-keys", {"name": "CI agent"}),
    ("mint agent key, default name", "POST", "/api/sentinel/agent-keys", {}),
    ("mint agent key, name at the bound", "POST", "/api/sentinel/agent-keys",
     {"name": "x" * 100}),
    ("mint agent key, name too long", "POST", "/api/sentinel/agent-keys",
     {"name": "x" * 101}),
    ("mint agent key, name not a string", "POST", "/api/sentinel/agent-keys",
     {"name": 5}),
    ("mint agent key, name null", "POST", "/api/sentinel/agent-keys", {"name": None}),
    # Non-ASCII reaches the audit `details` JSON, where json.dumps
    # escapes it, and the notification title, where it stays literal.
    ("mint agent key, non-ascii name", "POST", "/api/sentinel/agent-keys",
     {"name": "Café 🎥"}),
    ("mint agent key, member", "POST", "/api/sentinel/agent-keys", {"name": "x"}, "member"),
    # Billing, not just admin: this key spends money on every run.
    ("mint agent key, past due", "POST", "/api/sentinel/agent-keys", {"name": "x"},
     "admin", past_due_setup(PAST_DUE_EXPIRED)),
    ("mint agent key, unlicensed", "POST", "/api/sentinel/agent-keys", {"name": "x"},
     "admin", "DELETE FROM settings WHERE org_id='self-host' AND key LIKE 'sentinel_license%';"),

    # Integration keys are free on every tier — admin, but no billing
    # gate — and carry no per-tool scoping, so both scope columns stay
    # null where an MCP row has values.
    ("mint integration key", "POST", "/api/integration/keys", {"name": "HA"}),
    ("mint integration key, default name", "POST", "/api/integration/keys", {}),
    ("mint integration key, name too long", "POST", "/api/integration/keys",
     {"name": "x" * 101}),
    ("mint integration key, name not a string", "POST", "/api/integration/keys",
     {"name": []}),
    ("mint integration key, member", "POST", "/api/integration/keys", {"name": "x"}, "member"),
    # Free on every tier, so a past-due org can still mint one.
    ("mint integration key, past due", "POST", "/api/integration/keys", {"name": "x"},
     "admin", past_due_setup(PAST_DUE_EXPIRED)),

    # Agent keys: a soft revoke, so `last_used_at` survives as the
    # forensic answer to "when did this leaked credential last act?"
    # and the unique hash stays burned. Seeded rows 1-3 are a live key,
    # a revoked one and another tenant's.
    ("revoke agent key", "DELETE", "/api/sentinel/agent-keys/1", None),
    ("revoke an already-revoked agent key", "DELETE", "/api/sentinel/agent-keys/2", None),
    # 404 rather than 403, so a caller cannot probe which ids exist
    # in another org.
    ("revoke another tenant's agent key", "DELETE", "/api/sentinel/agent-keys/3", None),
    ("revoke missing agent key", "DELETE", "/api/sentinel/agent-keys/9999", None),
    ("revoke agent key, non-integer id", "DELETE", "/api/sentinel/agent-keys/abc", None),
    ("member: revoke agent key", "DELETE", "/api/sentinel/agent-keys/1", None, "member"),

    # MCP keys: the same revoke, a different response shape, and a
    # notification the integration surface deliberately has none of —
    # an MCP key is full programmatic access, so both ends of its
    # lifecycle are a security audit signal.
    ("revoke mcp key", "DELETE", "/api/mcp/keys/1", None),
    ("revoke mcp key with a custom scope", "DELETE", "/api/mcp/keys/2", None),
    # The name reaches the audit `details` JSON *and* the notification
    # title and body, so a non-ASCII one exercises json.dumps'
    # ensure_ascii in one place and leaves it literal in the others.
    ("revoke mcp key with a non-ascii name", "DELETE", "/api/mcp/keys/11", None),
    # Already revoked: no second UPDATE, but Python still audits and
    # still notifies.
    ("revoke an already-revoked mcp key", "DELETE", "/api/mcp/keys/4", None),
    # An integration key id must 404 here rather than crossing surfaces.
    ("revoke an integration key via mcp", "DELETE", "/api/mcp/keys/5", None),
    ("revoke missing mcp key", "DELETE", "/api/mcp/keys/9999", None),
    ("revoke another tenant's mcp key", "DELETE", "/api/mcp/keys/9", None),
    ("revoke mcp, non-integer id", "DELETE", "/api/mcp/keys/abc", None),
    ("member: revoke mcp key", "DELETE", "/api/mcp/keys/1", None, "member"),

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

    # --- minting an MCP key --------------------------------------------
    #
    # The response hands back the only copy of the secret, and the row
    # beside it carries the scope the key will be held to. Both are
    # compared, along with the audit row and the admin notification —
    # a key being created is a security signal, so losing the
    # notification would be a regression a response diff cannot see.
    *[(f"mcp key: {label}", "POST", "/api/mcp/keys", body)
      for label, body in [
          ("default everything", {}),
          ("named", {"name": "CI"}),
          ("readonly", {"name": "Reader", "scope_mode": "readonly"}),
          ("custom", {"name": "Scoped", "scope_mode": "custom",
                      "scope_tools": ["list_cameras", "get_camera"]}),
          # Deduplicated and stripped, WITHOUT reordering.
          ("custom with duplicates and blanks",
           {"name": "Scoped", "scope_mode": "custom",
            "scope_tools": ["get_camera", "list_cameras", "get_camera", "", "  ",
                            " list_cameras "]}),
          # `custom` with nothing usable is a 400, not an unscoped key.
          ("custom with no tools", {"name": "x", "scope_mode": "custom"}),
          ("custom with an empty list",
           {"name": "x", "scope_mode": "custom", "scope_tools": []}),
          ("custom with only blanks",
           {"name": "x", "scope_mode": "custom", "scope_tools": ["", "   "]}),
          # An unknown name is refused rather than silently dropped —
          # the opposite of what compute_allowed_tools does at call
          # time, and deliberately so: a typo here is a customer
          # mistake worth reporting.
          ("custom with an unknown tool",
           {"name": "x", "scope_mode": "custom",
            "scope_tools": ["list_cameras", "rm_minus_rf"]}),
          ("custom with several unknown tools",
           {"name": "x", "scope_mode": "custom", "scope_tools": ["nope", "also_nope"]}),
          # A write tool by name is perfectly legal for a user key; it
          # is only the AGENT that is held to an allowlist.
          ("custom naming a write tool",
           {"name": "x", "scope_mode": "custom",
            "scope_tools": ["set_camera_recording_policy"]}),
          # scope_tools on a non-custom mode is ignored, not an error.
          ("readonly with tools anyway",
           {"name": "x", "scope_mode": "readonly", "scope_tools": ["list_cameras"]}),
          ("all with tools anyway",
           {"name": "x", "scope_mode": "all", "scope_tools": ["list_cameras"]}),
          # Validation.
          ("an unknown scope mode", {"name": "x", "scope_mode": "nonsense"}),
          ("scope mode of the wrong type", {"name": "x", "scope_mode": 5}),
          ("scope mode null", {"name": "x", "scope_mode": None}),
          ("name too long", {"name": "x" * 101}),
          ("name of the wrong type", {"name": 5}),
          ("scope_tools not a list",
           {"name": "x", "scope_mode": "custom", "scope_tools": "list_cameras"}),
          ("scope_tools holding a number",
           {"name": "x", "scope_mode": "custom", "scope_tools": ["list_cameras", 5]}),
          ("empty body", b""),
          ("malformed json", b"{x"),
          ("list body", [1, 2]),
      ]],
    ("mcp key: as a member", "POST", "/api/mcp/keys", {"name": "x"}, "member"),

    ("policy: bad HH:MM", "PATCH", "/api/cameras/cam-live/recording-settings",
     {"scheduled_start": "25:00"}),
    ("policy: single-digit hour", "PATCH", "/api/cameras/cam-live/recording-settings",
     {"scheduled_start": "8:30"}),

    # --- CameraNode registration --------------------------------------
    #
    # The org's fixture already holds 21 cameras against a free cap of
    # five, so the default path here is the *skipped* one: every camera
    # a node reports that does not already exist is refused, and the
    # response carries `plan_limit_hit`. The creation path needs the cap
    # lifted, which `PRO_PLUS` does.
    *[(f"register: {label}", "POST", "/api/nodes/register", body, who, setup)
      for label, body, who, setup in [
          ("ok, no cameras", {"node_id": "node-aaaa1111"}, "node:test-node-key", None),
          ("hostname and ip", {"node_id": "node-aaaa1111", "hostname": "pi-front",
                               "local_ip": "192.168.1.9", "http_port": 8085},
           "node:test-node-key", None),
          # lan_streaming=False clears local_ip so the integration layer
          # stops advertising a URL that refuses connections.
          ("loopback-bound clears the ip",
           {"node_id": "node-dddd4444", "local_ip": "192.168.1.40", "lan_streaming": False},
           "node:node-key-\u00ff", None),
          ("an absent lan_streaming keeps it",
           {"node_id": "node-dddd4444", "local_ip": "192.168.1.41"},
           "node:node-key-\u00ff", None),
          # Every reported camera already exists, so this is the update
          # path: names refreshed, status online, nothing created.
          ("existing cameras are updated",
           {"node_id": "node-aaaa1111",
            "cameras": [{"device_path": "live", "name": "Renamed Live"},
                        {"device_path": "stale"}, {"device_path": "boundary"},
                        {"device_path": "restart"}, {"device_path": "error"},
                        {"device_path": "neverseen"}]},
           "node:test-node-key", None),
          # A node that reports fewer cameras than it has: the rest are
          # deleted, with their segment caches.
          ("unreported cameras are removed",
           {"node_id": "node-aaaa1111", "cameras": [{"device_path": "live"}]},
           "node:test-node-key", None),
          # Over cap: skipped, notified, and told so in the response.
          ("a new camera over the cap is skipped",
           {"node_id": "node-aaaa1111",
            "cameras": [{"device_path": "/dev/video9", "name": "New One"}]},
           "node:test-node-key", None),
          ("six over the cap lists five and a count",
           {"node_id": "node-aaaa1111",
            "cameras": [{"device_path": f"/dev/video{i}"} for i in range(1, 8)]},
           "node:test-node-key", None),
          # The debounce: a second register inside the hour says nothing.
          ("the plan-limit notice is debounced",
           {"node_id": "node-aaaa1111", "cameras": [{"device_path": "/dev/video9"}]},
           "node:test-node-key",
           "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
           f" (9310,'self-host','plan_limit_notif_last_at',"
           f" '{PLAN_LIMIT_ANCHOR_RECENT}', timestamp '2026-06-01')"),
          ("a stale debounce lets it through",
           {"node_id": "node-aaaa1111", "cameras": [{"device_path": "/dev/video9"}]},
           "node:test-node-key",
           "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
           " (9311,'self-host','plan_limit_notif_last_at','2020-01-01T00:00:00',"
           " timestamp '2026-06-01')"),
          ("a malformed debounce is treated as never",
           {"node_id": "node-aaaa1111", "cameras": [{"device_path": "/dev/video9"}]},
           "node:test-node-key",
           "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
           " (9312,'self-host','plan_limit_notif_last_at','not-a-date',"
           " timestamp '2026-06-01')"),
          # Under the cap, so the camera is actually created — with the
          # capability list joined, the node_type defaulted, and the
          # device path sanitised into the id.
          ("a camera is created under a raised cap",
           {"node_id": "node-aaaa1111",
            "cameras": [{"device_path": "/dev/video 9", "name": "Created",
                         "node_type": "rtsp", "capabilities": ["streaming", "audio"]}]},
           "node:test-node-key", PRO_PLUS),
          ("an empty capability list takes the default",
           {"node_id": "node-aaaa1111",
            "cameras": [{"device_path": "/dev/video9", "capabilities": []}]},
           "node:test-node-key", PRO_PLUS),
          ("no device_path falls back to camera_id then unknown",
           {"node_id": "node-aaaa1111",
            "cameras": [{"camera_id": "from-id"}, {}]},
           "node:test-node-key", PRO_PLUS),
          ("codecs are sanitised and stamped",
           {"node_id": "node-aaaa1111", "video_codec": "avc1.64e00a",
            "audio_codec": "mp4a.40.2",
            "cameras": [{"device_path": "live"}]},
           "node:test-node-key", None),
          # Auth and existence.
          ("no key", {"node_id": "node-aaaa1111"}, "agent:none", None),
          # Validation runs before the handler body, so this is a 422
          # and not a 401. Without the combination, a port that checked
          # the key first looks identical on every case.
          ("no key and a bad body", {"node_id": 5}, "agent:none", None),
          ("no key and no body at all", b"", "agent:none", None),
          ("empty key", {"node_id": "node-aaaa1111"}, "node:", None),
          ("wrong key records the error", {"node_id": "node-aaaa1111"}, "node:wrong", None),
          ("another org's node is still found", {"node_id": "node-cccc3333"},
           "node:test-node-key", None),
          ("missing node", {"node_id": "nope"}, "node:test-node-key", None),
          # Versions: refused below the floor, and the reported version
          # is persisted either way so the dashboard can show it.
          ("an old version is refused",
           {"node_id": "node-aaaa1111", "node_version": "0.0.1"}, "node:test-node-key", None),
          ("a current version registers",
           {"node_id": "node-aaaa1111", "node_version": "9.9.9"}, "node:test-node-key", None),
          ("an unparseable version",
           {"node_id": "node-aaaa1111", "node_version": "banana"}, "node:test-node-key", None),
          ("an absent version clears the column",
           {"node_id": "node-aaaa1111"}, "node:test-node-key", None),
          # Validation.
          ("no node_id", {}, "node:test-node-key", None),
          ("node_id too long", {"node_id": "x" * 51}, "node:test-node-key", None),
          ("node_id not a string", {"node_id": 5}, "node:test-node-key", None),
          ("cameras not a list", {"node_id": "node-aaaa1111", "cameras": 5},
           "node:test-node-key", None),
          ("a camera that is not an object",
           {"node_id": "node-aaaa1111", "cameras": [5]}, "node:test-node-key", None),
          ("two bad fields in one camera",
           {"node_id": "node-aaaa1111", "cameras": [{"name": 5, "width": 0}]},
           "node:test-node-key", None),
          ("a bad field in the second camera",
           {"node_id": "node-aaaa1111",
            "cameras": [{"device_path": "live"}, {"capabilities": "streaming"}]},
           "node:test-node-key", None),
          ("http_port out of range",
           {"node_id": "node-aaaa1111", "http_port": 70000}, "node:test-node-key", None),
          ("http_port as a string",
           {"node_id": "node-aaaa1111", "http_port": "8080"}, "node:test-node-key", None),
          ("lan_streaming as a string",
           {"node_id": "node-aaaa1111", "lan_streaming": "no"}, "node:test-node-key", None),
          ("lan_streaming uncoercible",
           {"node_id": "node-aaaa1111", "lan_streaming": "maybe"}, "node:test-node-key", None),
          ("empty body", b"", "node:test-node-key", None),
          ("malformed json", b"{x", "node:test-node-key", None),
          ("list body", [1, 2], "node:test-node-key", None),
      ]],

    # --- CameraNode heartbeat -----------------------------------------
    *[(f"heartbeat: {label}", "POST", "/api/nodes/heartbeat", body, who, setup)
      for label, body, who, setup in [
          ("ok", {"node_id": "node-aaaa1111"}, "node:test-node-key", None),
          ("camera statuses are applied",
           {"node_id": "node-aaaa1111",
            "cameras": [{"camera_id": "cam-live", "status": "online"},
                        {"camera_id": "cam-restart", "status": "restarting",
                         "last_error": "pipeline stalled"},
                        {"camera_id": "cam-error", "status": "failed",
                         "last_error": "no such device"}]},
           "node:test-node-key", None),
          # A healthy status wipes last_error, so a recovered camera
          # stops showing a stale reason.
          ("a healthy status clears the error",
           {"node_id": "node-aaaa1111",
            "cameras": [{"camera_id": "cam-error", "status": "online",
                         "last_error": "ignored on a healthy status"}]},
           "node:test-node-key", None),
          # `cam-offline` exists and belongs to node-bbbb2222 in the same
          # org; `cam-theirs` belongs to another org. Both must be left
          # alone. A camera id that does not exist at all proves
          # nothing here — the UPDATE matches no row either way, which
          # is what the first version of this case did.
          ("a camera on another node of the same org is ignored",
           {"node_id": "node-aaaa1111",
            "cameras": [{"camera_id": "cam-offline", "status": "failed",
                         "last_error": "should not land"}]},
           "node:test-node-key", None),
          ("another org's camera is ignored",
           {"node_id": "node-aaaa1111",
            "cameras": [{"camera_id": "cam-theirs", "status": "failed",
                         "last_error": "should not land"}]},
           "node:test-node-key", None),
          ("the same camera twice applies in order",
           {"node_id": "node-aaaa1111",
            "cameras": [{"camera_id": "cam-live", "status": "failed", "last_error": "first"},
                        {"camera_id": "cam-live", "status": "online"}]},
           "node:test-node-key", None),
          # Storage, and the disk-low alert it drives.
          ("storage stats are persisted",
           {"node_id": "node-aaaa1111",
            "storage_stats": {"used_bytes": 5000, "max_bytes": 10000,
                              "disk_free_bytes": 50_000_000_000,
                              "disk_total_bytes": 100_000_000_000}},
           "node:test-node-key", None),
          ("a full disk alerts",
           {"node_id": "node-aaaa1111",
            "storage_stats": {"disk_free_bytes": 5_000_000_000,
                              "disk_total_bytes": 100_000_000_000}},
           "node:test-node-key", None),
          ("the alert is debounced for six hours",
           {"node_id": "node-aaaa1111",
            "storage_stats": {"disk_free_bytes": 5_000_000_000,
                              "disk_total_bytes": 100_000_000_000}},
           "node:test-node-key",
           "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
           f" (9320,'self-host','cameranode_disk_low_emit_at:node-aaaa1111',"
           f" '{DISK_ANCHOR_RECENT}', timestamp '2026-06-01')"),
          ("a malformed anchor is treated as never",
           {"node_id": "node-aaaa1111",
            "storage_stats": {"disk_free_bytes": 5_000_000_000,
                              "disk_total_bytes": 100_000_000_000}},
           "node:test-node-key",
           "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
           " (9323,'self-host','cameranode_disk_low_emit_at:node-aaaa1111',"
           " 'not-a-date', timestamp '2026-06-01')"),
          ("a stale anchor re-alerts",
           {"node_id": "node-aaaa1111",
            "storage_stats": {"disk_free_bytes": 5_000_000_000,
                              "disk_total_bytes": 100_000_000_000}},
           "node:test-node-key",
           "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
           " (9321,'self-host','cameranode_disk_low_emit_at:node-aaaa1111',"
           " '2020-01-01T00:00:00', timestamp '2026-06-01')"),
          # Back under the threshold: the anchor is cleared so the next
          # crossing alerts at once.
          ("recovery clears the anchor",
           {"node_id": "node-aaaa1111",
            "storage_stats": {"disk_free_bytes": 80_000_000_000,
                              "disk_total_bytes": 100_000_000_000}},
           "node:test-node-key",
           "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
           " (9322,'self-host','cameranode_disk_low_emit_at:node-aaaa1111',"
           " '2020-01-01T00:00:00', timestamp '2026-06-01')"),
          # Zero free bytes means "could not identify the disk", not a
          # full one — no alert.
          ("zero free bytes is unknown, not full",
           {"node_id": "node-aaaa1111",
            "storage_stats": {"disk_free_bytes": 0, "disk_total_bytes": 100_000_000_000}},
           "node:test-node-key", None),
          ("a partial storage block",
           {"node_id": "node-aaaa1111", "storage_stats": {"used_bytes": 1}},
           "node:test-node-key", None),
          ("an absent block leaves the last reading",
           {"node_id": "node-aaaa1111"}, "node:test-node-key", None),
          # The recording map. Only time-independent policies are
          # compared here — a window would straddle a minute boundary
          # between the two passes. The arithmetic is unit-tested.
          ("recording state: continuous and off",
           {"node_id": "node-aaaa1111"}, "node:test-node-key", None),
          ("recording state: a zero-length window",
           {"node_id": "node-aaaa1111"}, "node:test-node-key",
           "UPDATE cameras SET scheduled_recording = true, scheduled_start = '08:00',"
           " scheduled_end = '08:00' WHERE camera_id = 'cam-stale'"),
          ("recording state: a schedule with no times",
           {"node_id": "node-aaaa1111"}, "node:test-node-key",
           "UPDATE cameras SET scheduled_recording = true WHERE camera_id = 'cam-stale'"),
          ("recording state: an unparseable window",
           {"node_id": "node-aaaa1111"}, "node:test-node-key",
           "UPDATE cameras SET scheduled_recording = true, scheduled_start = 'abc',"
           " scheduled_end = 'def' WHERE camera_id = 'cam-stale'"),
          ("recording state: a bad timezone falls back to UTC",
           {"node_id": "node-aaaa1111"}, "node:test-node-key",
           pref("timezone", "Mars/Olympus")),
          # The three that make the zone itself observable. In UTC the
          # window is live; seven zones west it is not, and a port that
          # ignored the setting would answer the same for both.
          ("recording state: a live UTC window records",
           {"node_id": "node-aaaa1111"}, "node:test-node-key", window_setup("UTC")),
          ("recording state: the same window in another zone does not",
           {"node_id": "node-aaaa1111"}, "node:test-node-key",
           window_setup("America/Los_Angeles")),
          ("recording state: a bad zone falls back to UTC, not elsewhere",
           {"node_id": "node-aaaa1111"}, "node:test-node-key",
           window_setup("Mars/Olympus")),
          # Suspended cameras are named so the node stops pushing for
          # them instead of collecting 402s.
          ("disabled cameras are listed",
           {"node_id": "node-aaaa1111"}, "node:test-node-key",
           "UPDATE cameras SET disabled_by_plan = true"
           " WHERE camera_id IN ('cam-stale','cam-error')"),
          # The time-based past-due transition, which no webhook covers.
          ("past due within grace changes nothing",
           {"node_id": "node-aaaa1111"}, "node:test-node-key",
           past_due_setup(PAST_DUE_DAYS_LEFT)),
          ("past due beyond grace suspends cameras",
           {"node_id": "node-aaaa1111"}, "node:test-node-key",
           past_due_setup(PAST_DUE_EXPIRED)),
          # Auth, existence and versions.
          ("no key", {"node_id": "node-aaaa1111"}, "agent:none", None),
          ("no key and a bad body", {"node_id": 5}, "agent:none", None),
          ("wrong key", {"node_id": "node-aaaa1111"}, "node:wrong", None),
          ("missing node", {"node_id": "nope"}, "node:test-node-key", None),
          ("an old version is refused",
           {"node_id": "node-aaaa1111", "node_version": "0.0.1"}, "node:test-node-key", None),
          ("a current version heartbeats",
           {"node_id": "node-aaaa1111", "node_version": "9.9.9"}, "node:test-node-key", None),
          # Validation.
          ("no node_id", {}, "node:test-node-key", None),
          ("a camera status missing its status",
           {"node_id": "node-aaaa1111", "cameras": [{"camera_id": "cam-live"}]},
           "node:test-node-key", None),
          ("a camera status that is not an object",
           {"node_id": "node-aaaa1111", "cameras": ["cam-live"]}, "node:test-node-key", None),
          ("last_error too long",
           {"node_id": "node-aaaa1111",
            "cameras": [{"camera_id": "cam-live", "status": "failed",
                         "last_error": "x" * 501}]},
           "node:test-node-key", None),
          ("negative storage",
           {"node_id": "node-aaaa1111", "storage_stats": {"used_bytes": -1}},
           "node:test-node-key", None),
          ("storage that is not an object",
           {"node_id": "node-aaaa1111", "storage_stats": 5}, "node:test-node-key", None),
          ("storage bytes as a string",
           {"node_id": "node-aaaa1111", "storage_stats": {"used_bytes": "100"}},
           "node:test-node-key", None),
          ("empty body", b"", "node:test-node-key", None),
      ]],
]


# ---------------------------------------------------------------------
# Cases that only mean anything against the Clerk-mode pair.
#
# `resolve_org_plan` opens with
#
#     if settings.is_local_auth():
#         return "self_host"
#
# and the default tiers run AUTH_PROVIDER=local, so every plan lookup
# there returns one constant with a 999-camera cap. Against those tiers
# no camera is ever over cap, nothing is ever skipped, and the whole
# plan-cap half of registration — the skip loop, the skipped list, the
# notification and its debounce, the `plan_limit_hit` body — is
# unreachable. Five mutations survived the first run of this spec for
# exactly that reason, and every `org_plan` a case wrote was inert.
#
# These run against the 8100/8101 pair instead, where the short-circuit
# is off. They authenticate with a node API key, which is what makes it
# possible at all: the ~20 Clerk-gated routes would need real RS256
# session tokens from a Clerk instance neither tier has, and these need
# none. Every case pins a PAID slug, because `resolve_org_plan` returns
# a paid one from the Setting without calling Clerk — a free slug would
# reach for the network on every request.
#
# Run them with tests/differential/clerk_run.sh.
PRO = pref("org_plan", "pro")

# Epoch milliseconds, which is what Clerk puts in a webhook payload.
FUTURE_PERIOD_END = int((_PAST_DUE_NOW + timedelta(days=20)).timestamp() * 1000)
PAST_PERIOD_END = int((_PAST_DUE_NOW - timedelta(days=20)).timestamp() * 1000)

# Four more cameras, which puts the org exactly at Pro's cap of 25.
FILL_TO_CAP = PRO + (
    "INSERT INTO cameras (camera_id, org_id, node_id, name, status, node_type,"
    " capabilities, continuous_24_7, scheduled_recording, created_at, updated_at)"
    " SELECT 'filler-'||g, 'self-host',"
    " (SELECT id FROM camera_nodes WHERE node_id='node-bbbb2222'),"
    " 'Filler '||g, 'online', 'usb', 'streaming', false, false,"
    " timestamp '2026-01-01', timestamp '2026-01-01'"
    " FROM generate_series(1,4) g;"
)

CLERK_CASES = [
    # 21 cameras against Pro's 25: the first four land, the rest are
    # refused. The cap has to count what THIS request has created —
    # comparing against the pre-loop count alone lets one register
    # create as many cameras as it reports.
    ("cap: a batch crosses the cap mid-loop", "POST", "/api/nodes/register",
     {"node_id": "node-aaaa1111",
      "cameras": [{"device_path": f"/dev/v{i}"} for i in range(1, 7)]},
     "node:test-node-key", PRO),
    # Already at the cap, so even one new camera is refused.
    ("cap: at the cap, nothing new lands", "POST", "/api/nodes/register",
     {"node_id": "node-aaaa1111", "cameras": [{"device_path": "/dev/new"}]},
     "node:test-node-key", FILL_TO_CAP),
    # The notification names the first five and counts the rest.
    ("cap: eight refused lists five and a count", "POST", "/api/nodes/register",
     {"node_id": "node-aaaa1111",
      "cameras": [{"device_path": f"/dev/w{i}"} for i in range(1, 9)]},
     "node:test-node-key", FILL_TO_CAP),
    ("cap: exactly five refused adds no count", "POST", "/api/nodes/register",
     {"node_id": "node-aaaa1111",
      "cameras": [{"device_path": f"/dev/x{i}"} for i in range(1, 6)]},
     "node:test-node-key", FILL_TO_CAP),
    # A camera that already exists is updated, not counted against the
    # cap — otherwise a node at its limit could never re-register.
    ("cap: existing cameras are not re-counted", "POST", "/api/nodes/register",
     {"node_id": "node-aaaa1111",
      "cameras": [{"device_path": "live"}, {"device_path": "stale"},
                  {"device_path": "boundary"}, {"device_path": "restart"},
                  {"device_path": "error"}, {"device_path": "neverseen"}]},
     "node:test-node-key", FILL_TO_CAP),
    # The notification's debounce, which is only reachable once the
    # notification itself is.
    ("cap: the notice is debounced", "POST", "/api/nodes/register",
     {"node_id": "node-aaaa1111", "cameras": [{"device_path": "/dev/new"}]},
     "node:test-node-key",
     FILL_TO_CAP
     + "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
     + f" (9340,'self-host','plan_limit_notif_last_at','{PLAN_LIMIT_ANCHOR_RECENT}',"
     + " timestamp '2026-06-01');"),
    ("cap: a stale debounce lets it through", "POST", "/api/nodes/register",
     {"node_id": "node-aaaa1111", "cameras": [{"device_path": "/dev/new"}]},
     "node:test-node-key",
     FILL_TO_CAP
     + "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
     + " (9341,'self-host','plan_limit_notif_last_at','2020-01-01T00:00:00',"
     + " timestamp '2026-06-01');"),
    ("cap: a malformed debounce is treated as never", "POST", "/api/nodes/register",
     {"node_id": "node-aaaa1111", "cameras": [{"device_path": "/dev/new"}]},
     "node:test-node-key",
     FILL_TO_CAP
     + "INSERT INTO settings (id, org_id, key, value, updated_at) VALUES"
     + " (9342,'self-host','plan_limit_notif_last_at','not-a-date',"
     + " timestamp '2026-06-01');"),
    # Pro Plus has room, so the same batch lands in full and no notice
    # is emitted at all.
    ("cap: a higher tier takes the whole batch", "POST", "/api/nodes/register",
     {"node_id": "node-aaaa1111",
      "cameras": [{"device_path": f"/dev/v{i}"} for i in range(1, 7)]},
     "node:test-node-key", PRO_PLUS),
    # Registration's cap sweep, which is the safety net for an org whose
    # subscription webhook never arrived: the flags are recomputed even
    # though nothing about this request concerns them.
    ("cap: register re-runs enforcement", "POST", "/api/nodes/register",
     {"node_id": "node-aaaa1111"}, "node:test-node-key",
     PRO + "UPDATE cameras SET disabled_by_plan = true WHERE org_id = 'self-host';"),
    # The heartbeat's badge reads the Setting rather than resolving, and
    # its disabled list is scoped to this node.
    ("cap: the heartbeat badge reads the setting", "POST", "/api/nodes/heartbeat",
     {"node_id": "node-aaaa1111"}, "node:test-node-key", PRO),
    ("cap: the heartbeat lists this node's suspended cameras", "POST",
     "/api/nodes/heartbeat", {"node_id": "node-aaaa1111"}, "node:test-node-key",
     PRO + "UPDATE cameras SET disabled_by_plan = true WHERE org_id = 'self-host';"),
    # Past due beyond grace drops the caps to free — 5 against 21
    # cameras, so the sweep has plenty to suspend.
    ("cap: past due beyond grace tightens to free", "POST", "/api/nodes/heartbeat",
     {"node_id": "node-aaaa1111"}, "node:test-node-key",
     PRO + past_due_setup(PAST_DUE_EXPIRED)),
    ("cap: past due within grace keeps the tier", "POST", "/api/nodes/heartbeat",
     {"node_id": "node-aaaa1111"}, "node:test-node-key",
     PRO + past_due_setup(PAST_DUE_DAYS_LEFT)),

    # --- The Clerk webhook --------------------------------------------
    #
    # main.py mounts the webhooks router under Clerk only, so this route
    # does not exist on the local pair at all — Python answers 404 there
    # and so must the port. Every case is Svix-signed with Clerk's own
    # secret, which is separate from Resend's: a port that verified
    # either against the other would accept a forged plan change.
    #
    # `payer.organization_id` is how a subscription event names its org,
    # and `self-host` is the org the fixture is built around.
    *[(f"clerk: {label}", "POST", "/api/webhooks/clerk", body, who, setup)
      for label, body, who, setup in [
          # Signature, which is the whole security boundary here.
          ("unsigned", {"type": "subscription.active", "data": {}}, "agent:none", None),
          ("bad signature", {"type": "subscription.active", "data": {}},
           "clerk-bad:msg_c1", None),
          ("a stale timestamp", {"type": "subscription.active", "data": {}},
           "clerk-stale:msg_c2", None),
          ("a future timestamp", {"type": "subscription.active", "data": {}},
           "clerk-future:msg_c3", None),
          ("signed with Resend's secret", {"type": "subscription.active", "data": {}},
           "svix:msg_c4", None),
          ("the webhook- header set", {"type": "subscription.active", "data": {}},
           "clerk-webhook:msg_c5", None),
          ("signed but not JSON", b"{x", "clerk:msg_c6", None),
          ("signed but not an object", [1, 2], "clerk:msg_c7", None),
          ("signed empty body", b"", "clerk:msg_c8", None),
          # Dedup: the same msg id twice is answered once.
          ("a replayed delivery", {"type": "subscription.active", "data": {}},
           "clerk:msg_seen", "INSERT INTO processed_webhooks (svix_msg_id, event_type, processed_at)"
           " VALUES ('msg_seen','subscription.active', timestamp '2026-06-01')"),
          # An event nobody handles still records itself as processed.
          ("an unknown event type", {"type": "user.created", "data": {"id": "u_1"}},
           "clerk:msg_c9", None),
          ("no type at all", {"data": {}}, "clerk:msg_c10", None),
          # Subscription lifecycle. The first active item wins.
          ("subscription active on pro",
           {"type": "subscription.active",
            "data": {"payer": {"organization_id": "self-host"},
                     "items": [{"status": "active", "plan": {"slug": "pro"}}]}},
           "clerk:msg_s1", None),
          ("subscription updated to pro_plus",
           {"type": "subscription.updated",
            "data": {"payer": {"organization_id": "self-host"},
                     "items": [{"status": "active", "plan": {"slug": "pro_plus"}}]}},
           "clerk:msg_s2", PRO),
          # A downgrade suspends the cameras past the new cap, without
          # deleting anything.
          ("a downgrade suspends over-cap cameras",
           {"type": "subscription.updated",
            "data": {"payer": {"organization_id": "self-host"},
                     "items": [{"status": "active", "plan": {"slug": "pro"}}]}},
           "clerk:msg_s3", PRO_PLUS),
          # A canceled item still inside its period keeps the tier; the
          # snapshot Clerk sends right after a cancel click contains
          # only that item, and reading it as "free" downgraded paying
          # customers on the spot.
          ("a canceled item paid through keeps its plan",
           {"type": "subscription.updated",
            "data": {"payer": {"organization_id": "self-host"},
                     "items": [{"status": "canceled", "plan": {"slug": "pro"},
                                "period_end": FUTURE_PERIOD_END}]}},
           "clerk:msg_s4", None),
          ("a canceled item past its period does not",
           {"type": "subscription.updated",
            "data": {"payer": {"organization_id": "self-host"},
                     "items": [{"status": "canceled", "plan": {"slug": "pro"},
                                "period_end": PAST_PERIOD_END}]}},
           "clerk:msg_s5", None),
          # Two canceled-but-paid items: the FIRST wins, matching the
          # loop's order. Without a second one, nothing distinguishes
          # "first wins" from "last wins".
          ("the first canceled-but-paid item wins",
           {"type": "subscription.updated",
            "data": {"payer": {"organization_id": "self-host"},
                     "items": [{"status": "canceled", "plan": {"slug": "pro"},
                                "period_end": FUTURE_PERIOD_END},
                               {"status": "canceled", "plan": {"slug": "pro_plus"},
                                "period_end": FUTURE_PERIOD_END}]}},
           "clerk:msg_s14", None),
          ("an active item beats a canceled one, whatever the order",
           {"type": "subscription.updated",
            "data": {"payer": {"organization_id": "self-host"},
                     "items": [{"status": "canceled", "plan": {"slug": "pro_plus"},
                                "period_end": FUTURE_PERIOD_END},
                               {"status": "active", "plan": {"slug": "pro"}}]}},
           "clerk:msg_s6", None),
          ("an item with no slug is skipped",
           {"type": "subscription.updated",
            "data": {"payer": {"organization_id": "self-host"},
                     "items": [{"status": "active", "plan": {}},
                               {"status": "active", "plan": {"slug": "pro"}}]}},
           "clerk:msg_s7", None),
          ("no items at all is free",
           {"type": "subscription.updated",
            "data": {"payer": {"organization_id": "self-host"}, "items": []}},
           "clerk:msg_s8", PRO),
          ("no payer organization",
           {"type": "subscription.active", "data": {"items": []}}, "clerk:msg_s9", None),
          # A paid snapshot must NOT clear a held past-due flag: during
          # dunning the item stays active with its paid slug, and
          # clearing here reset the grace clock every cycle.
          ("a paid snapshot leaves a held past-due alone",
           {"type": "subscription.updated",
            "data": {"payer": {"organization_id": "self-host"},
                     "items": [{"status": "active", "plan": {"slug": "pro"}}]}},
           "clerk:msg_s10", past_due_setup(PAST_DUE_DAYS_LEFT)),
          ("a paid snapshot clears past-due when none is held",
           {"type": "subscription.updated",
            "data": {"payer": {"organization_id": "self-host"},
                     "items": [{"status": "active", "plan": {"slug": "pro"}}]}},
           "clerk:msg_s11", None),
          # Only an ACTIVE paid item clears a pending cancellation.
          ("an active item clears a pending cancel",
           {"type": "subscription.updated",
            "data": {"payer": {"organization_id": "self-host"},
                     "items": [{"status": "active", "plan": {"slug": "pro"}}]}},
           "clerk:msg_s12", pref("plan_cancel_pending", "true")),
          ("a canceled-but-paid item does not",
           {"type": "subscription.updated",
            "data": {"payer": {"organization_id": "self-host"},
                     "items": [{"status": "canceled", "plan": {"slug": "pro"},
                                "period_end": FUTURE_PERIOD_END}]}},
           "clerk:msg_s13", pref("plan_cancel_pending", "true")),
          # The item-level activation, which is the authoritative
          # "payment went through" signal.
          ("item active on pro",
           {"type": "subscriptionItem.active",
            "data": {"payer": {"organization_id": "self-host"},
                     "plan": {"slug": "pro"}}}, "clerk:msg_i1", None),
          ("item active on an unknown slug does nothing",
           {"type": "subscriptionItem.active",
            "data": {"payer": {"organization_id": "self-host"},
                     "plan": {"slug": "enterprise"}}}, "clerk:msg_i2", PRO),
          ("item active with no plan",
           {"type": "subscriptionItem.active",
            "data": {"payer": {"organization_id": "self-host"}}}, "clerk:msg_i3", None),
          # Past due: the anchor is stamped only on entry.
          ("past due stamps an anchor",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"}}}, "clerk:msg_p1", PRO),
          ("past due again does not re-stamp",
           {"type": "subscriptionItem.pastDue",
            "data": {"payer": {"organization_id": "self-host"}}},
           "clerk:msg_p2", PRO + past_due_setup(PAST_DUE_DAYS_LEFT)),
          ("past due with epoch millis",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"},
                     "past_due_at": 1750000000123}}, "clerk:msg_p3", PRO),
          ("past due with epoch seconds",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"},
                     "past_due_at": 1750000000}}, "clerk:msg_p4", PRO),
          ("past due with camelCase",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"},
                     "pastDueAt": 1750000000123}}, "clerk:msg_p5", PRO),
          ("past due with a string timestamp",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"},
                     "past_due_at": "2026-06-01T00:00:00+00:00"}}, "clerk:msg_p6", PRO),
          # Both zero: `A or B` yields B, which is not None, so it
          # parses and stamps 1970 — where past_due_at alone being zero
          # stamps now.
          ("past due with both keys zero",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"},
                     "past_due_at": 0, "pastDueAt": 0}}, "clerk:msg_p7", PRO),
          ("past due with only past_due_at zero",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"},
                     "past_due_at": 0}}, "clerk:msg_p8", PRO),
          ("past due with a bool",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"},
                     "past_due_at": True}}, "clerk:msg_p9", PRO),
          ("past due with an unparseable value",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"},
                     "past_due_at": "whenever"}}, "clerk:msg_p10", PRO),
          # Out of range: Python's except clause catches TypeError and
          # ValueError, and these raise OverflowError or OSError, which
          # propagate. A 500 rather than a stored string — and the
          # delivery must NOT be recorded as processed, because Svix
          # has to be able to retry it.
          ("past due past the year an int can hold",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"},
                     "past_due_at": 1e30}}, "clerk:msg_p12", PRO),
          ("past due with infinity as a string",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"},
                     "past_due_at": "inf"}}, "clerk:msg_p13", PRO),
          # Inside the datetime range but outside year 1..9999, which
          # IS caught — so this one stores the string and answers 200.
          ("past due past year 9999",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"},
                     "past_due_at": 253402300800}}, "clerk:msg_p14", PRO),
          ("past due with a list",
           {"type": "subscription.pastDue",
            "data": {"payer": {"organization_id": "self-host"},
                     "past_due_at": [1]}}, "clerk:msg_p11", PRO),
          # Payment recovered.
          ("a paid attempt clears past-due and re-enables",
           {"type": "paymentAttempt.updated",
            "data": {"payer": {"organization_id": "self-host"}, "status": "paid"}},
           "clerk:msg_a1", PRO + past_due_setup(PAST_DUE_EXPIRED)),
          ("a failed attempt changes nothing",
           {"type": "paymentAttempt.updated",
            "data": {"payer": {"organization_id": "self-host"}, "status": "failed"}},
           "clerk:msg_a2", PRO + past_due_setup(PAST_DUE_DAYS_LEFT)),
          # Cancellation: scheduled, then actually ended.
          ("a scheduled cancel records the marker",
           {"type": "subscriptionItem.canceled",
            "data": {"payer": {"organization_id": "self-host"}}}, "clerk:msg_x1", PRO),
          ("the trial-ending notice does nothing",
           {"type": "subscriptionItem.freeTrialEnding",
            "data": {"payer": {"organization_id": "self-host"}}}, "clerk:msg_x2", PRO),
          # Membership audit.
          ("a member was added",
           {"type": "organizationMembership.created",
            "data": {"organization": {"id": "self-host"},
                     "public_user_data": {"identifier": "new@example.com",
                                          "user_id": "user_1"},
                     "role": "org:member"}}, "clerk:msg_m1", None),
          ("an admin was added",
           {"type": "organizationMembership.created",
            "data": {"organization": {"id": "self-host"},
                     "public_user_data": {"identifier": "boss@example.com"},
                     "role": "org:admin"}}, "clerk:msg_m2", None),
          ("a member with no identifier falls back to the user id",
           {"type": "organizationMembership.created",
            "data": {"organization": {"id": "self-host"},
                     "public_user_data": {"user_id": "user_2"}}}, "clerk:msg_m3", None),
          ("a member with nothing at all",
           {"type": "organizationMembership.created",
            "data": {"organization": {"id": "self-host"}}}, "clerk:msg_m4", None),
          ("a role with no org prefix",
           {"type": "organizationMembership.updated",
            "data": {"organization": {"id": "self-host"},
                     "public_user_data": {"identifier": "x@example.com"},
                     "role": "admin"}}, "clerk:msg_m5", None),
          ("a role change to member",
           {"type": "organizationMembership.updated",
            "data": {"organization": {"id": "self-host"},
                     "public_user_data": {"identifier": "x@example.com"},
                     "role": "org:member"}}, "clerk:msg_m6", None),
          ("a member was removed",
           {"type": "organizationMembership.deleted",
            "data": {"organization": {"id": "self-host"},
                     "public_user_data": {"identifier": "gone@example.com",
                                          "user_id": "user_3"}}}, "clerk:msg_m7", None),
          ("a membership event with no organization",
           {"type": "organizationMembership.created",
            "data": {"public_user_data": {"identifier": "x@example.com"}}},
           "clerk:msg_m8", None),
          # Org lifecycle.
          ("an organization was created",
           {"type": "organization.created",
            "data": {"id": "self-host", "name": "Acme", "created_by": "user_9"}},
           "clerk:msg_o1", None),
          ("an organization created with no name",
           {"type": "organization.created", "data": {"id": "self-host"}},
           "clerk:msg_o2", None),
          ("an organization deleted wipes everything",
           {"type": "organization.deleted", "data": {"id": "self-host"}},
           "clerk:msg_o3", None),
          ("an organization deleted with no id",
           {"type": "organization.deleted", "data": {}}, "clerk:msg_o4", None),
      ]],
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

# The unsubscribe link in every email footer, which is a JWT *inside* a
# larger string — one per recipient, substituted into `body_text` and
# `body_html`. The bare-JWT rule above cannot see it.
UNSUB_LINK = re.compile(
    r"(/api/notifications/email/unsubscribe\?t=)"
    r"([A-Za-z0-9_-]+\.([A-Za-z0-9_-]+)\.[A-Za-z0-9_-]+)"
)


def _unsub_claims(match):
    """Replace the token with its own claims, minus the two that move.

    Not a blanket `<jwt>`: `org_id`, `kind`, `rcpt` and `sub` are the
    whole content of the link and have to be compared. Only `iat` and
    `exp` differ between the two runs, and only when they straddle a
    second boundary.

    The signature bytes go uncompared, which is sound because they are a
    function of the header, these claims and the derived secret — and
    that derivation is held to PyJWT's exact output by a unit test, not
    inferred here.
    """
    raw = match.group(3)
    try:
        payload = base64.urlsafe_b64decode(raw + "=" * (-len(raw) % 4))
        claims = json.loads(payload)
    except Exception:  # noqa: BLE001 — an unparseable token is itself the difference
        return match.group(1) + "<unparseable-jwt>"
    claims.pop("iat", None)
    claims.pop("exp", None)
    return match.group(1) + "<jwt:" + json.dumps(claims, sort_keys=True) + ">"


# Values random by construction whose *name* identifies them more
# safely than their content could. `key_last4` is four hex characters —
# short enough that substituting it by value would rewrite an unrelated
# run of four characters inside some other hash, differently on each
# tier, and manufacture a difference out of nothing. That it really is
# the last four characters of the minted key is held by a unit test,
# which is the right place for an invariant that needs the key itself.
RANDOM_FIELDS = {"key_last4": "<issued-key-last4>"}

# The same field, but inside a JSON *string* — the audit `details`
# column, which is text and so never reaches the dict rule above.
# Anchored on the field name for the same reason.
LAST4_IN_JSON = re.compile(r'("key_last4":\s*")[^"]{0,8}(")')

# An ISO timestamp inside a JSON string column. `meta_json` on a motion
# notification carries `event_timestamp`, which is a server clock
# reading when the node sent none — so the two tiers write values that
# differ by however long the first run took. The value-level rule below
# never sees it, because the column is one long string.
#
# Anchored on the JSON shape and still subject to the same recency
# test, so a timestamp the *fixture* put there stays compared.
TS_IN_JSON = re.compile(r'("\w+":\s*")(\d{4}-\d{2}-\d{2}T[\d:.+-]{8,})(")')


def normalise(value, now):
    """Replace just-written timestamps with a token, recursively.

    A freshly signed session token differs between the two runs by its
    `iat`/`exp` alone, so only its shape is compared. The token is
    proven equivalent elsewhere: both stacks verify each other's, which
    is what the HTTP differential runs on.
    """
    if isinstance(value, dict):
        return {
            k: (RANDOM_FIELDS[k] if k in RANDOM_FIELDS and isinstance(v, str)
                else normalise(v, now))
            for k, v in value.items()
        }
    if isinstance(value, list):
        return [normalise(v, now) for v in value]
    if isinstance(value, str) and JWT.match(value):
        return "<jwt>"
    if isinstance(value, str) and "/api/notifications/email/unsubscribe?t=" in value:
        value = UNSUB_LINK.sub(_unsub_claims, value)
    if isinstance(value, str) and "key_last4" in value:
        value = LAST4_IN_JSON.sub(r"\1<issued-key-last4>\2", value)
    if isinstance(value, str) and '":' in value and "T" in value:
        value = TS_IN_JSON.sub(
            lambda m: m.group(1) + normalise(m.group(2), now) + m.group(3), value
        )
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
                    out.append(f"row {k!r} field {field}: "
                               + field_diff(py[k].get(field), rs[k].get(field)))
    return out or ["(rows equal but order differs)"]


def field_diff(python_value, rust_value, width=200):
    """Show both sides of one differing column, windowed on the divergence.

    Truncating the *formatted pair* — which this did — drops the rust
    side entirely whenever the python side is long, and the columns that
    differ most interestingly are the long ones: a JSON blob of delete
    counts, a meta_json, an audit `details`. Each side is truncated
    separately, and when both are long the window is centred on the
    first character that actually differs, because two 4KB JSON strings
    that diverge at character 900 are otherwise identical on screen.
    """
    a, b = repr(python_value), repr(rust_value)
    if len(a) <= width and len(b) <= width:
        return f"python={a} rust={b}"
    at = 0
    while at < min(len(a), len(b)) and a[at] == b[at]:
        at += 1
    start = max(0, at - width // 4)

    def window(text):
        return (("…" if start else "") + text[start:start + width]
                + ("…" if start + width < len(text) else ""))

    return (f"diverges at char {at}\n"
            f"                python={window(a)}\n"
            f"                rust=  {window(b)}")


RESEND_SECRET = os.environ.get(
    "RESEND_WEBHOOK_SECRET", "whsec_aGFybmVzcy13ZWJob29rLXNlY3JldC0xMjM0NTY=")
CLERK_SECRET = os.environ.get(
    "CLERK_WEBHOOK_SECRET", "whsec_Y2xlcmstaGFybmVzcy1zZWNyZXQtNjU0MzIxMDA=")


def sign_svix(req, variant, msg_id, data, secret=None):
    from datetime import timedelta  # noqa: PLC0415

    from svix.webhooks import Webhook  # noqa: PLC0415

    when = datetime.now(timezone.utc)
    if variant == "svix-stale":
        when -= timedelta(minutes=10)
    elif variant == "svix-future":
        when += timedelta(minutes=10)
    signature = Webhook(secret or RESEND_SECRET).sign(
        msg_id, when, data.decode("utf-8", "replace"))
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
    elif who == "agent:none" or who == "none":
        # No credential at all. The unsubscribe link is public — its
        # signed token is the authority — and sending a session token
        # anyway would let a port that wrongly required one still pass.
        pass
    elif who.startswith("clerk"):
        # A Clerk delivery. Same library and the same variants, signed
        # with Clerk's own secret — the two webhooks have separate
        # secrets in production, and a port that verified either one
        # against the other would accept a forged plan change.
        variant, _, msg_id = who.partition(":")
        variant = variant.replace("clerk", "svix", 1)
        sign_svix(req, variant, msg_id, data or b"", secret=CLERK_SECRET)
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

    What is not arbitrary is the *grouping* of the two cascade children:
    Python loads them per parent (`for inc in incidents: for ev in
    inc.evidence`), so every row of one parent is contiguous. A port
    that replaced that with one flat join would interleave them, so
    that property is compared explicitly.

    The *order the parents come in* is arbitrary, though, and comparing
    it was a latent flake: `db.query(CameraNode).filter_by(...).all()`
    has no ORDER BY either, so the parent sequence is the same heap
    order as the rows, and it flipped between runs on its own. What is
    compared instead is the set of parents plus contiguity — a flat
    join still fails, because it makes a parent appear in more than one
    run and `contiguous` goes false.
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
        out["grouped_by"] = sorted(seen, key=str)
        out["grouping_is_contiguous"] = len(seen) == len(set(seen))
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
    rows = snapshot()
    subs = {**dispatched_run_ids(rows), **issued_secrets(parsed), **issued_ids(parsed)}
    return (status, substitute(normalise(parsed, now), subs),
            substitute(normalise(rows, now), subs))


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
    if not isinstance(parsed, dict):
        return subs
    # `api_key` is a node's; `key` is what the three key-minting routes
    # call theirs. Both are returned once and stored only as a hash.
    for field in ("api_key", "key"):
        value = parsed.get(field)
        if isinstance(value, str) and value:
            subs[value] = f"<issued-{field}>"
            subs[hashlib.sha256(value.encode()).hexdigest()] = f"<sha256-of-issued-{field}>"
    if not isinstance(parsed.get("api_key"), str):
        return subs
    key = parsed["api_key"]
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


def dispatched_run_ids(rows):
    """Run ids the *dispatcher* minted, which no response carries.

    `issued_ids` reads the id out of the response, which works for
    `POST /runs/manual` — the response is the run. A run dispatched from
    a notification has no such response: filing an incident returns the
    incident, and the run appears only in the side-effect snapshot with
    a fresh uuid4 that differs by construction between the two passes.

    Scoped to this one column rather than matching 32 hex characters
    anywhere, which would also blank anything else that happened to
    look like one. Seeded ids spell "run0000…", which is not hex, so
    only a generated one matches.
    """
    return {
        row["id"]: "<dispatched-run-id>"
        for row in rows.get("sentinel_runs", [])
        if re.fullmatch(r"[0-9a-f]{32}", str(row.get("id", "")))
    }


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
    # CASE_SET=clerk swaps in the cases that need the Clerk-mode pair.
    # They are a separate list rather than a filter over CASES because
    # running them against the local pair does not fail — it passes,
    # vacuously, which is the worst of the three outcomes.
    pool = CLERK_CASES if os.environ.get("CASE_SET") == "clerk" else CASES
    cases = [c for c in pool if not wanted or any(w in c[0] or w in c[2] for w in wanted)]
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

    scope = f" [DIFF_ONLY={DIFF_ONLY!r}: {len(cases)} of {len(pool)} cases]" if DIFF_ONLY else ""
    print(f"{len(cases) - bad}/{len(cases)} identical (response + side effects), {bad} differing{scope}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
