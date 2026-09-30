#!/usr/bin/env python3
"""A fake Sentinel-Sync-Service that records every push.

The data mirror's behaviour is almost entirely in what it SENDS: which
tables, in what order, which columns, which rows since which cursor, and
whether a full id snapshot rides along. None of that is visible in the
local database afterwards except the cursors — so unlike the other
fakes, this one exists to be read back rather than to answer.

`GET /__pushes` returns everything received since the last `__reset`, in
arrival order. The probes diff that, which is what makes the column
denylist testable at all: `api_key_hash` and the evidence blob not
appearing is a property of the request body and of nothing else.

`/__fail` makes the next push for one named table return 500, so the
"one table's failure must not block the others" contract has something
to fail against. Failing by table rather than by call count keeps it
deterministic regardless of how many batches a table takes.

Usage: fake_sync.py [--port 18091]
"""
from __future__ import annotations

import argparse
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

_lock = threading.Lock()
PUSHES: list[dict] = []
FAIL_TABLES: set[str] = set()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def _body(self) -> dict:
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length)
        try:
            return json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            return {}

    def _send(self, status: int, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        if self.path.startswith("/__reset"):
            self._body()
            with _lock:
                PUSHES.clear()
                FAIL_TABLES.clear()
            self._send(200, {"ok": True})
            return
        if self.path.startswith("/__fail"):
            spec = self._body()
            with _lock:
                FAIL_TABLES.clear()
                FAIL_TABLES.update(spec.get("tables") or [])
            self._send(200, {"ok": True, "tables": sorted(FAIL_TABLES)})
            return
        if not self.path.startswith("/v1/sync/push"):
            self._body()
            self._send(404, {"detail": "no route"})
            return

        payload = self._body()
        table = payload.get("table")
        with _lock:
            PUSHES.append({
                "table": table,
                # The bearer is the licence key, and that it is SENT is
                # the behaviour — the value is a fixture constant.
                "authorized": bool(self.headers.get("Authorization")),
                "row_count": len(payload.get("rows") or []),
                # Recorded whole so the probes can diff the column set
                # and the values. Kept as-is rather than summarised:
                # the denylist is only observable here.
                "rows": payload.get("rows") or [],
                "known_ids": payload.get("known_ids"),
            })
            failing = table in FAIL_TABLES
        if failing:
            self._send(500, {"detail": f"scripted failure for {table}"})
            return
        self._send(200, {"ok": True, "accepted": len(payload.get("rows") or [])})

    def do_GET(self):
        if self.path.startswith("/__pushes"):
            with _lock:
                self._send(200, list(PUSHES))
            return

        # The READ half, for `sentinel-restore-from-cloud`. Served from
        # whatever was pushed, so a round-trip test is a real one: the
        # rows the restore pulls are the rows the sync sent, not a
        # separate fixture that could agree with neither.
        if self.path.split("?")[0] == "/v1/sync/tables":
            with _lock:
                by_table = {}
                for push in PUSHES:
                    by_table.setdefault(push["table"], 0)
                    by_table[push["table"]] += push["row_count"]
            self._send(200, {"tables": [
                {"table": t, "rows": n, "deleted": 0} for t, n in sorted(by_table.items())
            ]})
            return

        if self.path.split("?")[0] == "/v1/sync/rows":
            from urllib.parse import parse_qs, urlparse
            params = parse_qs(urlparse(self.path).query)
            table = (params.get("table") or [""])[0]
            with _lock:
                rows = [r for push in PUSHES if push["table"] == table
                        for r in (push["rows"] or [])]
            # Unpaginated: the fixture is small, and a `next_cursor` the
            # client must follow is the one thing a single page cannot
            # exercise. The restore's paging loop is still driven, because
            # it asks for a cursor and gets none.
            self._send(200, {"rows": rows, "next_cursor": None})
            return

        self._send(404, {"detail": "no route"})


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=18091)
    args = ap.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"fake sync service on http://127.0.0.1:{args.port}", flush=True)
    server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
