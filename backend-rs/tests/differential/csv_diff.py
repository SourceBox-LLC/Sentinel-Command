#!/usr/bin/env python3
"""Diff the three `?format=csv` exports, byte for byte.

These were the last routes behind the proxy and the only ones ported
*after* the Python was deleted — so they are the one slice that had no
running reference while it was written. `csv_run.sh` gets one back by
serving the Python from a worktree of the commit before the cut, which
makes this the same harness as every other slice, just with the reference
fetched out of history.

What is compared, per case:

  * the status code;
  * `Content-Type`, `Content-Disposition` and `Cache-Control` — the
    download headers are the part a browser acts on, and a filename that
    differs means an auditor's file lands with the wrong name;
  * the body, **exactly**. Not parsed and re-serialised: the whole risk
    in a CSV port is quoting and line endings, and a comparison that
    parses both sides would agree on two files that no spreadsheet reads
    the same way. CRLF vs LF is invisible to a CSV reader and obvious
    here.

Content-Length is deliberately NOT compared: both stacks stream, so
neither sends one, and `Transfer-Encoding` framing is a property of the
server rather than of the export.

Usage: csv_diff.py <admin-token> <member-token> [-v]
"""
from __future__ import annotations

import sys
import urllib.error
import urllib.request

# The ports come from the environment because csv_run.sh puts this pair
# on 8010/8011: a differential session already holding 8000/8001 must not
# be torn down to run this one.
import os

RUST = os.environ.get("CSV_RUST", "http://127.0.0.1:8000")
PYTHON = os.environ.get("CSV_PYTHON", "http://127.0.0.1:8001")

#: Headers that are part of the download's meaning.
COMPARED_HEADERS = ("content-type", "content-disposition", "cache-control")

AUDIT = "/api/audit-logs"
STREAM = "/api/audit/stream-logs"
MCP = "/api/mcp/activity/logs"

#: (method, path, use_admin_token, extra headers)
CASES: list[tuple[str, str, bool]] = [
    # ── the happy paths, one per route ──────────────────────────────
    ("GET", f"{AUDIT}?format=csv", True),
    ("GET", f"{STREAM}?format=csv", True),
    ("GET", f"{MCP}?format=csv", True),

    # ── filters apply to CSV as they do to JSON ─────────────────────
    ("GET", f"{AUDIT}?format=csv&event=login", True),
    ("GET", f"{AUDIT}?format=csv&event=nosuchevent", True),          # header only
    ("GET", f"{STREAM}?format=csv&camera_id=cam-1", True),
    ("GET", f"{STREAM}?format=csv&camera_id=nosuchcam", True),       # header only
    ("GET", f"{MCP}?format=csv&tool_name=list_cameras", True),
    ("GET", f"{MCP}?format=csv&status=error", True),
    ("GET", f"{MCP}?format=csv&tool_name=list_cameras&status=error", True),

    # ── LIKE escaping, which differs BY ROUTE and must keep differing ──
    # /api/audit-logs and /api/mcp/activity/logs escape the caller's
    # wildcards; /api/audit/stream-logs does not. Copying that
    # inconsistency is the only way the ported routes return the same
    # rows, so each side of it is a case.
    ("GET", f"{AUDIT}?format=csv&username=%25", True),      # literal %
    ("GET", f"{AUDIT}?format=csv&username=_", True),        # literal _
    ("GET", f"{AUDIT}?format=csv&username=h%C3%A9llo", True),
    ("GET", f"{MCP}?format=csv&key_name=ci_robot", True),   # escaped _
    ("GET", f"{MCP}?format=csv&key_name=_", True),          # literal _
    ("GET", f"{STREAM}?format=csv&user_id=_", True),        # wildcard _
    ("GET", f"{STREAM}?format=csv&user_id=%25", True),      # wildcard %

    # ── the pagination parameters are VALIDATED and then ignored ────
    # CSV takes a flat 50,000-row window, so `limit` changes nothing
    # about the body — but an out-of-range one is still a 422, because
    # validation runs before the branch.
    ("GET", f"{AUDIT}?format=csv&limit=1", True),
    ("GET", f"{AUDIT}?format=csv&limit=1&offset=5", True),
    ("GET", f"{AUDIT}?format=csv&limit=0", True),           # 422
    ("GET", f"{AUDIT}?format=csv&limit=501", True),         # 422
    ("GET", f"{AUDIT}?format=csv&offset=-1", True),         # 422
    ("GET", f"{MCP}?format=csv&limit=abc", True),           # 422

    # ── the format parameter itself ─────────────────────────────────
    ("GET", f"{AUDIT}?format=CSV", True),                   # 422: case-sensitive
    ("GET", f"{AUDIT}?format=xml", True),                   # 422
    ("GET", f"{AUDIT}?format=", True),                      # 422
    ("GET", f"{AUDIT}?format=csv&format=json", True),       # duplicate param
    ("GET", f"{AUDIT}?format=json&format=csv", True),
    ("GET", AUDIT, True),                                   # default is json

    # ── auth, on a route that hands out an org's whole audit trail ──
    ("GET", f"{AUDIT}?format=csv", False),                  # member → 403
    ("GET", f"{STREAM}?format=csv", False),
    ("GET", f"{MCP}?format=csv", False),
    ("GET", f"{AUDIT}?format=csv", None),                   # no token → 401/403

    # ── methods ────────────────────────────────────────────────────
    ("POST", f"{AUDIT}?format=csv", True),                  # 405
    ("HEAD", f"{AUDIT}?format=csv", True),                  # 405, not a body
]


def fetch(base: str, method: str, path: str, token: str | None):
    request = urllib.request.Request(base + path, method=method)
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.status, dict(response.headers), response.read()
    except urllib.error.HTTPError as err:
        return err.code, dict(err.headers), err.read()
    except Exception as err:  # noqa: BLE001
        return None, {"error": str(err)}, b""


def headers_of(raw: dict) -> dict[str, str]:
    lowered = {k.lower(): v for k, v in raw.items()}
    return {name: lowered.get(name, "<absent>") for name in COMPARED_HEADERS}


def show(label: str, value: bytes) -> str:
    """A body rendered so CRLF and a stray apostrophe are both visible."""
    text = value.decode("utf-8", "replace")
    if len(text) > 1200:
        text = text[:1200] + f"… ({len(value)} bytes)"
    return f"{label}: {text!r}"


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    admin, member = sys.argv[1], sys.argv[2]
    verbose = "-v" in sys.argv[3:]

    same = 0
    differences: list[str] = []

    for method, path, which in CASES:
        token = {True: admin, False: member, None: None}[which]
        role = {True: "admin", False: "member", None: "anon"}[which]

        py_status, py_headers, py_body = fetch(PYTHON, method, path, token)
        rs_status, rs_headers, rs_body = fetch(RUST, method, path, token)

        problems = []
        if py_status != rs_status:
            problems.append(f"status: python {py_status}, rust {rs_status}")

        py_h, rs_h = headers_of(py_headers), headers_of(rs_headers)
        for name in COMPARED_HEADERS:
            if py_h[name] != rs_h[name]:
                problems.append(f"{name}: python {py_h[name]!r}, rust {rs_h[name]!r}")

        if py_body != rs_body:
            problems.append("body differs")
            problems.append("  " + show("python", py_body))
            problems.append("  " + show("rust  ", rs_body))

        if problems:
            differences.append(f"\n{method} {path}  [{role}]")
            differences.extend("    " + line for line in problems)
        else:
            same += 1
            if verbose:
                head = py_body.split(b"\r\n")[0].decode("utf-8", "replace")
                print(
                    f"  ok  {method} {path} [{role}] "
                    f"{py_status} {len(py_body)}B  {head[:60]!r}"
                )

    # An export that returns nothing at all would pass every byte
    # comparison above, so the corpus has to be proved non-trivial.
    status, _, body = fetch(PYTHON, "GET", f"{AUDIT}?format=csv", admin)
    rows = body.count(b"\r\n") - 1 if body else 0
    print(f"\ncorpus: the audit export carries {rows} data row(s) at status {status}")
    if rows < 8:
        print(
            "REFUSING: fewer rows than csv_fixture.sql seeds for this org — the "
            "fixture did not apply, and every case above compared two empty "
            "exports.",
            file=sys.stderr,
        )
        return 2
    # Tenant isolation, asserted rather than assumed: the fixture seeds a
    # row for another org in each table.
    if b"must not appear" in body or b"intruder" in body:
        print("REFUSING: another org's row is in the export", file=sys.stderr)
        return 2

    # And the corpus has to have exercised what it was built for. Two
    # identical bodies prove nothing if neither contains a quote, a CRLF
    # or a formula leader — the three things this port could plausibly
    # get wrong. Each probe below is a byte sequence only the intended
    # rule produces.
    _, _, mcp_body = fetch(PYTHON, "GET", f"{MCP}?format=csv", admin)
    probes = {
        # A defanged formula leader, inside quotes because the value also
        # contains a comma: apostrophe first, then quoting.
        "defanged =": (b'"\'=SUM(A1,A2)"', body),
        # A doubled quote — `""` inside a quoted field. Probed on the
        # JSON key rather than on the inner `\"hi\"`, whose backslashes
        # are part of the stored value and would make the expected bytes
        # a puzzle rather than an assertion.
        "doubled quote": (b'""name""', body),
        # A field holding a real newline, which forces quotes and puts a
        # bare LF inside a CRLF-terminated record.
        "embedded LF": (b'"two\nlines"', body),
        # A bare CR inside a quoted field.
        "embedded CR": (b"\r", body),
        # Non-ASCII, so the body is bytes and not characters.
        "non-ascii": ("ünicode".encode(), body),
        # A NEGATIVE integer column, NOT defanged. `,-5,` can only appear
        # if the cell was written as a number.
        "bare -5": (b",-5,", mcp_body),
        # The same value as text, which IS defanged.
        "defanged -5": (b"'-5", body),
        # An empty field for a NULL, rather than a quoted empty string.
        "null as bare empty": (b",,", body),
    }
    missing = [name for name, (probe, target) in probes.items() if probe not in target]
    if missing:
        print(
            "REFUSING: the corpus did not exercise " + ", ".join(missing) +
            " — the fixture drifted and these cases agree about nothing.",
            file=sys.stderr,
        )
        return 2
    print(f"corpus: all {len(probes)} quoting/defanging probes present in the bytes")

    # The summary line's shape is the harness convention, and mutate.py
    # parses it: "N/M identical, K differing". A run that prints anything
    # else scores as "no result" rather than as a catch.
    print(f"{same}/{len(CASES)} identical, {len(CASES) - same} differing")
    if differences:
        print(f"\n{len(differences)} difference report(s):")
        for line in differences:
            print(line)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
