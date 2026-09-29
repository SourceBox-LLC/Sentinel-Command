#!/usr/bin/env python3
"""Hold the MCP tool catalog, rate limits and result framing to Python's.

Three lists and a table decide what an MCP key can reach and how often,
and all four are transcribed constants. A transcription slip in any of
them is silent: a tool missing from the READ set is simply unreachable
by a readonly key, an extra name in the agent's allowlist is a tool the
agent can call that Python would refuse, and a wrong rate limit is only
visible under load.

A fourth list decides how each tool's result is FRAMED, and it is
transcribed from something even less visible: the return annotation on
the Python function. MCP requires an output schema to be an object, so
FastMCP wraps a non-object return under a `result` key and wraps the
structured half of every call result to match. Change `-> dict` to
`-> list[dict]` on either side and the wire shape moves under a client
that is reading `structuredContent`.

The agent allowlist is the one that matters most. It is an allowlist
rather than a denylist because the agent's model is steered by content
an attacker can put in front of a lens — so a write tool leaking into
it is a real escalation, not a cosmetic difference.

Reads both sides as source. The Python is parsed with `ast` rather than
imported: importing `app.mcp.server` builds a FastMCP instance and
opens a database.

Usage: mcp_parity.py
"""
from __future__ import annotations

import ast
import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
BACKEND = HERE.parent.parent.parent / "backend"
RS = HERE.parent.parent / "src" / "mcp" / "scope.rs"


def python_sets() -> dict[str, set[str]]:
    """The three frozensets, read out of the module's source."""
    tree = ast.parse((BACKEND / "app/mcp/server.py").read_text())
    wanted = {"MCP_READ_TOOLS", "MCP_WRITE_TOOLS", "_AGENT_WRITE_TOOLS"}
    out: dict[str, set[str]] = {}
    for node in ast.walk(tree):
        if not isinstance(node, ast.AnnAssign | ast.Assign):
            continue
        targets = [node.target] if isinstance(node, ast.AnnAssign) else node.targets
        for target in targets:
            if not isinstance(target, ast.Name) or target.id not in wanted:
                continue
            call = node.value
            # `frozenset({...})`
            if isinstance(call, ast.Call) and call.args:
                inner = call.args[0]
                if isinstance(inner, ast.Set):
                    out[target.id] = {
                        e.value for e in inner.elts if isinstance(e, ast.Constant)
                    }
    return out


def python_descriptions() -> dict[str, str]:
    """The `description=` on every `@mcp.tool`.

    Python reads these back off the live FastMCP registry so a UI edit
    cannot desync from the server. The port has no registry to read, so
    the strings are constants — which means they can drift, which is
    why they are checked.
    """
    tree = ast.parse((BACKEND / "app/mcp/server.py").read_text())
    out = {}
    for node in ast.walk(tree):
        if not isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
            continue
        for dec in node.decorator_list:
            if not (isinstance(dec, ast.Call) and isinstance(dec.func, ast.Attribute)
                    and dec.func.attr == "tool"):
                continue
            kw = {k.arg: k.value for k in dec.keywords}
            name = ast.literal_eval(kw["name"]) if "name" in kw else node.name
            out[name] = ast.literal_eval(kw["description"]) if "description" in kw else ""
    return out


def rust_descriptions() -> dict[str, str]:
    src = RS.read_text()
    consts = dict(re.findall(r'^const (DESC_[A-Z_]+): &str = "((?:[^"\\]|\\.)*)";',
                             src, re.M))
    out = {}
    block = re.search(r"pub const TOOL_DESCRIPTIONS: \[\(&str, &str\); \d+\] = \[(.*?)\];",
                      src, re.S)
    if not block:
        return out
    for name, const in re.findall(r'\("([^"]+)", (DESC_[A-Z_]+)\)', block.group(1)):
        raw = consts.get(const, "")
        out[name] = raw.replace('\\"', '"').replace("\\\\", "\\")
    return out


def python_result_framing() -> dict[str, str]:
    """How FastMCP frames each tool's result, per its return annotation.

    Three outcomes, and the annotation alone decides which:

      * `dict` — already an object, so the structured half is the
        returned value as-is;
      * `list[dict]` — not an object, so the schema (and the structured
        half) is wrapped under `result`;
      * `Image`, or no annotation at all — no serialisable schema, so
        there is no structured half whatsoever.
    """
    tree = ast.parse((BACKEND / "app/mcp/server.py").read_text())
    out: dict[str, str] = {}
    for node in ast.walk(tree):
        if not isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
            continue
        if not any(
            isinstance(dec, ast.Call)
            and isinstance(dec.func, ast.Attribute)
            and dec.func.attr == "tool"
            for dec in node.decorator_list
        ):
            continue
        if node.returns is None:
            out[node.name] = "media"
            continue
        annotation = ast.unparse(node.returns)
        if annotation == "dict":
            out[node.name] = "object"
        elif annotation.startswith(("list[", "tuple[")):
            out[node.name] = "wrapped"
        else:
            # Image / Audio / File / ToolResult all reach FastMCP's
            # `_UnserializableType` sentinel and produce no schema.
            out[node.name] = "media"
    return out


def rust_result_framing() -> dict[str, str]:
    """The same three outcomes, read off the port.

    `WRAP_RESULT_TOOLS` names the wrapped ones. The media ones are the
    dispatch arms that do NOT go through the `json(...)` adapter — that
    adapter is what attaches a structured half, so its absence IS the
    media case.
    """
    scope = RS.read_text()
    server = (RS.parent / "server.rs").read_text()
    out: dict[str, str] = {}

    block = re.search(r"pub const WRAP_RESULT_TOOLS: \[&str; \d+\] = \[(.*?)\];", scope, re.S)
    wrapped = set(re.findall(r'"([^"]+)"', block.group(1))) if block else set()

    dispatch = re.search(r"match name \{(.*?)\n        \}", server, re.S)
    if not dispatch:
        return out
    # Two arm shapes: `"x" => json(...)` and the braced
    # `"x" => {\n json(...) }` rustfmt produces for a long call.
    for arm, body in re.findall(
        r'"([a-z_]+)" =>\s*\{?\s*(json\(|t::)', dispatch.group(1), re.S
    ):
        if arm in wrapped:
            out[arm] = "wrapped"
        else:
            out[arm] = "object" if body == "json(" else "media"
    for name in wrapped - set(out):
        out[name] = "wrapped"
    return out


def python_rate_limits() -> dict[str, tuple[int, int]]:
    tree = ast.parse((BACKEND / "app/mcp/server.py").read_text())
    for node in ast.walk(tree):
        if isinstance(node, ast.Assign) and any(
            isinstance(t, ast.Name) and t.id == "RATE_LIMITS" for t in node.targets
        ):
            table = ast.literal_eval(node.value)
            return {plan: (v["minute"], v["daily"]) for plan, v in table.items()}
    return {}


def rust_sets() -> dict[str, set[str]]:
    src = RS.read_text()
    out = {}
    for rust_name, python_name in [
        ("MCP_READ_TOOLS", "MCP_READ_TOOLS"),
        ("MCP_WRITE_TOOLS", "MCP_WRITE_TOOLS"),
        ("AGENT_WRITE_TOOLS", "_AGENT_WRITE_TOOLS"),
    ]:
        m = re.search(
            rf"pub const {rust_name}: \[&str; \d+\] = \[(.*?)\];", src, re.S
        )
        if not m:
            print(f"  FAIL  {rust_name} not found in scope.rs")
            continue
        out[python_name] = set(re.findall(r'"([^"]+)"', m.group(1)))
    return out


def rust_rate_limits() -> dict[str, tuple[int, int]]:
    src = RS.read_text()
    m = re.search(r"pub fn rate_limits\(plan: &str\) -> Option<\(usize, usize\)> \{(.*?)\n\}", src, re.S)
    if not m:
        return {}
    out = {}
    for line in m.group(1).splitlines():
        arm = re.match(r'\s*("[^=]+?) => Some\(\((\d[\d_]*), (\d[\d_]*)\)\),', line)
        if not arm:
            continue
        minute = int(arm.group(2).replace("_", ""))
        daily = int(arm.group(3).replace("_", ""))
        for plan in re.findall(r'"([^"]+)"', arm.group(1)):
            out[plan] = (minute, daily)
    return out


def main() -> int:
    bad = 0
    py = python_sets()
    rs = rust_sets()
    if not py:
        print("REFUSING: could not read the Python tool sets")
        return 2
    for name in sorted(py):
        a, b = py[name], rs.get(name, set())
        if a == b:
            print(f"  ok    {name:22} {len(a)} tools")
            continue
        bad += 1
        print(f"  FAIL  {name}")
        for missing in sorted(a - b):
            print(f"          only in python: {missing}")
        for extra in sorted(b - a):
            print(f"          only in rust:   {extra}")

    # The derived agent set, checked as a whole rather than trusting
    # that the right inputs imply the right output.
    py_agent = (py["MCP_READ_TOOLS"] | py["_AGENT_WRITE_TOOLS"]) & (
        py["MCP_READ_TOOLS"] | py["MCP_WRITE_TOOLS"]
    )
    excluded = (py["MCP_READ_TOOLS"] | py["MCP_WRITE_TOOLS"]) - py_agent
    print(f"  ok    agent reaches {len(py_agent)}, excluded: {sorted(excluded) or 'nothing'}")
    if not excluded:
        print("  FAIL  the agent allowlist excludes NOTHING — it has stopped being one")
        bad += 1

    pd, rd = python_descriptions(), rust_descriptions()
    if not pd:
        print("REFUSING: could not read the Python tool descriptions")
        return 2
    if set(pd) != set(rd):
        bad += 1
        print("  FAIL  the described tool set differs")
        for missing in sorted(set(pd) - set(rd)):
            print(f"          only in python: {missing}")
        for extra in sorted(set(rd) - set(pd)):
            print(f"          only in rust:   {extra}")
    else:
        differing = [n for n in sorted(pd) if pd[n].strip() != rd[n].strip()]
        if differing:
            bad += 1
            print(f"  FAIL  {len(differing)} description(s) differ")
            for name in differing[:3]:
                print(f"          {name}")
                print(f"            python={pd[name][:90]!r}")
                print(f"            rust=  {rd[name][:90]!r}")
        else:
            print(f"  ok    descriptions        {len(pd)} tools")
        # A description is what an agent reads to decide whether a tool
        # is the one it wants. An empty one is not a port decision.
        empty = [n for n in sorted(rd) if not rd[n].strip()]
        if empty:
            bad += 1
            print(f"  FAIL  {len(empty)} tool(s) describe as empty: {empty[:5]}")

    pf, rf = python_result_framing(), rust_result_framing()
    if not pf:
        print("REFUSING: could not read the Python return annotations")
        return 2
    if pf != rf:
        bad += 1
        print("  FAIL  result framing")
        for name in sorted(set(pf) | set(rf)):
            if pf.get(name) != rf.get(name):
                print(f"          {name:28} python={pf.get(name)} rust={rf.get(name)}")
    else:
        counts = {kind: sum(1 for v in pf.values() if v == kind) for kind in
                  ("object", "wrapped", "media")}
        print(f"  ok    result framing       {counts}")
        # If nothing is wrapped, the wrap path is dead and this check
        # has stopped testing anything.
        if not counts["wrapped"]:
            print("  FAIL  no tool wraps its result — the check proves nothing")
            bad += 1

    pl, rl = python_rate_limits(), rust_rate_limits()
    if pl != rl:
        bad += 1
        print("  FAIL  RATE_LIMITS")
        print(f"          python={pl}")
        print(f"          rust=  {rl}")
    else:
        print(f"  ok    RATE_LIMITS          {len(pl)} plans")

    print(f"\n{bad} mismatch(es)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
