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
}

# Modules only PART of which has a Rust equivalent. Importing one of the
# listed names does not block a route; importing anything else from the
# module still does. Module granularity alone would have to choose
# between hiding `enforce_camera_cap` (unported) and flagging every
# caller of `effective_plan_for_caps` (ported, and verified by
# plan_run.sh).
PORTED_FUNCTIONS = {
    "core.plans": {
        "effective_plan_for_caps", "resolve_org_plan", "get_plan_limits",
        "get_plan_display_name", "invalidate_effective_plan_cache",
        "PAID_PLAN_SLUGS", "PAYMENT_GRACE_DAYS", "PLAN_LIMITS",
    },
    "core.license_client": {
        "is_sentinel_licensed", "is_sync_enabled", "sentinel_blocked_by_license",
        "SENTINEL_LICENSE_GRACE_HOURS",
    },
}

# A module whose *import alone* means the route cannot move yet, with the
# reason. Keys are module prefixes under `app.`.
BLOCKERS = {
    "core.plans": "plan-cache + Clerk billing API",
    "core.clerk": "Clerk Backend API",
    "core.email": "Resend + the outbox worker",
    "core.email_templates": "Resend + the outbox worker",
    "core.email_unsubscribe": "Resend + the outbox worker",
    "core.email_worker": "Resend + the outbox worker",
    "core.recipients": "Clerk org membership lookup",
    "core.license_client": "Sentinel License Service client",
    "core.release_cache": "in-process GitHub release cache",
    "core.health_probes": "Clerk + license probes",
    "core.integration_auth": "Home Assistant integration key",
    "core.sync_client": "Sentinel Sync Service client",
    "core.sentinel_dispatch": "sentinel dispatch (DB-backed, portable)",
    "core.versions": "node version check (reads release cache)",
    "api.hls": "in-process HLS segment + playlist caches",
    "api.ws": "in-process WebSocket connection manager",
    "mcp": "MCP server (fastmcp -> rmcp)",
}

# Unported, but nothing stands in the way — plain logic or DB work that
# simply has not been written yet. Listing these separately keeps
# "blocked" meaning blocked.
JUST_WORK = {
    "core.sentinel_dispatch": "sentinel dispatch (DB-backed)",
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

# Standard-library imports that still need a Rust counterpart chosen.
STDLIB_BLOCKERS = {"zoneinfo": "tzdata / chrono-tz"}


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


def ported_paths() -> set[str]:
    rs = (REPO / "backend-rs/src/app.rs").read_text()
    return set(re.findall(r'"([^"]+)"\s*,\s*\n?\s*(?:ported|served)\(', rs))


def main() -> int:
    show_all = "--all" in sys.argv
    why = sys.argv[sys.argv.index("--why") + 1] if "--why" in sys.argv else None

    index = ModuleIndex()
    ported = ported_paths()
    groups: dict[tuple, list[tuple[str, str]]] = defaultdict(list)

    for path, methods, qual in routes():
        if path in ported and not show_all:
            continue
        found, chain = index.walk(qual)
        key = tuple(sorted(why_ for _, why_ in found))
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
    print(f"== clear: nothing in the way ({len(clear)}) ==")
    for path, methods in sorted(clear):
        print(f"   {methods:12} {path}")
    for key in sorted(groups, key=lambda k: (-len(groups[k]), k)):
        print(f"\n== blocked by {', '.join(key)} ({len(groups[key])}) ==")
        for path, methods in sorted(groups[key]):
            print(f"   {methods:12} {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
