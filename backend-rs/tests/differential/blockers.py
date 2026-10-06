#!/usr/bin/env python3
"""Which primitive blocks each unported route.

Route-by-route porting stalled once the easy handlers were gone, and
"which route can I do next" became a question answered by reading
imports and guessing. Guessing was wrong twice: an earlier inventory
called 33 routes clear by looking only at a handler's own module, and a
later one missed every route defined in main.py because it keyed off
`app.routes` handlers it never resolved.

So this resolves each route's handler function, walks the call graph
transitively across the whole `app` package by AST, and reports every
blocking primitive it can reach. A blocker is something that lives in
the Python *process* (an in-memory cache, a subscriber registry) or
that has no Rust equivalent written yet.

Usage:  blockers.py            # unported routes, grouped by blocker set
        blockers.py --all      # include the ones already ported
        blockers.py --why PATH # the call chain that reaches each blocker
"""
from __future__ import annotations

import ast
import os
import pathlib
import re
import sys
import tempfile
from collections import defaultdict

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parents[2]
BACKEND = REPO / "backend"
APP = BACKEND / "app"

# Modules that already have a Rust equivalent. The walk stops at these
# and does not propagate what they reach.
#
# `core.auth` is the one that matters: it calls
# `clerk.authenticate_request`, so counting `core.clerk` through it
# marked every authenticated route in the service as blocked on the
# Clerk Backend API. The Rust port does not use that API at all — it
# verifies RS256 locally against cached JWKS — so the edge is a fiction.
# The four real Backend API call sites are recipients.py,
# plans.py, health_probes.py and webhooks.py.
PORTED = {
    "core.auth",
    "core.local_auth",
    "core.limiter",
    "core.audit",
    "core.database",
    "core.request_context",
    "core.errors",
    "core.logging_setup",
    "core.config",
    "core.sentry",
    "core.migrations",
    "models.models",
    "models",
    # Ported in the notification slice. Stopping here matters as much
    # as it does for `core.auth`: `core.recipients` calls the Clerk
    # Backend API, and without this every route that emits a
    # notification counts `core.clerk` as a blocker — when the Rust
    # side makes that same membership call itself.
    "core.recipients",
    "core.email_templates",
    "core.email_unsubscribe",
    "core.versions",
    "core.release_cache",
    # Ported with the camera-cap slice, and like `core.recipients` it
    # has to STOP the walk rather than merely be excused: the resolver
    # calls Clerk's billing API through `core.clerk`, so descending past
    # it counts `core.clerk` as a blocker for every route that resolves
    # a plan — when the Rust resolver makes that same REST call itself.
    #
    # `get_plan_limits_for_org` has no named Rust counterpart; it is two
    # lines composing `resolve_org_plan` and `get_plan_limits`, and the
    # one ported caller inlines it (see hls.rs).
    "core.plans",
    # The transport and the outbox drain, ported with the email slice
    # and verified by email_run.sh against a fake Resend.
    "core.email",
    "core.email_worker",
    # The readiness probes, ported with the health slice. It reaches
    # `core.clerk` through the Clerk probe, so like `core.plans` it has
    # to stop the walk: the Rust probe makes that same REST call.
    "core.health_probes",
    # The `osi_` key resolver, ported with the integration slice as the
    # `IntegrationUser` extractor.
    "core.integration_auth",
}

# Modules only PART of which has a Rust equivalent. Importing one of the
# listed names does not block a route; importing anything else from the
# module still does. `core.plans` lived here while `enforce_camera_cap`
# was the one unported piece — module granularity alone would have had
# to choose between hiding it and flagging every caller of
# `effective_plan_for_caps`. It is ported now, so core.plans has moved
# up to PORTED entirely.
PORTED_FUNCTIONS = {
    "core.license_client": {
        "is_sentinel_licensed", "is_sync_enabled", "sentinel_blocked_by_license",
        "SENTINEL_LICENSE_GRACE_HOURS",
    },
    # The dispatch path is ported; `reap_stranded_runs` — the reaper
    # loop's body — is not, and no route reaches it.
    "core.sentinel_dispatch": {
        "maybe_dispatch_for_notification", "dispatch_manual_run",
        "global_dispatch_allowed", "cap_for_plan", "cap_remaining",
        "runs_used_this_month", "runs_used_this_month_global",
        "SENTINEL_PLANS", "STRANDED_RUN_AGE_MINUTES",
    },
}

# A module whose *import alone* means the route cannot move yet, with the
# reason. Keys are module prefixes under `app.`.
# Ported since this table was written, and so deliberately absent
# below: core.recipients, core.email_templates, core.email_unsubscribe,
# core.sentinel_dispatch, core.versions, core.release_cache, core.plans,
# and — since the email slice — core.email and core.email_worker.
BLOCKERS = {
    "core.clerk": "Clerk Backend API",
    "core.license_client": "Sentinel License Service client",
    "core.sync_client": "Sentinel Sync Service client",
    "api.hls": "in-process HLS segment + playlist caches",  # now Rust's — see PORTED_STATE
    "api.ws": "in-process WebSocket connection manager",  # likewise
    "mcp": "MCP server (fastmcp -> rmcp)",
}

# Unported, but nothing stands in the way — plain logic or DB work that
# simply has not been written yet. Listing these separately keeps
# "blocked" meaning blocked.
JUST_WORK = {
    "core.csv_export": "CSV export",
    "core.gdpr": "GDPR export builder",
    "core.codec": "codec negotiation",
}

# In-process state that lives as a module-level *variable* inside a
# module that is otherwise portable — so neither a module-level blocker
# nor the handler-lives-in-a-blocked-module rule can see it.
#
# Found the hard way: once plans and the licence gate were ported, this
# script reported /api/motion/events/stream and /api/notifications/stream
# as clear. Both subscribe to a broadcaster whose publishers — motion
# ingestion and notification creation — are still in Python. Served
# from Rust, those streams would accept the connection and never
# deliver an event, which no status code and no differential would
# show.
BLOCKING_SYMBOLS = {
    ("api.motion", "motion_broadcaster"): "in-process motion SSE broadcaster",
    ("api.motion", "integration_motion_broadcaster"): "in-process motion SSE broadcaster",
    ("api.notifications", "notification_broadcaster"): "in-process notification SSE broadcaster",
    ("api.notifications", "_transition_debounce"): "in-process transition debounce",
}

# The same distinction as PORTED_STATE, one level down: in-process state
# named by BLOCKING_SYMBOLS that the port has already taken over. Once
# `GET /api/notifications/stream` is served from Rust, every subscriber
# is on Rust's broadcaster — so a Python route that still publishes to
# its own copy is not blocked, it is overdue, and its notifications
# reach nobody.
PORTED_SYMBOLS = {
    ("api.notifications", "notification_broadcaster"),
    ("api.notifications", "_transition_debounce"),
    # Both motion feeds moved with their producer: the HTTP motion
    # route and the socket's `event` frame are Rust's, so Python's
    # broadcasters have no publisher left.
    ("api.motion", "motion_broadcaster"),
    ("api.motion", "integration_motion_broadcaster"),
}

# In-process state the port has already taken over. A route whose only
# remaining blockers are in here is not blocked at all — it is *overdue*,
# because the Python copy of that state is no longer the live one. The
# plan panel is the example that made this worth distinguishing: it reads
# the viewer-second counter, and once segments were served from Rust, the
# Python route answered zero hours used for every org. That looks like a
# counter reset, not like a port boundary.
PORTED_STATE = {
    "api.hls": "the HLS caches and the viewer-second counter are Rust's",
    # A node holds one socket, to one machine. Once Rust serves
    # `/ws/node`, every connected node is on Rust's registry, and a
    # Python route asking its own `manager` whether a node is connected
    # is asking a registry nothing ever writes to — so every command it
    # would send reports the node offline.
    "api.ws": "the CameraNode socket registry is Rust's",
}

# Routes nothing blocks and nobody should port: FastAPI generates them
# from its own route table, so there is no handler to port and no
# response to hold to a differential.
NOT_A_PORT = {
    "/api/openapi.json": "FastAPI generates this from its own routes",
    "/api-docs": "Swagger UI over that schema",
    "/api-redoc": "ReDoc over that schema",
    "/docs/oauth2-redirect": "Swagger UI's OAuth callback page",
}

# Standard-library imports that still need a Rust counterpart chosen.
#
# `zoneinfo` is no longer one: `src/zoneinfo.rs` reproduces its lookup —
# the tzdata package's name list plus a walk of the system directories,
# the system copy preferred, and the three different ways it fails —
# over jiff's bundled database, pinned to the same IANA release. It is
# what `GET /api/sentinel/runs` computes "today" with.
STDLIB_BLOCKERS: dict[str, str] = {}


def module_name(path: pathlib.Path) -> str:
    rel = path.relative_to(APP).with_suffix("")
    parts = list(rel.parts)
    if parts[-1] == "__init__":
        parts.pop()
    return ".".join(parts)


class ModuleIndex:
    """Every function in the app package, and what each one calls."""

    def __init__(self) -> None:
        self.calls: dict[str, set[str]] = {}
        self.imports: dict[str, dict[str, str]] = {}
        self.module_imports: dict[str, set[str]] = defaultdict(set)
        self.defs: dict[str, set[str]] = defaultdict(set)
        self.globals: dict[str, set[str]] = defaultdict(set)
        for path in sorted(APP.rglob("*.py")):
            if "__pycache__" in path.parts:
                continue
            self._index(path)

    def _index(self, path: pathlib.Path) -> None:
        mod = module_name(path)
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))

        # name -> module it came from, for resolving a call to its home
        local: dict[str, str] = {}
        for node in ast.walk(tree):
            if isinstance(node, ast.ImportFrom) and node.module:
                target = node.module
                if node.level:  # relative import
                    base = mod.rsplit(".", node.level - 1)[0] if node.level > 1 else mod
                    target = f"{base}.{node.module}" if node.module else base
                target = target[4:] if target.startswith("app.") else target
                self.module_imports[mod].add(target)
                for alias in node.names:
                    local[alias.asname or alias.name] = target
            elif isinstance(node, ast.Import):
                for alias in node.names:
                    target = alias.name
                    target = target[4:] if target.startswith("app.") else target
                    self.module_imports[mod].add(target)
                    local[alias.asname or alias.name] = target
        self.imports[mod] = local

        # Module-level assignments, so a bare name used inside a function
        # can be traced to a variable defined beside it.
        for node in tree.body:
            targets = []
            if isinstance(node, ast.Assign):
                targets = node.targets
            elif isinstance(node, ast.AnnAssign):
                targets = [node.target]
            for t in targets:
                if isinstance(t, ast.Name):
                    self.globals[mod].add(t.id)

        for node in ast.walk(tree):
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                qual = f"{mod}:{node.name}"
                self.defs[mod].add(node.name)
                called: set[str] = set()
                for sub in ast.walk(node):
                    if isinstance(sub, ast.Call):
                        f = sub.func
                        if isinstance(f, ast.Name):
                            called.add(f.id)
                        elif isinstance(f, ast.Attribute):
                            called.add(f.attr)
                            # `tracker.get_stats()` — the object matters
                            # more than the method, so record it too.
                            if isinstance(f.value, ast.Name):
                                called.add(f.value.id)
                    elif isinstance(sub, ast.Name):
                        called.add(sub.id)
                self.calls[qual] = called
        # Decorators and default arguments (Depends(require_admin)) are
        # inside the FunctionDef, so ast.walk above already caught them.

    def blockers_for(self, mod: str, names: set[str]) -> set[tuple[str, str]]:
        """Blocking primitives reachable from `names` used inside `mod`."""
        found = set()
        # A handler defined *inside* a blocked module is blocked by it.
        # Only imports were checked at first, which called
        # /api/cameras/{camera_id}/stream.m3u8 clear — it lives in
        # api/hls.py and reads that module's own segment cache off a
        # global, so there was no import edge to find.
        for prefix, why in BLOCKERS.items():
            if mod == prefix or mod.startswith(prefix + "."):
                found.add((prefix, why))
        local = self.imports.get(mod, {})
        for name in names:
            home = local.get(name) or (mod if name in self.globals.get(mod, ()) else None)
            if home and (home, name) in BLOCKING_SYMBOLS:
                found.add((f"{home}:{name}", BLOCKING_SYMBOLS[(home, name)]))
        for name in names:
            target = local.get(name)
            if target is None:
                continue
            if any(target == m or target.startswith(m + ".") for m in PORTED):
                continue
            if name in PORTED_FUNCTIONS.get(target, ()):
                continue
            for prefix, why in BLOCKERS.items():
                if target == prefix or target.startswith(prefix + "."):
                    found.add((prefix, why))
            for prefix, why in STDLIB_BLOCKERS.items():
                if target == prefix or target.startswith(prefix + "."):
                    found.add((prefix, why))
        return found

    def walk(self, start: str, seen: set[str] | None = None) -> tuple[set[tuple[str, str]], list[str]]:
        """Transitively collect blockers reachable from `mod:func`."""
        seen = seen if seen is not None else set()
        if start in seen or start not in self.calls:
            return set(), []
        seen.add(start)
        mod = start.split(":", 1)[0]
        names = self.calls[start]
        found = self.blockers_for(mod, names)
        chain = [start] if found else []
        local = self.imports.get(mod, {})
        for name in names:
            # same-module call
            if name in self.defs.get(mod, ()):
                sub, subchain = self.walk(f"{mod}:{name}", seen)
                if sub:
                    found |= sub
                    chain += [start] + subchain
                continue
            # imported call — follow it into its own module, unless the
            # module is already ported and therefore a dead end.
            target = local.get(name)
            if target and any(target == m or target.startswith(m + ".") for m in PORTED):
                continue
            if target and name in PORTED_FUNCTIONS.get(target, ()):
                continue
            if target and target in self.defs and name in self.defs[target]:
                sub, subchain = self.walk(f"{target}:{name}", seen)
                if sub:
                    found |= sub
                    chain += [start] + subchain
        return found, chain


def routes() -> list[tuple[str, str, str]]:
    """(path, methods, "module:function") for every HTTP route."""
    import deleted_python

    deleted_python.require(BACKEND / "app/main.py")
    sys.path.insert(0, str(BACKEND))
    os.environ.setdefault(
        "DATABASE_URL", "sqlite:///" + tempfile.gettempdir() + "/cc-blockers.db"
    )
    from app.main import app  # noqa: PLC0415
    from fastapi.routing import APIRoute  # noqa: PLC0415
    from starlette.routing import Route  # noqa: PLC0415

    out = []
    stack = list(app.routes)
    seen_routers = set()
    while stack:
        r = stack.pop()
        # include_router() results are wrapped in `_IncludedRouter`,
        # whose own `.path` is None and which exposes the real router as
        # `.original_router` rather than `.routes`. Missing this is why
        # the first version of this script reported six routes out of
        # eighty-five and called four of them clear.
        inner = getattr(r, "original_router", None)
        if inner is not None and id(inner) not in seen_routers:
            seen_routers.add(id(inner))
            stack.extend(getattr(inner, "routes", []) or [])
            continue
        sub = getattr(r, "routes", None)
        if sub:
            stack.extend(sub)
            continue
        if not isinstance(r, (APIRoute, Route)):
            continue
        fn = getattr(r, "endpoint", None)
        # The /mcp mount's endpoint is an ASGI app object, not a
        # function; it is handled as a whole rather than per route.
        if fn is None or not hasattr(fn, "__name__"):
            continue
        mod = getattr(fn, "__module__", "") or ""
        mod = mod[4:] if mod.startswith("app.") else mod
        methods = ",".join(sorted((r.methods or set()) - {"HEAD", "OPTIONS"}))
        out.append((r.path, methods, f"{mod}:{fn.__name__}"))
    return sorted(set(out))


def ported_routes() -> set[tuple[str, str]]:
    """(METHOD, path) for every route Rust actually serves.

    Keyed on the method as well as the path, because a path can be
    split between the two stacks: `/api/mcp/keys` is a ported GET and a
    proxied POST, and matching the path alone reported the creation
    route — which mints a secret and fires a notification — as done.
    Both key-creation routes disappeared from this report that way.
    """
    rs = (REPO / "backend-rs/src/app.rs").read_text()
    out: set[tuple[str, str]] = set()
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,', rs):
        path = m.group(1)
        # Balance parentheses from `.route(` to find just this call.
        start = rs.rindex("(", 0, m.end())
        depth, j = 0, start
        while j < len(rs):
            if rs[j] == "(":
                depth += 1
            elif rs[j] == ")":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        body = rs[m.end():j]
        if "still_python" in body:
            continue
        # `ported(h)` is GET only; `served(...)` carries its verbs.
        if re.search(r"\bported\(", body):
            out.add(("GET", path))
        for verb in ("get", "post", "put", "patch", "delete"):
            if re.search(rf"(?:^|[^\w])(?:axum::routing::)?{verb}\(", body):
                out.add((verb.upper(), path))
    return out


def main() -> int:
    show_all = "--all" in sys.argv
    why = sys.argv[sys.argv.index("--why") + 1] if "--why" in sys.argv else None

    index = ModuleIndex()
    ported = ported_routes()
    groups: dict[tuple, list[tuple[str, str]]] = defaultdict(list)
    # Reasons that name state the port already owns. They are not
    # blockers — calling them that implies the Rust side does not exist
    # — but a route that reads one is reading a copy that is no longer
    # live, so they are subtracted here and reported separately.
    ported_reasons = {BLOCKERS.get(mod, mod) for mod in PORTED_STATE}
    ported_reasons |= {BLOCKING_SYMBOLS[sym] for sym in PORTED_SYMBOLS}
    reads_ported: dict[str, set[str]] = defaultdict(set)

    for path, methods, qual in routes():
        # A route counts as done only when every verb it answers is
        # served from Rust.
        verbs = [v for v in methods.split(",") if v]
        if verbs and all((v, path) in ported for v in verbs) and not show_all:
            continue
        found, chain = index.walk(qual)
        all_reasons = {why_ for _, why_ in found}
        stale = all_reasons & ported_reasons
        if stale:
            reads_ported[path] = stale
        key = tuple(sorted(all_reasons - ported_reasons))
        groups[key].append((path, methods))
        if why and path == why:
            print(f"{methods} {path}  ->  {qual}")
            for step in dict.fromkeys(chain):
                print(f"    via {step}")
            for prefix, reason in sorted(found):
                print(f"    BLOCKED BY app.{prefix} — {reason}")
            return 0

    if why:
        print(f"no route matches {why}", file=sys.stderr)
        return 1

    clear = groups.pop((), [])
    total = sum(len(v) for v in groups.values()) + len(clear)
    print(f"{total} route(s) considered\n")

    # Nothing blocks these, and none of them is a port: they are
    # FastAPI's own generated schema and the two pages that render it.
    # Reproducing that JSON from Rust would be re-deriving it from a
    # different framework's idea of the same routes, which is not the
    # same exercise as porting a handler and cannot be held to the
    # differential the way a handler can. They stay on the proxy while
    # the Python is there; when it goes, the question is what Rust
    # should generate, or whether to serve them at all.
    deliberate = [(p, m) for p, m in clear if p in NOT_A_PORT]
    clear = [(p, m) for p, m in clear if p not in NOT_A_PORT]

    # A route whose blockers are now all ported lands on the empty key,
    # which is `clear` — but it is not clear, it is overdue: the Python
    # copy of that state is no longer the live one.
    overdue = [(p, m) for p, m in clear if p in reads_ported]
    clear = [(p, m) for p, m in clear if p not in reads_ported]

    print(f"== clear: nothing in the way ({len(clear)}) ==")
    for path, methods in sorted(clear):
        print(f"   {methods:12} {path}")
    if deliberate:
        print(f"\n== not a port, by decision ({len(deliberate)}) ==")
        for path, methods in sorted(deliberate):
            print(f"   {methods:12} {path:28} {NOT_A_PORT[path]}")
    if overdue:
        print(f"\n== overdue: reads state the port already owns ({len(overdue)}) ==")
        for path, methods in sorted(overdue):
            print(f"   {methods:12} {path:44} {', '.join(sorted(reads_ported[path]))}")
    still_reading = sorted(
        (p, m) for paths in groups.values() for p, m in paths if p in reads_ported
    )
    if still_reading:
        print(f"\n== blocked, and also reads state the port owns ({len(still_reading)}) ==")
        for path, methods in still_reading:
            print(f"   {methods:12} {path:44} {', '.join(sorted(reads_ported[path]))}")

    for key in sorted(groups, key=lambda k: (-len(groups[k]), k)):
        print(f"\n== blocked by {', '.join(key)} ({len(groups[key])}) ==")
        for path, methods in sorted(groups[key]):
            print(f"   {methods:12} {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
