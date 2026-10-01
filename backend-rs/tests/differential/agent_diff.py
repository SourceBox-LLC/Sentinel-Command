#!/usr/bin/env python3
"""Differential for the Sentinel agent: Python vs Rust, same model, same server.

The agent has no response to diff — it is a client. What it *does* is the
behaviour: what it tells the model, which tools it calls, what it writes
back to Command Center. So each scenario is run twice, once through each
agent, against

  * one real Command Center (the Rust tier), reseeded before each run;
  * one scripted model (`fake_llm.py`), given the same turns both times;

and four things are compared:

  1. **what the model was told** — every completion request, reduced to
     a provider-neutral event list (system / user / image / assistant
     text / tool call / tool result). Reduced, because LiteLLM and rig are
     different libraries and will not serialise identically; what must
     match is the conversation, not its JSON. The tool list offered and
     the token cap are compared too.
  2. **the run row** Command Center ends up with: outcome, severity,
     incident id, tool-call count, summary, and the whole tool trace.
  3. **what was filed**: incidents and their evidence.
  4. **the wakeup response**, which is the drain summary.

Two strings are compared by prefix only, and both are named here so the
relaxation is visible: `LLM call failed: …` and `Agent harness failure:
…` end in the underlying library's error text, which is httpx's on one
side and reqwest's on the other.

Usage: agent_diff.py --wire ollama|openai|anthropic [-v] [--only NAME]
Env:   CC_URL, FAKE_LLM_URL, RUST_AGENT_URL, PYTHON_AGENT_URL, AGENT_KEY,
       PG_CONTAINER
"""
from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import os
import pathlib
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
CC = os.environ.get("CC_URL", "http://127.0.0.1:8052")
FAKE = os.environ.get("FAKE_LLM_URL", "http://127.0.0.1:18096")
AGENTS = {
    "python": os.environ.get("PYTHON_AGENT_URL", "http://127.0.0.1:8051"),
    "rust": os.environ.get("RUST_AGENT_URL", "http://127.0.0.1:8050"),
}
AGENT_KEY = os.environ.get("AGENT_KEY", "harness-agent-queue-key")
PG = os.environ.get("PG_CONTAINER", "cc-schema-test")
MAX_ITERATIONS = int(os.environ.get("MAX_AGENT_ITERATIONS", "10"))

NEW = "$NEW_INCIDENT"   # replaced with the id the next create_incident will get


def call(name, **arguments):
    return {"name": name, "arguments": arguments}


def tools(*calls):
    return {"tool_calls": list(calls)}


def text(s):
    return {"text": s}


def run(suffix, trigger="motion", camera="cam-live", org="self-host", prompt=None):
    return {"id": f"agt{suffix:0>29}", "trigger": trigger, "camera": camera, "org": org, "prompt": prompt}


# name, wires it runs on, pending runs, model turns
ALL = ("ollama", "openai", "anthropic")
OPENAI_5XX = "a provider failure on the OpenAI wire is not retried"
SCENARIOS = [
    ("an answer with no tool call is no_action", ALL,
     [run(1)], [text("A cat crossed the driveway. Nothing to log.")]),

    ("an empty answer still gets a summary", ("ollama", "openai"),
     [run(1)], [text("")]),

    ("each trigger gets its own brief", ALL,
     [run(1, "motion"), run(2, "incident_opened", None), run(3, "scheduled", None),
      run(4, "manual", None, prompt="Check the driveway camera and tell me what you see."),
      run(5, "something_new", None)],
     [text("one"), text("two"), text("three"), text("four"), text("five")]),

    ("read tools, then an answer", ALL,
     [run(1)],
     [tools(call("list_cameras")), tools(call("get_camera", camera_id="cam-live")),
      text("Camera is online; nothing unusual.")]),

    ("two tool calls in one turn", ALL,
     [run(1, "scheduled", None)],
     [tools(call("list_cameras"), call("list_nodes")), text("Sweep complete, all clear.")]),

    ("file, observe, finalize: an incident with its id", ALL,
     [run(1)],
     [tools(call("create_incident", title="Unfamiliar vehicle", summary="Dark sedan on the driveway",
                 severity="medium", camera_id="cam-live")),
      tools(call("add_observation", incident_id=NEW, text="No occupants visible for ~3 minutes.")),
      tools(call("finalize_incident", incident_id=NEW, report="## Report\n\nA dark sedan lingered.")),
      text("Filed and finalized.")]),

    ("a later update_incident moves the severity", ALL,
     [run(1)],
     [tools(call("create_incident", title="Person at door", summary="Unknown pedestrian", severity="low")),
      tools(call("update_incident", incident_id=NEW, severity="high")),
      text("Escalated to high.")]),

    ("an invalid severity is not recorded", ("ollama",),
     [run(1)],
     [tools(call("create_incident", title="t", summary="s", severity="catastrophic")),
      text("Tried to file.")]),

    ("an incident named only in prose is not an incident", ALL,
     [run(1)],
     [tools(call("list_incidents")), text("I reviewed incident #1 and incident_id 2; nothing new to file.")]),

    ("an unknown tool is an error the model can read", ALL,
     [run(1)],
     [tools(call("open_the_pod_bay_doors", please=True)), text("That tool does not exist.")]),

    ("a tool the agent is denied", ("ollama",),
     [run(1)],
     [tools(call("set_camera_recording_policy", camera_id="cam-live", mode="off")), text("Denied, as expected.")]),

    ("a failing tool call is fed back, not fatal", ALL,
     [run(1)],
     [tools(call("get_camera", camera_id="no-such-camera")),
      tools(call("get_incident", incident_id=999999)), text("Neither exists.")]),

    ("frames arrive as images and are pruned later", ALL,
     [run(1, "incident_opened", None)],
     [tools(call("get_incident_snapshot", incident_id=1, evidence_id=2)),
      tools(call("get_incident_snapshot", incident_id=1, evidence_id=2),
            call("get_incident", incident_id=1)),
      tools(call("list_cameras")),
      text("Reviewed the attached snapshot twice.")]),

    ("the iteration budget is a hard stop", ("ollama",),
     [run(1)],
     [tools(call("list_cameras"))] * (MAX_ITERATIONS + 3)),

    ("budget exhausted after filing still reports the incident", ("ollama",),
     [run(1)],
     [tools(call("create_incident", title="Late filing", summary="s", severity="critical"))]
     + [tools(call("list_cameras"))] * (MAX_ITERATIONS + 3)),

    ("a provider failure before any filing is an error", ("ollama", "anthropic"),
     [run(1)], [{"status": 500}]),

    # Scripted to fail for as long as anyone asks, so both agents end in
    # the same place and only the number of attempts can differ. See
    # EXPECTED_DIVERGENCES.
    (OPENAI_5XX, ("openai",),
     [run(1)], [{"status": 500}] * 6),

    ("a provider failure after filing keeps the incident", ("ollama", "anthropic"),
     [run(1)],
     [tools(call("create_incident", title="Filed first", summary="s", severity="high")), {"status": 503}]),

    ("malformed tool arguments degrade to none", ("openai",),
     [run(1)],
     [{"tool_calls": [{"name": "get_camera", "raw_arguments": "{not json"}]}, text("Arguments were malformed.")]),

    ("arguments that are not an object degrade to none", ("openai",),
     [run(1)],
     [{"tool_calls": [{"name": "list_cameras", "raw_arguments": "[1, 2, 3]"}]}, text("ok")]),

    ("long arguments are cut in the trace", ("ollama", "openai"),
     [run(1)],
     [tools(call("create_incident", title="T" * 150, summary="é" * 260, severity="low")),
      tools(call("add_observation", incident_id=NEW, text="x" * 1200)),
      text("Filed a long one.")]),

    ("several runs drain in order, each under its own org", ("ollama",),
     [run(1, org="self-host"), run(2, "scheduled", None, org="other-org"), run(3, org="self-host")],
     [tools(call("get_system_status")), text("first"),
      tools(call("get_system_status")), text("second"),
      text("third")]),

    ("a run with no org is skipped", ("ollama",),
     [run(1, org="")], [text("should never be asked")]),
]


# ── plumbing ────────────────────────────────────────────────────────────

def psql(sql: str) -> str:
    out = subprocess.run(["docker", "exec", "-i", PG, "psql", "-U", "cc", "-d", "cc", "-tA",
                          "-v", "ON_ERROR_STOP=1", "-c", sql],
                         capture_output=True, text=True, timeout=60)
    if out.returncode != 0:
        raise RuntimeError(f"psql failed: {out.stderr.strip()}")
    return out.stdout.strip()


def seed(runs: list[dict]) -> int:
    with open(HERE / "seed_cameras.sql", "rb") as fixture:
        out = subprocess.run(["docker", "exec", "-i", PG, "psql", "-U", "cc", "-d", "cc", "-q",
                              "-v", "ON_ERROR_STOP=1"], stdin=fixture, capture_output=True, timeout=120)
    if out.returncode != 0:
        raise RuntimeError(f"seed failed: {out.stderr.decode()}")
    # The fixture's own pending runs would be drained too, and they are
    # not this scenario's — park them in a terminal state.
    psql("UPDATE sentinel_runs SET outcome = 'no_action' WHERE outcome IN ('pending', 'running')")
    # Sentinel must be ENABLED for the orgs in play. The fixture seeds only
    # another org's config, switched off — so the first full run of this
    # harness scored 20/20 identical with every single tool call refused
    # ("Sentinel disabled for this org"), two agents in perfect agreement
    # about having been allowed to do nothing. The coverage guard at the
    # bottom of this file is what noticed.
    psql("DELETE FROM sentinel_config WHERE org_id IN ('self-host', 'other-org')")
    psql(
        "INSERT INTO sentinel_config (org_id, enabled, motion_enabled, incident_opened_enabled, "
        "motion_cooldown_min, schedule_mode, schedule_start, schedule_end, created_at, updated_at) "
        "SELECT org, true, true, true, 5, 'always', '22:00', '06:00', now(), now() "
        "FROM (VALUES ('self-host'), ('other-org')) AS t(org)"
    )
    for index, r in enumerate(runs):
        camera = "NULL" if r["camera"] is None else f"'{r['camera']}'"
        prompt = "NULL" if r["prompt"] is None else "'" + r["prompt"].replace("'", "''") + "'"
        psql(
            "INSERT INTO sentinel_runs (id, org_id, triggered_at, trigger_type, camera_id, "
            "tool_call_count, outcome, manual_prompt, summary, updated_at) VALUES "
            f"('{r['id']}', '{r['org']}', timestamp '2026-09-15 10:00:0{index}', '{r['trigger']}', "
            f"{camera}, 0, 'pending', {prompt}, '', timestamp '2026-09-15 10:00:0{index}')"
        )
    return int(psql("SELECT coalesce(max(id), 0) + 1 FROM incidents"))


def http(method: str, url: str, body=None, headers=None, timeout=200):
    data = json.dumps(body).encode() if body is not None and not isinstance(body, bytes) else body
    req = urllib.request.Request(url, data=data, method=method, headers=headers or {})
    if data is not None and "Content-Type" not in (headers or {}):
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()


def wake(agent_url: str):
    body = json.dumps({"ts": time.time()}).encode()
    signature = "sha256=" + hmac.new(AGENT_KEY.encode(), body, hashlib.sha256).hexdigest()
    status, raw = http("POST", agent_url + "/wakeup", body, {"X-Sentinel-Signature": signature,
                                                            "Content-Type": "application/json"})
    try:
        return status, json.loads(raw)
    except ValueError:
        return status, raw.decode("utf-8", "replace")


def substitute(value, new_id: int):
    if value == NEW:
        return new_id
    if isinstance(value, dict):
        return {k: substitute(v, new_id) for k, v in value.items()}
    if isinstance(value, list):
        return [substitute(v, new_id) for v in value]
    return value


# ── reducing a completion request to what the model was told ─────────────

def _args(raw):
    if isinstance(raw, str):
        try:
            return json.loads(raw)
        except ValueError:
            return {"__unparsed__": raw}
    return raw if raw is not None else {}


def _text_of(content) -> str:
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    return "".join(p.get("text", "") for p in content if isinstance(p, dict) and p.get("type") in ("text", "input_text"))


def events_ollama(body):
    out = []
    for m in body.get("messages", []):
        role, content = m.get("role"), m.get("content") or ""
        if role == "assistant":
            if content:
                out.append(["assistant", content])
            for c in m.get("tool_calls") or []:
                out.append(["call", c["function"]["name"], _args(c["function"].get("arguments"))])
        elif role == "tool":
            out.append(["tool", content])
        else:
            if content:
                out.append([role, content])
            if m.get("images"):
                out.append(["images", len(m["images"])])
    return out


def events_openai(body):
    out = []
    for m in body.get("messages", []):
        role, content = m.get("role"), m.get("content")
        if role == "assistant":
            if _text_of(content):
                out.append(["assistant", _text_of(content)])
            for c in m.get("tool_calls") or []:
                out.append(["call", c["function"]["name"], _args(c["function"].get("arguments"))])
        elif role == "tool":
            out.append(["tool", _text_of(content)])
        else:
            if _text_of(content):
                out.append([role, _text_of(content)])
            if isinstance(content, list):
                n = sum(1 for p in content if isinstance(p, dict) and p.get("type") == "image_url")
                if n:
                    out.append(["images", n])
    return out


def events_anthropic(body):
    out = []
    system = body.get("system")
    if system:
        out.append(["system", system if isinstance(system, str) else _text_of(system)])
    for m in body.get("messages", []):
        role, content = m.get("role"), m.get("content")
        if isinstance(content, str):
            if content:
                out.append([role, content])
            continue
        images = 0
        for block in content or []:
            kind = block.get("type")
            # Emitted where the run of images ends, not at the end of the
            # message: this wire folds consecutive user messages into one,
            # so position within it is the only ordering there is.
            if kind != "image" and images:
                out.append(["images", images])
                images = 0
            if kind == "text":
                if block.get("text"):
                    out.append([role, block["text"]])
            elif kind == "tool_use":
                out.append(["call", block["name"], block.get("input") or {}])
            elif kind == "tool_result":
                out.append(["tool", _text_of(block.get("content"))])
            elif kind == "image":
                images += 1
        if images:
            out.append(["images", images])
    return out


def tools_before_frames(events):
    """Within one batch of tool output, text results first, frames after.

    The Python agent appended a tool's frames straight after that tool's
    result, so a turn with two calls produced tool, images, tool — a user
    message between two tool results, which the OpenAI and Anthropic APIs
    reject (PYTHON_BUGS #19). The Rust agent appends the frames after the
    batch. Same content, and the order is the one deliberate difference,
    so it is removed here rather than left to fail every frames scenario.
    """
    def is_frame_text(event, following):
        # A frames message is a `user` text part plus its images — or,
        # once pruned, the text alone carrying the pruned notice.
        if event[0] != "user":
            return False
        return (following is not None and following[0] == "images") or "frames pruned" in str(event[1])

    units = []  # (kind, [events])
    index = 0
    while index < len(events):
        event = events[index]
        following = events[index + 1] if index + 1 < len(events) else None
        if event[0] == "tool":
            units.append(("tool", [event]))
        elif is_frame_text(event, following):
            taken = [event, following] if following is not None and following[0] == "images" else [event]
            units.append(("frame", taken))
            index += len(taken) - 1
        elif event[0] == "images":
            units.append(("frame", [event]))
        else:
            units.append(("other", [event]))
        index += 1

    out, batch = [], []

    def flush():
        for wanted in ("tool", "frame"):
            for kind, taken in batch:
                if kind == wanted:
                    out.extend(taken)
        batch.clear()

    for kind, taken in units:
        # A batch opens on a tool result: the run's opening user prompt is
        # never a frame, and must not be swept into one.
        if kind == "tool" or (kind == "frame" and batch):
            batch.append((kind, taken))
        else:
            flush()
            out.extend(taken)
    flush()
    return out


REDUCERS = {"ollama": events_ollama, "openai": events_openai, "anthropic": events_anthropic}


def tool_names(wire, body):
    if wire == "anthropic":
        return sorted(t["name"] for t in body.get("tools") or [])
    return sorted(t["function"]["name"] for t in body.get("tools") or [])


def token_cap(wire, body):
    if wire == "ollama":
        return (body.get("options") or {}).get("num_predict") or body.get("max_tokens")
    return body.get("max_tokens") or body.get("max_completion_tokens")


def reduce_log(wire, log):
    out = []
    for entry in log:
        if entry["wire"] != wire:
            # LiteLLM asks Ollama about the model before its first chat
            # (`/api/show`); that is the library probing, not the agent
            # talking to the model, and rig does not do it.
            continue
        body = entry["body"]
        out.append({
            "events": tools_before_frames(REDUCERS[wire](body)),
            "tools": tool_names(wire, body),
            "max_tokens": token_cap(wire, body),
            "authorized": entry.get("authorized"),
        })
    return out


# ── what Command Center ends up holding ──────────────────────────────────

PREFIX_ONLY = ("LLM call failed:", "Agent harness failure:")


# The two agents run a second apart, and tool output is full of "now":
# `created_at` on the incident just filed, `last_seen` on fixture cameras
# seeded relative to the clock. The FORMAT is still compared — a
# timestamp that changed shape would no longer match the pattern and
# would survive as a difference.
TIMESTAMP = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,6})?")


def soften(value):
    """Mask clock readings, and cut a library's error tail to its prefix."""
    if isinstance(value, str):
        value = TIMESTAMP.sub("<ts>", value)
        for prefix in PREFIX_ONLY:
            if value.startswith(prefix):
                # A filing-then-failure summary keeps its stable suffix.
                tail = " An incident was filed before the cutoff."
                return prefix + " <library text>" + (tail if value.endswith(tail) else "")
        return value
    if isinstance(value, dict):
        return {k: soften(v) for k, v in value.items()}
    if isinstance(value, list):
        return [soften(v) for v in value]
    return value


def collect(run_ids, first_new_incident):
    rows = psql(
        "SELECT json_agg(r ORDER BY r.id) FROM (SELECT id, org_id, outcome, severity, incident_id, "
        "tool_call_count, summary, tool_trace, (started_at IS NOT NULL) AS started, "
        "(completed_at IS NOT NULL) AS completed FROM sentinel_runs "
        f"WHERE id IN ({','.join(repr(i) for i in run_ids)})) r"
    )
    runs = json.loads(rows) if rows else []
    for r in runs or []:
        if r.get("tool_trace"):
            r["tool_trace"] = json.loads(r["tool_trace"])
    incidents = psql(
        "SELECT json_agg(i ORDER BY i.id) FROM (SELECT id, org_id, title, summary, report, severity, "
        f"status, camera_id, created_by FROM incidents WHERE id >= {first_new_incident}) i"
    )
    evidence = psql(
        "SELECT json_agg(e ORDER BY e.id) FROM (SELECT id, incident_id, kind, text, camera_id FROM "
        f"incident_evidence WHERE incident_id >= {first_new_incident}) e"
    )
    return {
        "runs": soften(runs or []),
        "incidents": json.loads(incidents) if incidents else [],
        "evidence": json.loads(evidence) if evidence else [],
    }


def restart_command_center():
    """A clean rate limiter for every play.

    Command Center's per-org MCP limit is in-process and the budget
    scenarios spend it. Without this the limit lands on whichever agent
    runs next — it showed up as two "differences" that were the same
    refusal arriving at different agents.
    """
    # A path, not a command line: this repository lives under a directory
    # with a space in its name, and splitting a command string on
    # whitespace cut the path in half.
    script = os.environ.get("CC_RESTART_SCRIPT")
    if script:
        subprocess.run([script, "--restart-cc"], check=True, timeout=60)


def play(agent: str, wire: str, runs, turns):
    restart_command_center()
    new_id = seed(runs)
    status, _ = http("POST", FAKE + "/__script", {"turns": substitute(turns, new_id)})
    assert status == 200
    wake_status, wake_body = wake(AGENTS[agent])
    _, raw = http("GET", FAKE + "/__log")
    return {
        "wakeup": [wake_status, soften(wake_body)],
        "llm": soften(reduce_log(wire, json.loads(raw))),
        "state": collect([r["id"] for r in runs], new_id),
    }


# ── differences that are decisions, not bugs ─────────────────────────────

def collapse_retries(py, rs):
    """The OpenAI SDK under LiteLLM re-sends a 5xx; rig does not.

    Nothing in the agent asked for that retry — it is the SDK's default,
    and it exists on this one wire only: the same failure on Ollama, the
    production default, was always a single attempt. The Rust agent makes
    it one attempt everywhere, and the run's 3-attempt `/complete` and
    the reaper are what cover a transient provider.

    So identical consecutive requests are collapsed on both sides, and
    the shape of the divergence is asserted rather than waved through: if
    Python stops retrying, or Rust starts, this fails.
    """
    def collapsed(requests):
        out = []
        for request in requests:
            if not out or out[-1] != request:
                out.append(request)
        return out

    if not (len(py["llm"]) > 1 and len(rs["llm"]) == 1):
        return f"expected python to retry and rust not to: python {len(py['llm'])} call(s), rust {len(rs['llm'])}"
    py["llm"], rs["llm"] = collapsed(py["llm"]), collapsed(rs["llm"])
    return None


MALFORMED = "malformed tool arguments degrade to none"


def unparsed_replay(py, rs):
    """What goes BACK to the model after a call with unparseable arguments.

    Both agents call the tool with no arguments. The Python then replayed
    the model's own broken string in the history; rig decodes arguments
    into JSON on arrival and has nowhere to keep a string that is not, so
    the Rust agent replays `{}` — what was actually executed. Asserted in
    both directions, then removed.
    """
    def unparsed(result):
        return [e for r in result["llm"] for e in r["events"]
                if e[0] == "call" and isinstance(e[2], dict) and "__unparsed__" in e[2]]

    if not unparsed(py) or unparsed(rs):
        return (f"expected python to replay the broken string and rust not to: "
                f"python {len(unparsed(py))}, rust {len(unparsed(rs))}")
    for event in unparsed(py):
        event[2] = {}
    return None


EXPECTED_DIVERGENCES = {OPENAI_5XX: collapse_retries, MALFORMED: unparsed_replay}


def first_difference(a, b, path=""):
    if type(a) is not type(b):
        return f"{path or '.'}: python {a!r:.300} | rust {b!r:.300}"
    if isinstance(a, dict):
        for key in sorted(set(a) | set(b)):
            if key not in a or key not in b:
                return f"{path}.{key}: only in {'python' if key in a else 'rust'}"
            found = first_difference(a[key], b[key], f"{path}.{key}")
            if found:
                return found
        return None
    if isinstance(a, list):
        for index, (x, y) in enumerate(zip(a, b)):
            found = first_difference(x, y, f"{path}[{index}]")
            if found:
                return found
        if len(a) != len(b):
            return f"{path}: python has {len(a)} item(s), rust {len(b)}"
        return None
    return None if a == b else f"{path or '.'}: python {a!r:.300} | rust {b!r:.300}"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--wire", required=True, choices=ALL)
    ap.add_argument("--only")
    ap.add_argument("-v", action="store_true")
    args = ap.parse_args()

    cases = [s for s in SCENARIOS if args.wire in s[1] and (not args.only or args.only in s[0])]
    differing = 0
    llm_calls = images_seen = incidents_filed = pruned = 0
    for name, _, runs, turns in cases:
        py = play("python", args.wire, runs, turns)
        rs = play("rust", args.wire, runs, turns)
        expected = EXPECTED_DIVERGENCES.get(name)
        difference = (expected(py, rs) if expected else None) or first_difference(py, rs)
        for request in py["llm"]:
            llm_calls += 1
            for event in request["events"]:
                if event[0] == "images":
                    images_seen += event[1]
                if "frames pruned" in str(event[1:]):
                    pruned += 1
        incidents_filed += len(py["state"]["incidents"])
        if difference:
            differing += 1
            print(f"DIFF  [{args.wire}] {name}\n        {difference}")
            # The first difference is often a symptom — a model call
            # count — and the run's own summary is the cause.
            for side, result in (("python", py), ("rust", rs)):
                for row in result["state"]["runs"]:
                    print(f"        {side:6} {row['outcome']:9} {str(row['summary'])[:260]!r}")
        elif args.v:
            outcomes = [r["outcome"] for r in py["state"]["runs"]]
            print(f"  ok  [{args.wire}] {name}  → {outcomes}, {len(py['llm'])} model call(s)")

    # Two agents that both did nothing agree perfectly. These are the
    # things the scenario list exists to exercise; if the reference side
    # never produced them, the comparison above was of two empty runs.
    print(f"\ncoverage [{args.wire}]: {llm_calls} model call(s), {images_seen} image(s) sent, "
          f"{pruned} pruned frame notice(s), {incidents_filed} incident(s) filed")
    if not args.only:
        thin = []
        if llm_calls < len(cases):
            thin.append("fewer model calls than scenarios")
        if images_seen == 0:
            thin.append("no image ever reached the model")
        if pruned == 0:
            thin.append("no frame was ever pruned")
        if incidents_filed == 0:
            thin.append("no incident was ever filed")
        if thin:
            print("COVERAGE TOO THIN: " + "; ".join(thin), file=sys.stderr)
            return 2

    print(f"{len(cases) - differing}/{len(cases)} identical, {differing} differing")
    return 1 if differing else 0


if __name__ == "__main__":
    sys.exit(main())
