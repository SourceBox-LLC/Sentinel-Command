"""Every route Rust serves must carry the rate limit Python gives it.

Porting a route to Rust removes its @limiter.limit decorator, because
the decorator is on the Python handler and Python never sees the request
once Rust owns the path. Slice 2 shipped five routes with their limits
silently dropped before this was noticed. This compares the two tables
directly so it cannot happen again quietly.

Usage: ratelimit_parity.py      (exits non-zero on a mismatch)
"""
import ast
import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
BACKEND_RS = HERE.parent.parent
BACKEND = BACKEND_RS.parent / "backend"


# WebSocket endpoints, which `@limiter.limit` cannot decorate: slowapi
# works on a Request, and an upgrade is not one. `/ws/node` throttles
# itself instead — a connect budget and a message budget, both in
# `app/api/ws.py` and both ported to `src/ws.rs`, neither visible to
# this comparison. Listed so the route is accounted for rather than
# reported as having no Python counterpart.
WEBSOCKET_ROUTES = {("GET", "/ws/node")}


def python_limits():
    """(METHOD, path) -> (n, window) or None."""
    out = {}
    for f in sorted((BACKEND / "app/api").glob("*.py")):
        src = f.read_text()
        m = re.search(r'APIRouter\((?:prefix="([^"]*)")?', src)
        prefix = m.group(1) if m and m.group(1) else ""
        for node in ast.walk(ast.parse(src)):
            if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            path = method = lim = None
            for d in node.decorator_list:
                seg = ast.get_source_segment(src, d) or ""
                rm = re.match(r'router\.(get|post|put|patch|delete)\(\s*"([^"]*)"', seg)
                if rm:
                    method, path = rm.group(1).upper(), rm.group(2)
                lm = re.search(r'limiter\.limit\(\s*"(\d+)/(minute|hour)"', seg)
                if lm:
                    lim = (int(lm.group(1)), lm.group(2))
            if path is not None:
                out[(method, prefix + path)] = lim
    return out


bodies = {}


def rust_handler_limits():
    """handler fn name -> (n, window) or None.

    Also records whether the handler ever calls `check()`. The
    extractor deliberately spends nothing — `RateLimit::check` goes
    where slowapi's decorator runs, after auth, so a 401 or a 422 does
    not cost the org its budget — which means a handler that takes the
    extractor and forgets the call has *no limit at all*. Reading the
    declaration alone scored exactly that as correct once, on the
    notification stream.
    """
    out = {}
    for f in sorted((BACKEND_RS / "src/api").glob("*.rs")):
        src = f.read_text()
        for m in re.finditer(r"pub async fn (\w+)\(", src):
            name = f"{f.stem}::{m.group(1)}"
            # take the argument list by balancing parentheses
            i = m.end() - 1
            depth, j = 0, i
            while j < len(src):
                if src[j] == "(":
                    depth += 1
                elif src[j] == ")":
                    depth -= 1
                    if depth == 0:
                        break
                j += 1
            args = src[i:j]
            # The handler body, for the `check()` scan below: from the
            # end of the signature to the start of the next item at
            # column zero.
            rest = src[j:]
            end = rest.find("\npub ")
            nxt = rest.find("\n#[cfg(test)]")
            if nxt != -1 and (end == -1 or nxt < end):
                end = nxt
            bodies[name] = rest if end == -1 else rest[:end]
            per_minute = re.search(r"PerMinute<(\d+)>", args)
            per_hour = re.search(r"PerHour<(\d+)>", args)
            raw = re.search(r"RateLimit<(\d+),\s*(\d+)>", args)
            if per_minute:
                out[name] = (int(per_minute.group(1)), "minute")
            elif per_hour:
                out[name] = (int(per_hour.group(1)), "hour")
            elif raw:
                secs = int(raw.group(2))
                out[name] = (int(raw.group(1)), "hour" if secs >= 3600 else "minute")
            else:
                out[name] = None
    return out


def unspent_limits(handlers):
    """Handlers that take a RateLimit and never call check() on it."""
    return sorted(
        name
        for name, limit in handlers.items()
        if limit is not None and ".check().await" not in bodies.get(name, "")
    )


def rust_routes():
    """[(METHOD, path, handler)] for routes Rust actually serves."""
    src = (BACKEND_RS / "src/app.rs").read_text()
    routes = []
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,', src):
        path = m.group(1)
        # balance parentheses from the .route( to find just this call
        i = src.rindex("(", 0, m.end())
        depth, j = 0, i
        while j < len(src):
            if src[j] == "(":
                depth += 1
            elif src[j] == ")":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        body = src[m.end():j]
        if "still_python" in body:
            continue
        ported = re.search(r"ported\(\s*(?:api::)?([\w:]+)", body)
        if ported:
            routes.append(("GET", path, "::".join(ported.group(1).split("::")[-2:])))
            continue
        for verb in ("get", "post", "put", "patch", "delete"):
            # Both `get(...)` and `axum::routing::delete(...)` are used in
            # the route table. An earlier version of this pattern excluded
            # ':' before the verb and so silently skipped every route
            # written the second way — four ported routes went unchecked
            # and the run still printed "0 mismatches".
            for h in re.finditer(
                rf"(?:^|[^\w])(?:axum::routing::)?{verb}\(\s*(?:api::)?([\w:]+)", body
            ):
                routes.append((verb.upper(), path, "::".join(h.group(1).split("::")[-2:])))
    return routes


def served_paths_in_app_rs():
    """Paths app.rs serves itself, i.e. every .route() that is not pinned."""
    src = (BACKEND_RS / "src/app.rs").read_text()
    out = []
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,', src):
        i = src.rindex("(", 0, m.end())
        depth, j = 0, i
        while j < len(src):
            if src[j] == "(":
                depth += 1
            elif src[j] == ")":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        if "still_python" not in src[m.end():j]:
            out.append(m.group(1))
    return out


def main():
    py = python_limits()
    handlers = rust_handler_limits()
    routes = rust_routes()

    # A parser that silently misses routes reports a clean run, which is
    # how the axum::routing:: form went unchecked. Cross-check the *set*
    # of served paths against app.rs rather than a count — counting was
    # itself wrong first time round, because `still_python()` appears in
    # the function's own definition as well as at every call site.
    expected = set(served_paths_in_app_rs())
    seen = {path for _, path, _ in routes}
    if seen != expected:
        missing, extra = sorted(expected - seen), sorted(seen - expected)
        print("PARSER GAP: this script's view of app.rs is incomplete.")
        for m in missing:
            print(f"  not parsed: {m}")
        for e in extra:
            print(f"  not in app.rs: {e}")
        print("Fix the parser before trusting the result.")
        return 2

    print(f"python declares {len(py)} routes; rust serves {len(routes)} "
          f"method+path pairs across {len(seen)} paths\n")
    bad = 0
    for name in unspent_limits(handlers):
        print(f"  FAIL  {name}: takes a RateLimit and never calls check() "
              f"— the limit is declared but not in force")
        bad += 1
    for method, path, handler in sorted(set(routes)):
        if handler == "health":
            continue
        if (method, path) in WEBSOCKET_ROUTES:
            print(f"  ok    {method:<6} {path:<46} websocket — throttled in ws.rs, not slowapi")
            continue
        if (method, path) not in py:
            print(f"  FAIL  {method:<6} {path:<46} no matching python route")
            bad += 1
            continue
        want, got = py[(method, path)], handlers.get(handler)
        if want is None:
            if got is not None:
                print(f"  FAIL  {method:<6} {path:<46} python has no limit, "
                      f"rust applies {got[0]}/{got[1]}")
                bad += 1
            else:
                print(f"  ok    {method:<6} {path:<46} unlimited, as in python")
            continue
        # The window is compared as well as the number: a 30/hour route
        # ported with a minute window is sixty times the budget, and the
        # count alone would look correct.
        if got != want:
            shown = "none" if got is None else f"{got[0]}/{got[1]}"
            print(f"  FAIL  {method:<6} {path:<46} python={want[0]}/{want[1]} rust={shown}")
            bad += 1
        else:
            print(f"  ok    {method:<6} {path:<46} {want[0]}/{want[1]}")

    print(f"\n{bad} mismatch(es)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
