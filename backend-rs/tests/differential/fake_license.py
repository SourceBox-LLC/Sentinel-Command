#!/usr/bin/env python3
"""A fake Sentinel License Service: always valid, never surprising.

The self-hosted licence gate has four states, and three of them are only
reachable with `SENTINEL_LICENSE_KEY` set. Without it every Sentinel
route answers 402 `license_required`, so the licensed paths — the config
PATCH, the manual run — cannot be compared at all.

Setting the key has a cost: Python's licence reconcile loop does one
check-in at boot and another every fifteen minutes, writing
`sentinel_license_*` Settings on its own timer. The interval is a module
constant, so unlike the other loops it cannot be pushed out of the way.

This server removes the sting rather than the loop. It answers exactly
what the seeded licence state already says, so a tick that lands mid-run
rewrites the same values: `reachable=true`, `valid=true`,
`sync_enabled=false`, and two timestamps that the write differential
normalises to `<recent>` either way. `install_id` is seeded, so
`_get_or_create_install_id` returns it rather than minting a new one.

`/v1/licenses/check-in` is the only endpoint the client calls.

Usage: fake_license.py [--port 18090]
"""
from __future__ import annotations

import argparse
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VALID = True
SYNC_ENABLED = False


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        self.rfile.read(length)

        # A scenario prefix, for the check-in probe. The client appends
        # `/v1/licenses/check-in` to whatever SENTINEL_LICENSE_SERVICE_URL
        # says, so pointing it at `…:18090/scenario/revoked` arrives here
        # as `/scenario/revoked/v1/licenses/check-in`. Selecting by URL
        # rather than by a mode endpoint keeps the fake stateless: two
        # probes can hit it in either order and neither can leave the
        # other a surprise. The unprefixed path below is unchanged, so
        # the tiers keep the always-valid behaviour they were built on.
        if self.path.startswith("/scenario/"):
            name = self.path[len("/scenario/"):].split("/", 1)[0]
            self._scenario(name)
            return

        if not self.path.startswith("/v1/licenses/check-in"):
            self._send(404, {"detail": "no route"})
            return
        # The client reads `valid` and `sync_enabled`; `reason` is only
        # logged. An answer it cannot parse is treated as unreachable,
        # which would silently move the gate into its grace branch.
        self._send(200, {"valid": VALID, "sync_enabled": SYNC_ENABLED, "reason": "ok"})

    def _scenario(self, name: str):
        """The answers the check-in has to tell apart.

        Everything that is not a well-formed object with a truthy
        `valid` has to land in the same "unreachable" branch — that is
        the contract, and the reason each of these exists separately is
        that a port can easily treat one of them as an error instead.
        """
        if name == "valid":
            self._send(200, {"valid": True, "sync_enabled": False, "reason": "ok"})
        elif name == "valid-sync":
            self._send(200, {"valid": True, "sync_enabled": True, "reason": "ok"})
        elif name == "revoked":
            # Reachable and explicitly NO. Fails closed immediately, with
            # no grace — the gate's whole point.
            self._send(200, {"valid": False, "reason": "revoked"})
        elif name == "sync-without-valid":
            # A body claiming sync on an invalid licence. Sync must not
            # survive its licence.
            self._send(200, {"valid": False, "sync_enabled": True, "reason": "expired"})
        elif name == "truthy":
            # `bool(data.get("valid"))` is Python truthiness, not a type
            # check: a non-empty string is valid and an empty one is not.
            self._send(200, {"valid": "yes", "sync_enabled": 1, "reason": "ok"})
        elif name == "falsy":
            self._send(200, {"valid": "", "sync_enabled": True, "reason": "ok"})
        elif name == "not-an-object":
            # Parses as JSON, is not an answer.
            self._send(200, [1, 2, 3])
        elif name == "garbage":
            body = b"<html>not json</html>"
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif name == "server-error":
            self._send(500, {"detail": "boom"})
        elif name == "missing-valid":
            # No `valid` key at all: `.get` is None, which is falsy.
            self._send(200, {"reason": "ok"})
        else:
            self._send(404, {"detail": f"unknown scenario {name}"})

    def _send(self, status: int, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=18090)
    args = ap.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"fake license service on http://127.0.0.1:{args.port}", flush=True)
    server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
