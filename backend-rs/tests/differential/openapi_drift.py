#!/usr/bin/env python3
"""Hold the harvested OpenAPI document to the routes Rust actually serves.

`assets/openapi.json` is FastAPI's generated document, captured from the
running Python at port time and compiled into the Rust binary. That is
the honest option — FastAPI derives the schema from its own route table
and Pydantic models, so there is nothing to port faithfully and a
hand-written document would be a different, worse one at the same URL.

The cost of a snapshot is drift: it cannot notice a route being added,
renamed or removed. This is what makes it accountable. It compares the
document's paths against `app.rs`'s route table and fails on either side
holding something the other does not.

Same shape as `route_capture.py`, and for the same reason: a checker that
reads the code rather than a list someone has to remember to update.

Path spellings differ in one respect that is not drift. FastAPI writes a
parameter as `{camera_id}`; axum writes `{camera_id}` too, so they agree
— but FastAPI documents a router mounted at a prefix with its trailing
slash (`/api/incidents/`) where axum registers `/api/incidents`. Those
are normalised, once, here.

Usage: openapi_drift.py
"""
from __future__ import annotations

import json
import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
RS = HERE.parent.parent
DOCUMENT = RS / "assets" / "openapi.json"
APP_RS = RS / "src" / "app.rs"

# Paths the document carries that Rust deliberately does not serve, each
# with the reason. An entry here is a claim, so it has to earn its place.
EXPECTED_ABSENT = {
    # Python declares it; the SPA middleware shadows it and serves the
    # React document instead, so it is unreachable there too. Rust
    # matches by having no route in the way. See api/docs.rs.
    "/docs/oauth2-redirect": "shadowed by the SPA on both stacks",
}

# Routes Rust serves that the document does not describe, each with the
# reason. The docs surface itself is the obvious one: FastAPI does not
# document its own documentation routes.
EXPECTED_UNDOCUMENTED = {
    "/api-docs": "FastAPI does not document its own docs routes",
    "/api-redoc": "same",
    "/api/openapi.json": "same",
    "/mcp": "a mounted ASGI app in Python, so it has no FastAPI route to document",
    "/mcp/": "same",
    "/ws/node": "a WebSocket; FastAPI documents no schema for one",
    "/favicon.svg": "a static file",
    "/assets": "a static mount",
}


def document_paths() -> set[str]:
    if not DOCUMENT.is_file():
        print(f"REFUSING: {DOCUMENT} is missing — nothing to compare", file=sys.stderr)
        raise SystemExit(2)
    data = json.loads(DOCUMENT.read_text())
    paths = data.get("paths")
    if not isinstance(paths, dict) or not paths:
        print("REFUSING: the document has no paths — a truncated harvest",
              file=sys.stderr)
        raise SystemExit(2)
    return {normalise(p) for p in paths}


def served_paths() -> set[str]:
    """Every path `app.rs` registers, however it registers it.

    Read out of the route table rather than listed here, so a newly
    ported route is compared the day it lands. Parses `.route(...)`,
    `.nest_service(...)` and `.route_service(...)` — the last two are how
    the static assets are mounted and they would otherwise look like
    routes the document is missing.
    """
    src = APP_RS.read_text()
    found = set()
    for pattern in (
        r'\.route\(\s*"([^"]+)"',
        r'\.nest_service\(\s*"([^"]+)"',
        r'\.route_service\(\s*"([^"]+)"',
    ):
        found.update(normalise(m.group(1)) for m in re.finditer(pattern, src))
    if not found:
        print("REFUSING: parsed no routes out of app.rs — the pattern has rotted",
              file=sys.stderr)
        raise SystemExit(2)
    return found


def normalise(path: str) -> str:
    """A trailing slash on a prefix-mounted router is not drift."""
    if len(path) > 1 and path.endswith("/") and path != "/mcp/":
        return path[:-1]
    return path


def main() -> int:
    documented = document_paths()
    served = served_paths()

    absent = {p for p in documented - served if p not in EXPECTED_ABSENT}
    undocumented = {p for p in served - documented if p not in EXPECTED_UNDOCUMENTED}

    print(f"document describes {len(documented)} path(s); app.rs serves {len(served)}")

    bad = 0
    if absent:
        bad += 1
        print(f"\nFAIL  {len(absent)} documented path(s) Rust does not serve:")
        for path in sorted(absent):
            print(f"        {path}")
        print("      Either the route was dropped and the document is stale, or it")
        print("      was renamed. Re-harvest, or add it to EXPECTED_ABSENT with a reason.")
    if undocumented:
        bad += 1
        print(f"\nFAIL  {len(undocumented)} served path(s) the document does not describe:")
        for path in sorted(undocumented):
            print(f"        {path}")
        print("      A route added since the harvest. `/api-docs` will not show it,")
        print("      which is how a documented API quietly stops being documented.")

    # An exemption that has stopped applying is as misleading as a missing
    # one: it reads as a considered decision when it is a leftover.
    for path, why in EXPECTED_ABSENT.items():
        if path in served:
            bad += 1
            print(f"\nFAIL  {path} is listed as deliberately absent ({why})")
            print("      but app.rs serves it now. Remove the exemption.")
    for path, why in EXPECTED_UNDOCUMENTED.items():
        if path in documented:
            bad += 1
            print(f"\nFAIL  {path} is listed as undocumented ({why})")
            print("      but the document describes it now. Remove the exemption.")

    if not bad:
        print("the document and the route table agree")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
