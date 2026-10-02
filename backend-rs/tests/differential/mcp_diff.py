#!/usr/bin/env python3
"""The MCP protocol surface, compared method by method.

`POST /mcp/` is JSON-RPC, so it is neither a read nor a write case: one
path, one method name, and the interesting variable is which credential
asked. This drives both stacks through the same requests and compares
the parsed responses.

Reseeds before EVERY case, like the write differential, and for the same
reason plus one more: `effective_status` turns a camera offline ninety
seconds after its last heartbeat, so a fixture that ages between the two
calls makes `get_system_status` report different counts for reasons that
have nothing to do with the port.

THREE THINGS ARE DELIBERATELY NOT COMPARED. The first two are framework
surface rather than product behaviour; the third is not a contract on
either side.

  * `serverInfo.version`, which on the Python is fastmcp's own release
    number. Matching it would mean hard-coding another project's
    version into a port whose purpose is to delete that project.
  * every tool's `inputSchema`, `outputSchema` and `_meta.fastmcp`, and
    a result's `_meta`. FastMCP derives the schemas from Python type
    hints and stamps its own metadata on each tool; the port writes
    schemas by hand and stamps nothing. The NAMES, TITLES, DESCRIPTIONS
    and read-only ANNOTATIONS are compared, because those are what a
    client shows a user and what the scope picker renders.
  * the ROW ORDER of the five answers that have no ORDER BY behind
    them — see `_UNORDERED`. Everything else keeps its order, because
    most lists here are sorted and the sort is the answer.

Everything else is compared exactly, including the text content block's
BYTES: it is re-parsed, blanked of moving timestamps and re-serialised
with Python's compact separators, so a spacing or key-order difference
inside it still shows. That matters because the text block and
`structuredContent` are two renderings of one value and a client may
read either.

Usage: mcp_diff.py [-v]

Env:
  DIFF_ONLY   pipe-separated substrings; only matching case labels run.
              "node commands" selects the socket-backed section.
"""
from __future__ import annotations

import asyncio
import json
import os
import pathlib
import subprocess
import sys
import urllib.error
import urllib.request

import websockets

HERE = pathlib.Path(__file__).resolve().parent
PG_CONTAINER = os.environ.get("PG_CONTAINER", "cc-schema-test")
REDIS_CONTAINER = os.environ.get("REDIS_CONTAINER", "cc-redis-test")
PORTS = {"python": 8001, "rust": 8000}
VERBOSE = "-v" in sys.argv
DIFF_ONLY = os.environ.get("DIFF_ONLY", "")

# Keys from the fixture. Only #14 carries a real hash for an MCP-kind
# row, which is why it is the one that authenticates.
LIVE_KEY = "osc_mcp_kind_key"
AGENT_SCOPED = "osa_00000000000000000000000000000001"
AGENT_SHARED = "harness-agent-mcp-key"

# The four scope modes, each on a key whose hash is real so the gate
# actually runs. Fixture rows 15-18; see the comment there for why they
# had to be added — the gate had never been exercised over the wire.
READONLY_KEY = "osc_readonly_key"
CUSTOM_KEY = "osc_custom_key"
BAD_SCOPE_KEY = "osc_badscope_key"
EMPTY_SCOPE_KEY = "osc_emptyscope_key"

# An MCP key with a real hash AND revoked=true. Fixture row 19, added
# because `osc_revoked_key` below hashes to nothing: the only real-hash
# revoked row was an INTEGRATION key, so dropping `revoked = false` from
# either MCP query changed nothing and two mutations scored zero on a
# case the fixture could not express.
REVOKED_LIVE_KEY = "osc_revoked_mcp_key"


def psql(sql: str) -> str:
    return subprocess.run(
        ["docker", "exec", "-i", PG_CONTAINER, "psql", "-U", "cc", "-d", "cc", "-tAq", "-c", sql],
        capture_output=True, text=True, check=False, timeout=60,
    ).stdout



# Set by dialect_run.sh: the tier on :8001 is the SQLite build and this
# is its file. The fixture is PostgreSQL's, so it is applied there and
# copied across — see dialect.py.
SQLITE_DB = os.environ.get("DIALECT_SQLITE_DB", "")


def mirror_to_sqlite() -> None:
    if SQLITE_DB:
        import dialect
        dialect.copy_from_postgres(SQLITE_DB)

def reseed() -> None:
    seed = (HERE / "seed_cameras.sql").read_text()
    result = subprocess.run(
        ["docker", "exec", "-i", PG_CONTAINER, "psql", "-U", "cc", "-d", "cc", "-q"],
        input=seed, capture_output=True, text=True, check=False, timeout=120,
    )
    if result.returncode != 0:
        raise SystemExit(f"reseed failed: {result.stderr.strip()[:300]}")
    subprocess.run(["docker", "exec", REDIS_CONTAINER, "redis-cli", "FLUSHDB"],
                   capture_output=True, check=False, timeout=60)
    mirror_to_sqlite()


def rpc(port: int, body: dict, key: str | None, extra: dict | None = None):
    """One JSON-RPC call, returning (status, parsed body)."""
    headers = {
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
    }
    if key is not None:
        headers["Authorization"] = f"Bearer {key}"
    headers.update(extra or {})
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}/mcp/", data=json.dumps(body).encode(), headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            raw = response.read()
            status = response.status
    except urllib.error.HTTPError as err:
        raw, status = err.read(), err.code
    except Exception as exc:  # noqa: BLE001
        return None, {"transport_error": type(exc).__name__}
    text = raw.decode("utf-8", "replace")
    # An SSE frame would arrive as `data: {...}`; both stacks answer
    # JSON here, and reading either keeps a framing change visible as a
    # difference rather than a crash.
    if text.startswith("data: "):
        text = text[len("data: "):]
    try:
        return status, json.loads(text)
    except json.JSONDecodeError:
        return status, {"unparseable": text[:300]}


# Fields written at request time, which differ by however many seconds
# apart the two calls were made.
_MOVING = ("created_at", "updated_at", "timestamp", "resolved_at", "last_seen",
           "last_used_at", "accessed_at")


# Every place a tool's answer has no ORDER BY behind it, so its row
# order is not part of the contract on EITHER side.
#
# `http_diff.py` reached the same conclusion for the REST routes and
# states it in its module docstring: a list route that emits
# SQLAlchemy's unordered `.all()` is compared as a multiset, because
# "anything order-dependent would be a flake, not a finding". Postgres
# answers an unordered scan in physical order, and the physical order
# after a few hundred DELETE-and-reinsert reseeds is not the insert
# order — so this surfaced twice, on different cases, before it was
# named.
#
# Narrower than http_diff's blanket sort on purpose: most lists here ARE
# ordered and the order is the answer. `list_incidents` is created_at
# DESC, `get_stream_logs` is accessed_at DESC, `get_incident`'s evidence
# is timestamp ASC, `watch_camera`'s frames are capture order, and the
# content blocks are a sequence. Sorting those away would drop real
# coverage.
#
# `by_camera` / `by_user` are `get_stream_stats`'s two GROUP BYs.
# `result` is the wrapper FastMCP puts a list return under, which is
# how the three list tools' arrays arrive — and it only sorts a LIST, so
# the JSON-RPC envelope's own `result` object is untouched.
_UNORDERED = ("by_camera", "by_user", "result")

# The tools whose top-level return is an unordered array. Their TEXT
# block is a bare array rather than a value under a key, so it needs the
# tool's name to know whether to sort.
_UNORDERED_TOOLS = ("list_cameras", "list_camera_groups", "list_nodes")


def _row_key(item):
    """http_diff.py's key, so the two harnesses order a row the same way."""
    if isinstance(item, dict):
        for k in ("camera_id", "id", "name"):
            if k in item:
                return (0, str(item[k]))
    return (1, json.dumps(item, sort_keys=True))


def _stamp(value):
    """Blank the timestamps, recursively, wherever they appear.

    A tool result carries them inside `structuredContent` AND inside the
    text block's serialised copy, so both have to be reached — the text
    is re-parsed, blanked and re-serialised compactly, the way FastMCP
    wrote it.

    Also sorts the two aggregate lists whose row ORDER is not part of
    either stack's contract. `get_stream_stats` groups without an
    ORDER BY (unlike the REST stats route, which sorts by count), so
    Postgres answers in whatever order the aggregate produced — and it
    does not have to answer the same way twice. That showed up as one
    intermittently differing case out of 131: `total_views` and
    `by_camera` agreed and `by_user` came back permuted.

    Sorting here rather than adding an ORDER BY to the port on purpose:
    an ORDER BY the Python does not have is a different answer, not a
    tidier one. What this hides is real, and it is recorded in
    expected_divergences.md — a client must not depend on the order of
    those two lists from either stack.
    """
    if isinstance(value, dict):
        out = {}
        for k, v in value.items():
            if k in _MOVING and v is not None:
                out[k] = "<moving>"
            elif k in _UNORDERED and isinstance(v, list):
                out[k] = sorted((_stamp(row) for row in v), key=_row_key)
            else:
                out[k] = _stamp(v)
        return out
    if isinstance(value, list):
        return [_stamp(v) for v in value]
    return value


def _blank_text_block(text, sort_rows=False):
    try:
        parsed = json.loads(text)
    except (json.JSONDecodeError, TypeError):
        return text
    stamped = _stamp(parsed)
    if sort_rows and isinstance(stamped, list):
        stamped = sorted(stamped, key=_row_key)
    return json.dumps(stamped, separators=(",", ":"))


def normalise(payload, tool=None):
    """Drop the framework surface; keep the product behaviour."""
    if not isinstance(payload, dict):
        return payload
    sort_rows = tool in _UNORDERED_TOOLS
    result = payload.get("result")
    if isinstance(result, dict):
        # `_meta.fastmcp.wrap_result` rides along on some results. It is
        # the framework describing its own wrapping, not an answer.
        result.pop("_meta", None)
        if isinstance(result.get("structuredContent"), (dict, list)):
            result["structuredContent"] = _stamp(result["structuredContent"])
        for block in result.get("content", []) or []:
            if isinstance(block, dict) and isinstance(block.get("text"), str):
                block["text"] = _blank_text_block(block["text"], sort_rows)
        info = result.get("serverInfo")
        if isinstance(info, dict) and "version" in info:
            info["version"] = "<framework version>"
        tools = result.get("tools")
        if isinstance(tools, list):
            for tool in tools:
                if not isinstance(tool, dict):
                    continue
                for field in ("inputSchema", "outputSchema", "_meta"):
                    tool.pop(field, None)
            # tools/list has no defined order, and the two build it from
            # different sources.
            result["tools"] = sorted(tools, key=lambda t: t.get("name", ""))
    return payload


CASES: list[tuple] = [
    # ---- the handshake ------------------------------------------------
    ("initialize", {"method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "probe", "version": "1"}}}, LIVE_KEY, None),

    # ---- the catalog, per credential ----------------------------------
    # Key 1 is scope_mode 'all', 2 is 'custom', 3 is a legacy NULL, 7
    # and 8 hold malformed scope lists. Only #14's hash is real, so the
    # others exercise the unrecognised-key path — which does NOT filter,
    # because the middleware defers to the tool's own auth.
    ("tools/list: a live key", {"method": "tools/list", "params": {}}, LIVE_KEY, None),
    ("tools/list: no credential", {"method": "tools/list", "params": {}}, None, None),
    ("tools/list: an unknown key", {"method": "tools/list", "params": {}}, "osc_nope", None),
    ("tools/list: an integration key", {"method": "tools/list", "params": {}},
     "osi_live_integration_key", None),
    ("tools/list: the shared agent key", {"method": "tools/list", "params": {}},
     AGENT_SHARED, {"X-Agent-Org-Override": "self-host"}),
    # PYTHON_BUGS #13: a scoped agent key is not recognised by the scope
    # lookup, so it sees the WHOLE catalog including the config-write
    # tool the agent allowlist exists to exclude. Asserting the current
    # behaviour on purpose — it fails loudly when master is fixed.
    ("tools/list: a scoped agent key sees everything (PYTHON_BUGS #13)",
     {"method": "tools/list", "params": {}}, AGENT_SCOPED,
     {"X-Agent-Org-Override": "self-host"}),

    # ---- the scope gate, per mode -------------------------------------
    #
    # Two things per key: what the CATALOG offers, and what a call it was
    # not given is answered with. The catalog is the half a client sees;
    # the call is the half that matters, because a client is free to
    # invoke a name it was never offered.
    *[(f"tools/list: {label}", {"method": "tools/list", "params": {}}, key, None)
      for label, key in [
          ("a readonly key", READONLY_KEY),
          ("a custom key", CUSTOM_KEY),
          ("a key whose custom scope is unparseable", BAD_SCOPE_KEY),
          ("a key whose custom scope is empty", EMPTY_SCOPE_KEY),
          # A revoked key is not recognised by the scope lookup either,
          # so it sees the UNFILTERED catalog and is refused only when
          # it calls something — the same shape as an unknown key, and
          # the reason the lookup never raises.
          ("a revoked key with a real hash", REVOKED_LIVE_KEY),
      ]],
    *[(f"call: {tool} with {label}", {"method": "tools/call",
                                      "params": {"name": tool, "arguments": args}}, key, None)
      for label, key, tool, args in [
          # readonly: a read is allowed, every write is refused — and the
          # refusal must be the SCOPE message, not the tool's own error.
          ("a readonly key", READONLY_KEY, "list_cameras", {}),
          ("a readonly key", READONLY_KEY, "create_incident",
           {"title": "T", "summary": "S"}),
          ("a readonly key", READONLY_KEY, "set_camera_recording_policy",
           {"camera_id": "cam-live", "continuous_24_7": False}),
          # custom: one named read, one named WRITE (so the mode is not
          # merely readonly by another spelling), and one not named.
          ("a custom key", CUSTOM_KEY, "list_cameras", {}),
          ("a custom key", CUSTOM_KEY, "create_incident", {"title": "T", "summary": "S"}),
          ("a custom key", CUSTOM_KEY, "get_camera", {"camera_id": "cam-live"}),
          # The unknown name in that key's scope list is dropped rather
          # than enabling anything — checked through the catalog above.
          #
          # An unparseable or empty custom scope is NO access, not full
          # access: the inverted default is the dangerous one here, so
          # the case that proves it is a plain read being refused.
          ("an unparseable custom scope", BAD_SCOPE_KEY, "list_cameras", {}),
          ("an empty custom scope", EMPTY_SCOPE_KEY, "list_cameras", {}),
          # A tool NOT in the custom key's list, and one it would only
          # reach if an unknown name in that list leaked a real tool.
          # The key's stored scope names `not_a_tool`; this is the tool
          # that must stay out of reach regardless.
          ("a custom key", CUSTOM_KEY, "set_camera_recording_policy",
           {"camera_id": "cam-live", "continuous_24_7": False}),
          # Revocation is the only way to withdraw a leaked key, so the
          # refusal has to come from the key's own row and not from the
          # hash failing to match anything.
          ("a revoked key with a real hash", REVOKED_LIVE_KEY, "list_cameras", {}),
      ]],

    # ---- auth refusals -----------------------------------------------
    *[(f"call: {label}", {"method": "tools/call",
                          "params": {"name": "list_cameras", "arguments": {}}}, key, extra)
      for label, key, extra in [
          ("no credential", None, None),
          ("an empty bearer", "", None),
          ("an unknown key", "osc_nope", None),
          ("a revoked key", "osc_revoked_key", None),
          ("an integration key", "osi_live_integration_key", None),
          ("the shared agent key with no org header", AGENT_SHARED, None),
          ("the shared agent key for an unknown org", AGENT_SHARED,
           {"X-Agent-Org-Override": "nobody"}),
          ("a scoped agent key for another org", AGENT_SCOPED,
           {"X-Agent-Org-Override": "other-org"}),
      ]],

    # ---- reads -------------------------------------------------------
    *[(f"call: {name}", {"method": "tools/call",
                         "params": {"name": name, "arguments": args}}, LIVE_KEY, None)
      for name, args in [
          ("list_cameras", {}),
          ("list_camera_groups", {}),
          ("list_nodes", {}),
          ("get_system_status", {}),
          ("get_camera", {"camera_id": "cam-live"}),
          ("get_camera", {"camera_id": "nope"}),
          ("get_camera", {"camera_id": "cam-theirs"}),
          ("get_stream_url", {"camera_id": "cam-live"}),
          ("get_stream_url", {"camera_id": "nope"}),
          ("get_node", {"node_id": "node-aaaa1111"}),
          ("get_node", {"node_id": "nope"}),
          ("get_node", {"node_id": "node-cccc3333"}),
          ("get_camera_recording_policy", {"camera_id": "cam-live"}),
          ("get_camera_recording_policy", {"camera_id": "nope"}),
          ("get_stream_logs", {}),
          ("get_stream_logs", {"camera_id": "cam-live"}),
          ("get_stream_logs", {"camera_id": "cam-live", "limit": 2}),
          # `if camera_id:` here, so an empty filter is NO filter and
          # every row comes back. `list_incidents` below tests
          # `is not None` on the same argument name and gets the
          # opposite answer — the pair is the point.
          ("get_stream_logs", {"camera_id": ""}),
          ("get_stream_stats", {}),
          ("get_stream_stats", {"days": 1}),
          ("get_stream_stats", {"days": 30}),
          ("list_incidents", {}),
          ("list_incidents", {"status": "open"}),
          ("list_incidents", {"status": "nonsense"}),
          ("list_incidents", {"severity": "critical"}),
          ("list_incidents", {"severity": "nonsense"}),
          ("list_incidents", {"camera_id": "cam-live"}),
          # `is not None`: an empty camera_id IS a filter, and it
          # matches nothing.
          ("list_incidents", {"camera_id": ""}),
          ("list_incidents", {"limit": 2, "offset": 1}),
          ("get_incident", {"incident_id": 1}),
          ("get_incident", {"incident_id": 9999}),
          # Evidence reads against the fixture's blob rows. These are
          # the only two tools with a happy path that needs no node:
          # the bytes are already in the database.
          #
          # ids 5-15 on incident 1 cover the MIME parsing; the
          # interesting ones here are the branches the REST blob route
          # does NOT share, because this tool reads `kind` and the
          # length separately.
          ("get_incident_clip", {"incident_id": 1, "evidence_id": 9999}),
          ("get_incident_clip", {"incident_id": 1, "evidence_id": 5}),
          # a clip with no duration parameter, and one with an
          # unparseable one: `float()` raises and the parameter is
          # skipped, leaving the duration null rather than defaulted
          ("get_incident_clip", {"incident_id": 1, "evidence_id": 6}),
          ("get_incident_clip", {"incident_id": 1, "evidence_id": 7}),
          # two duration parameters: the loop does not break, last wins
          ("get_incident_clip", {"incident_id": 1, "evidence_id": 13}),
          # underscore and whitespace, which Python's float() accepts
          ("get_incident_clip", {"incident_id": 1, "evidence_id": 15}),
          # a zero-length blob: `length()` is 0, not NULL, so this is
          # NOT the "no video data" refusal — it is a clip of nothing
          ("get_incident_clip", {"incident_id": 1, "evidence_id": 9}),
          # a clip row whose blob was never written: length() IS null
          ("get_incident_clip", {"incident_id": 1, "evidence_id": 14}),
          # data with NO mime: has_data reads FALSE off the missing MIME
          # while the bytes are there, and only `mime` takes the
          # fallback — `data_mime` stays null
          ("get_incident_clip", {"incident_id": 1, "evidence_id": 16}),
          # an observation, not a clip
          ("get_incident_clip", {"incident_id": 1, "evidence_id": 1}),
          # evidence that exists, on an incident that is not this org's
          ("get_incident_clip", {"incident_id": 4, "evidence_id": 4}),
          # the evidence of ANOTHER incident, by id
          ("get_incident_clip", {"incident_id": 2, "evidence_id": 5}),
          ("get_incident_clip", {"incident_id": 9999, "evidence_id": 5}),
          # a PNG snapshot, returned as an image content block
          ("get_incident_snapshot", {"incident_id": 1, "evidence_id": 2}),
          # data with no MIME at all, which falls back to jpeg — the
          # bytes are a PNG either way, so the FORMAT is the answer
          ("get_incident_snapshot", {"incident_id": 1, "evidence_id": 10}),
          # a text/* MIME, which is not one of the three recognised
          # image types and takes the same jpeg fallback
          ("get_incident_snapshot", {"incident_id": 1, "evidence_id": 11}),
          # a clip, not a snapshot
          ("get_incident_snapshot", {"incident_id": 1, "evidence_id": 5}),
          # an observation: right incident, no data
          ("get_incident_snapshot", {"incident_id": 1, "evidence_id": 1}),
          ("get_incident_snapshot", {"incident_id": 4, "evidence_id": 4}),
          ("get_incident_snapshot", {"incident_id": 9999, "evidence_id": 2}),
      ]],

    # ---- writes ------------------------------------------------------
    *[(f"call: {name} {label}", {"method": "tools/call",
                                 "params": {"name": name, "arguments": args}}, LIVE_KEY, None)
      for name, label, args in [
          ("create_incident", "minimal", {"title": "T", "summary": "S"}),
          ("create_incident", "with a camera",
           {"title": "T", "summary": "S", "camera_id": "cam-live"}),
          ("create_incident", "with an unknown camera",
           {"title": "T", "summary": "S", "camera_id": "nope"}),
          # `if camera_id:` skips the existence check, and then the
          # column is assigned the argument anyway — so this stores the
          # empty string rather than NULL, on the row AND on the
          # notification.
          ("create_incident", "with an empty camera_id",
           {"title": "T", "summary": "S", "camera_id": ""}),
          ("create_incident", "high severity",
           {"title": "T", "summary": "S", "severity": "high"}),
          ("create_incident", "a bad severity",
           {"title": "T", "summary": "S", "severity": "nonsense"}),
          ("create_incident", "a blank title", {"title": "   ", "summary": "S"}),
          ("create_incident", "a blank summary", {"title": "T", "summary": " "}),
          ("create_incident", "an over-long summary",
           {"title": "T", "summary": "x" * 2001}),
          ("create_incident", "a title that needs truncating",
           {"title": "x" * 250, "summary": "S"}),
          # `title.strip()[:200]` counts CHARACTERS. Every title case
          # above is ASCII, where bytes and characters agree — so a port
          # that sliced bytes scored identical on all of them while
          # cutting a multi-byte character in half here, and overflowing
          # a column that is 200 characters wide.
          ("create_incident", "a multi-byte title that needs truncating",
           {"title": "é" * 250, "summary": "S"}),
          ("create_incident", "an astral title that needs truncating",
           {"title": "🎥" * 250, "summary": "S"}),
          ("add_observation", "ok", {"incident_id": 1, "text": "saw a thing"}),
          ("add_observation", "with a camera",
           {"incident_id": 1, "text": "t", "camera_id": "cam-live"}),
          ("add_observation", "with a foreign camera",
           {"incident_id": 1, "text": "t", "camera_id": "cam-theirs"}),
          # `is not None` here, so the empty string is looked up and
          # refused — the opposite of `create_incident` above.
          ("add_observation", "with an empty camera_id",
           {"incident_id": 1, "text": "t", "camera_id": ""}),
          ("add_observation", "blank", {"incident_id": 1, "text": "  "}),
          ("add_observation", "too long", {"incident_id": 1, "text": "x" * 8001}),
          ("add_observation", "on a missing incident", {"incident_id": 9999, "text": "t"}),
          ("update_incident", "status", {"incident_id": 1, "status": "acknowledged"}),
          ("update_incident", "resolved stamps who", {"incident_id": 1, "status": "resolved"}),
          ("update_incident", "reopened clears it", {"incident_id": 2, "status": "open"}),
          ("update_incident", "a bad status", {"incident_id": 1, "status": "nonsense"}),
          ("update_incident", "severity", {"incident_id": 1, "severity": "low"}),
          ("update_incident", "a bad severity", {"incident_id": 1, "severity": "nonsense"}),
          ("update_incident", "summary", {"incident_id": 1, "summary": "new"}),
          ("update_incident", "a blank summary", {"incident_id": 1, "summary": "   "}),
          ("update_incident", "a report", {"incident_id": 1, "report": "# Body"}),
          ("update_incident", "a blank report", {"incident_id": 1, "report": "  "}),
          ("update_incident", "nothing at all", {"incident_id": 1}),
          ("update_incident", "on a missing incident", {"incident_id": 9999, "status": "open"}),
          ("finalize_incident", "on one with no report",
           {"incident_id": 2, "report": "# Body"}),
          ("finalize_incident", "on one that has one",
           {"incident_id": 1, "report": "# Body"}),
          ("finalize_incident", "blank", {"incident_id": 2, "report": "   "}),
          ("finalize_incident", "too long", {"incident_id": 2, "report": "x" * 64001}),
          ("set_camera_recording_policy", "continuous off",
           {"camera_id": "cam-live", "continuous_24_7": False}),
          ("set_camera_recording_policy", "a schedule",
           {"camera_id": "cam-stale", "scheduled_recording": True,
            "scheduled_start": "22:00", "scheduled_end": "06:00"}),
          ("set_camera_recording_policy", "a bad time",
           {"camera_id": "cam-stale", "scheduled_start": "25:00"}),
          ("set_camera_recording_policy", "clearing a window",
           {"camera_id": "cam-stale", "scheduled_start": ""}),
          ("set_camera_recording_policy", "an unknown camera",
           {"camera_id": "nope", "continuous_24_7": True}),
          # cam-live is continuous in the fixture, so turning the
          # SCHEDULE on without turning continuous off is the
          # mutual-exclusion refusal — which is a RESULT, not an error,
          # and the only answer on this surface shaped that way. No case
          # reached it until this one: the others each set a mode whose
          # counterpart was already off.
          ("set_camera_recording_policy", "both modes at once",
           {"camera_id": "cam-live", "scheduled_recording": True}),
          # And the same conflict reached from the other side:
          # cam-failed is the fixture's scheduled camera (22:00), so
          # turning continuous on without clearing the schedule
          # conflicts too.
          ("set_camera_recording_policy", "both modes from the other side",
           {"camera_id": "cam-failed", "continuous_24_7": True}),
          # The node is offline in the fixture, so these are refusals —
          # and the refusal text is the thing worth comparing.
          ("attach_snapshot", "with an offline node",
           {"incident_id": 1, "camera_id": "cam-live"}),
          ("attach_clip", "with an empty buffer",
           {"incident_id": 1, "camera_id": "cam-live"}),
          ("view_camera", "with an offline node", {"camera_id": "cam-live"}),
          ("watch_camera", "with an offline node", {"camera_id": "cam-live", "count": 2}),
          ("get_incident_snapshot", "missing evidence",
           {"incident_id": 1, "evidence_id": 9999}),
      ]],

    # ---- scope -------------------------------------------------------
    # The shared agent key IS recognised by the scope lookup, so the
    # config-write tool is refused for it — the contrast with the scoped
    # key above is the whole of PYTHON_BUGS #13.
    ("call: the shared agent key cannot set a recording policy",
     {"method": "tools/call", "params": {
         "name": "set_camera_recording_policy",
         "arguments": {"camera_id": "cam-live", "continuous_24_7": False}}},
     AGENT_SHARED, {"X-Agent-Org-Override": "self-host"}),

    # ---- protocol ----------------------------------------------------
    ("an unknown method", {"method": "resources/list", "params": {}}, LIVE_KEY, None),
    ("an unknown tool", {"method": "tools/call",
                         "params": {"name": "rm_minus_rf", "arguments": {}}}, LIVE_KEY, None),
    ("a tool with no arguments key", {"method": "tools/call",
                                      "params": {"name": "list_cameras"}}, LIVE_KEY, None),
]


# The node key the fixture's `node-aaaa1111` row hashes, and a JPEG
# small enough to inline. Shared with ws_diff.py, which is where the
# same bytes and the same node first appeared.
NODE_KEY = "test-node-key"
NODE_ID = "node-aaaa1111"
TINY_JPEG_B64 = (
    "/9j/4AAQSkZJRgABAQEAYABgAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0a"
    "HBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/wAALCAABAAEBAREA/8QAFAABAAAAAAAA"
    "AAAAAAAAAAAACf/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AKp//2Q=="
)

# Every tool that captures from a camera. None of them can answer on
# its own: the tool sends a `command` frame down the node's socket and
# awaits the `command_result`. The static cases above reach these tools
# only through the fixture's OFFLINE node, so every one of them is a
# refusal — which compares the error text and nothing else. These drive
# the other half.
#
# `reply` is a LIST, one per command the call is expected to issue, so
# `watch_camera` can be given a different answer per frame.
NODE_CASES: list[tuple] = [
    ("view_camera: a captured frame", "view_camera", {"camera_id": "cam-live"},
     [{"status": "success", "data": {"image_b64": TINY_JPEG_B64}}]),
    # The flat shape an older CameraNode sends.
    ("view_camera: no envelope", "view_camera", {"camera_id": "cam-live"},
     [{"image_b64": TINY_JPEG_B64}]),
    ("view_camera: a dead pipeline", "view_camera", {"camera_id": "cam-live"},
     [{"status": "error", "error": "no segments available yet"}]),
    ("view_camera: unparseable base64", "view_camera", {"camera_id": "cam-live"},
     [{"status": "success", "data": {"image_b64": "not base64!!"}}]),

    # Two frames, one second apart. The second answer differs from the
    # first so a stack that reuses one reply for both is visible.
    ("watch_camera: two frames", "watch_camera",
     {"camera_id": "cam-live", "count": 2, "interval_seconds": 1},
     [{"status": "success", "data": {"image_b64": TINY_JPEG_B64}},
      {"status": "success", "data": {"image_b64": TINY_JPEG_B64}}]),
    # One frame fails: the text placeholder rides alongside the image,
    # and the call still succeeds because SOME frame came back.
    ("watch_camera: one frame fails", "watch_camera",
     {"camera_id": "cam-live", "count": 2, "interval_seconds": 1},
     [{"status": "success", "data": {"image_b64": TINY_JPEG_B64}},
      {"status": "error", "error": "camera busy"}]),
    # NO frame comes back: that is the one case that refuses outright.
    ("watch_camera: every frame fails", "watch_camera",
     {"camera_id": "cam-live", "count": 2, "interval_seconds": 1},
     [{"status": "error", "error": "camera busy"},
      {"status": "error", "error": "camera busy"}]),

    ("attach_snapshot: ok", "attach_snapshot",
     {"incident_id": 1, "camera_id": "cam-live"},
     [{"status": "success", "data": {"image_b64": TINY_JPEG_B64}}]),
    ("attach_snapshot: with a note", "attach_snapshot",
     {"incident_id": 1, "camera_id": "cam-live", "note": "  front door  "},
     [{"status": "success", "data": {"image_b64": TINY_JPEG_B64}}]),
    # `note.strip() if note else None`: the empty string is falsy and
    # stores NULL, while a whitespace-only note stores "". Both read as
    # "no caption" and they are different rows.
    ("attach_snapshot: an empty note", "attach_snapshot",
     {"incident_id": 1, "camera_id": "cam-live", "note": ""},
     [{"status": "success", "data": {"image_b64": TINY_JPEG_B64}}]),
    ("attach_snapshot: a whitespace note", "attach_snapshot",
     {"incident_id": 1, "camera_id": "cam-live", "note": "   "},
     [{"status": "success", "data": {"image_b64": TINY_JPEG_B64}}]),
    ("attach_snapshot: the node fails", "attach_snapshot",
     {"incident_id": 1, "camera_id": "cam-live"},
     [{"status": "error", "error": "ffmpeg exited with status 1"}]),
    # Ownership is checked BEFORE the node is woken, so no command is
    # issued at all — `None` asserts that nothing arrives.
    ("attach_snapshot: a foreign incident", "attach_snapshot",
     {"incident_id": 4, "camera_id": "cam-live"}, None),
]

# Clip capture reads the in-memory segment cache rather than the node,
# so it needs segments pushed into the tier under test — and because
# each tier has its OWN cache, they have to be pushed to each.
CLIP_CASES: list[tuple] = [
    # Three one-second segments in the buffer, four asked for: the
    # answer is what is there, not an error.
    ("attach_clip: the whole buffer", {"incident_id": 1, "camera_id": "cam-live",
                                       "duration_seconds": 4}, 3),
    # Fewer than are buffered: the NEWEST are the ones kept.
    ("attach_clip: a shorter window", {"incident_id": 1, "camera_id": "cam-live",
                                       "duration_seconds": 2}, 3),
    ("attach_clip: with a note", {"incident_id": 1, "camera_id": "cam-live",
                                  "duration_seconds": 2, "note": " tail "}, 3),
    ("attach_clip: an empty note", {"incident_id": 1, "camera_id": "cam-live",
                                    "duration_seconds": 2, "note": ""}, 3),
    ("attach_clip: a foreign incident", {"incident_id": 4, "camera_id": "cam-live",
                                         "duration_seconds": 2}, 3),
]


def push_segments(port: int, count: int) -> None:
    """Fill a tier's in-memory segment cache for `cam-live`.

    Per tier, because the cache is per process — this is the one place
    the differential has to write the same bytes twice rather than
    reseeding one database. The payload is a plausible MPEG-TS sync
    byte followed by filler, distinct per segment so a stack that
    concatenates them in the wrong order is visible in the length.

    Called before every clip case rather than once per pass; see the
    note at the call site for why the age of these matters.
    """
    for i in range(count):
        body = bytes([0x47, 0x40, 0x00, 0x11]) + bytes([i]) * (256 * (i + 1))
        request = urllib.request.Request(
            f"http://127.0.0.1:{port}/api/cameras/cam-live/push-segment"
            f"?filename=segment_{i:05d}.ts",
            data=body,
            headers={"X-Node-API-Key": NODE_KEY,
                     "Content-Type": "application/octet-stream"},
        )
        try:
            urllib.request.urlopen(request, timeout=10).read()
        except urllib.error.HTTPError as err:
            err.read()


async def node_roundtrip(port: int, cases: list[tuple]) -> dict:
    """Run every node-backed case against one tier, down ONE socket.

    The connect throttle allows ten handshakes per node per minute, so a
    socket per case would spend the budget and turn the rest of the run
    into identical refusals that agree about nothing. One socket, and
    the tool calls ride over it in sequence.

    A tool call blocks on the socket, so the HTTP request goes to an
    executor and the reply is sent from here while it waits.
    """
    out: dict = {}
    url = f"ws://127.0.0.1:{port}/ws/node"
    headers = {"X-Node-API-Key": NODE_KEY, "X-Node-Id": NODE_ID}
    loop = asyncio.get_running_loop()

    def call(name, args):
        return rpc(port, {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": name, "arguments": args}}, LIVE_KEY)

    async with websockets.connect(url, additional_headers=headers, open_timeout=5) as ws:
        for label, tool, args, replies in cases:
            if tool == "attach_clip":
                # Refilled before EVERY clip case, not once per pass.
                # The segment cache drops a camera whose newest segment
                # is more than 60 seconds old, swept on a 60-second
                # loop — and the node cases ahead of these take longer
                # than that between them (two of them sleep a second
                # per frame). Pushed once at the top, whichever tier's
                # sweep happened to fire first reported "the stream
                # must be live" while the other returned a clip. That
                # is a race in the harness and not a difference between
                # the stacks: both carry the same 60-second cutoff.
                #
                # Re-pushing the same three filenames replaces the same
                # keys, so it refreshes their age without changing what
                # is in the buffer — which is what makes "the newest
                # are the ones kept" a meaningful assertion.
                push_segments(port, 3)
            task = loop.run_in_executor(None, call, tool, args)
            commands = []
            for reply in (replies or [None]):
                try:
                    frame = json.loads(await asyncio.wait_for(ws.recv(), 20))
                except (TimeoutError, asyncio.TimeoutError):
                    commands.append("<no command>")
                    break
                if isinstance(frame, dict) and frame.get("type") == "command":
                    # The correlation id is generated per command, so it
                    # is echoed back rather than compared.
                    commands.append({k: v for k, v in frame.items() if k != "id"})
                    if reply is not None:
                        await ws.send(json.dumps({
                            "type": "command_result",
                            "id": frame.get("id"),
                            "payload": reply,
                        }))
                else:
                    commands.append(frame)
            status, payload = await task
            out[label] = {"commands": commands,
                          "status": status,
                          "response": normalise(payload, tool)}
    return out


async def run_node_cases(results: list[tuple]) -> None:
    """Both tiers, each from a freshly reseeded database.

    Reseeded per TIER rather than per case: `attach_snapshot` writes an
    evidence row, so the ids climb through the sequence, and the two
    tiers only agree about those ids if each starts its whole pass from
    the same state.
    """
    answers = {}
    for name, port in PORTS.items():
        reseed()
        try:
            answers[name] = await node_roundtrip(port, NODE_CASES + [
                (label, "attach_clip", args, None) for label, args, _ in CLIP_CASES
            ])
        except Exception as exc:  # noqa: BLE001
            answers[name] = {"failed": f"{type(exc).__name__}: {exc}"}

    for label, *_ in NODE_CASES + [(c[0],) for c in CLIP_CASES]:
        compare(label, {n: answers[n].get(label, answers[n]) for n in PORTS}, results)


def compare(label: str, sides: dict, results: list[tuple]) -> None:
    same = sides["python"] == sides["rust"]
    results.append((label, same))
    if same:
        if VERBOSE:
            print(f"  ok      {label}")
        return
    print(f"  DIFFER  {label}")
    for name in ("python", "rust"):
        print(f"            {name:6} {json.dumps(sides[name], sort_keys=True)[:320]}")


def main() -> int:
    wanted = [w for w in DIFF_ONLY.split("|") if w]
    cases = [c for c in CASES if not wanted or any(w in c[0] for w in wanted)]
    results: list[tuple[str, bool]] = []
    # How many cases came back as a SUCCESSFUL tool call. A run in which
    # everything refused would agree perfectly and prove only that both
    # stacks refuse, which is the failure mode this file is most likely
    # to drift into: nearly every tool needs either a live node or a
    # blob, and a fixture that stops providing one turns its cases into
    # matching errors rather than into failures.
    succeeded = 0
    for label, body, key, extra in cases:
        request = {"jsonrpc": "2.0", "id": 1, **body}
        answers = {}
        tool = (body.get("params") or {}).get("name")
        for name, port in PORTS.items():
            reseed()
            status, payload = rpc(port, request, key, extra)
            answers[name] = (status, normalise(payload, tool))
        result = answers["python"][1]
        if isinstance(result, dict) and isinstance(result.get("result"), dict):
            succeeded += not result["result"].get("isError", False)
        compare(label, {n: answers[n] for n in PORTS}, results)

    # The node-backed tools, which no request/response case can reach.
    if not wanted or any(w in "node commands" for w in wanted):
        print("node-backed tools")
        asyncio.run(run_node_cases(results))
        succeeded += sum(1 for label, ok in results if ok and "fails" not in label)

    reseed()
    bad = sum(1 for _, ok in results if not ok)
    print()
    if not wanted and succeeded < 60:
        print(f"REFUSING: only {succeeded} case(s) were a successful tool call — "
              "the fixture has stopped reaching the happy paths")
        bad += 1
    print(f"{len(results) - bad}/{len(results)} identical, {bad} differing"
          + (f" [DIFF_ONLY={DIFF_ONLY!r}]" if wanted else ""))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
