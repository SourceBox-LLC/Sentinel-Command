"""Catch ported `{param}` routes that swallow their static siblings.

A route registered as `/api/nodes/{node_id}` also matches
`/api/nodes/plan`, with `node_id = "plan"`. FastAPI is saved from this by
declaration order — `/plan` is declared above `/{node_id}` in the same
router — but axum has no ordering between separately registered paths.
So porting a parameterised route silently takes over every literal path
beside it and answers 404.

That is not hypothetical: porting `GET /api/nodes/{node_id}` captured
`/api/nodes/plan`, `/ws-status`, `/validate`, `/register` and
`/heartbeat` on the first run. The fix is to pin each static sibling to
the proxy in `app.rs`; this script proves none has been missed.

Reads the authoritative path list out of the live FastAPI app's OpenAPI
schema. An earlier version of this check walked `app.routes` instead and
reported a clean "(none)" — this FastAPI version nests them under
`_IncludedRouter` wrappers whose `path` is None, so the filter silently
dropped every real route. A check that cannot fail is worse than no
check.

Usage: route_capture.py            (exits non-zero on an unpinned path)
"""

import re
import os
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
BACKEND_RS = HERE.parent.parent
BACKEND = BACKEND_RS.parent / "backend"

sys.path.insert(0, str(BACKEND))

# Importing app.main builds the SQLAlchemy engine at module scope, and
# app/core/config.py defaults DATABASE_URL to "sqlite:///./sentinel.db"
# — a *relative* path, so running this from backend-rs/ silently
# created a 380 KB SQLite file with the full production schema in the
# Rust crate root, where `git add -A` duly picked it up. Nothing here
# touches the database; pointing it somewhere disposable is enough.
os.environ.setdefault("DATABASE_URL", "sqlite:///" + tempfile.gettempdir() + "/cc-route-capture.db")

from app.main import app  # noqa: E402


def rust_routes():
    """Pull the route table out of app.rs.

    Parsing the source rather than asking the binary keeps this a plain
    script; the route table is a literal list of `.route("...", ...)`
    calls, so it reads cleanly.
    """
    src = (BACKEND_RS / "src" / "app.rs").read_text()
    ported, pinned = [], set()
    for path, handler in re.findall(r'\.route\(\s*"([^"]+)"\s*,\s*(\w+)', src):
        if handler == "still_python":
            pinned.add(path)
        else:
            ported.append(path)
    return ported, pinned


def main():
    paths = sorted(app.openapi()["paths"].keys())
    ported, pinned = rust_routes()
    params = [p for p in ported if "{" in p]

    print(f"{len(paths)} python paths, {len(ported)} ported, {len(pinned)} pinned to proxy")
    if not params:
        print("no parameterised routes ported yet — nothing to capture")
        return 0

    unpinned = []
    for pat in params:
        prefix = pat.rsplit("/", 1)[0]
        for p in paths:
            if not p.startswith(prefix + "/"):
                continue
            rest = p[len(prefix) + 1:]
            # exactly one more segment, and a literal one
            if "/" in rest or not rest or "{" in rest:
                continue
            if p in pinned or p in ported:
                print(f"  ok   {pat} captures {p} — pinned")
            else:
                print(f"  FAIL {pat} captures {p} — NOT pinned, Rust will 404 it")
                unpinned.append(p)

    if unpinned:
        print(f"\n{len(unpinned)} path(s) would be swallowed. Add "
              f'`.route("<path>", still_python())` to app.rs for each.')
        return 1
    print("\nno static sibling is swallowed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
