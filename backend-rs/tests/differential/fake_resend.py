#!/usr/bin/env python3
"""A fake Resend, driven by the recipient address.

The email worker's whole job is deciding what to do with what Resend
answers, and there are eight distinct answers — each mapping to a
different exception class inside the SDK, whose NAME ends up in the
`error` column of both `email_outbox` and `email_log`. That string is
compared character for character by the differential, so the fake has
to be able to produce every one of them.

The mode is taken from the recipient's local part, so a case chooses
its own outcome by addressing the mail:

    ok@…          200 {"id": …}            a normal send
    noid@…        200 {}                   no message id to correlate with
    validation@…  422 validation_error     ValidationError
    apperror@…    500 application_error    ApplicationError
    rate@…        429 rate_limit_exceeded  RateLimitError
    badkey@…      401 invalid_api_key      ResendError (401 is not in the
                                           SDK's table; invalid_api_key is
                                           mapped under 403 only)
    empty@…       200 with an empty body   ApplicationError: Failed to decode…
    html@…        502 text/html            ResendError: Expected JSON response…
    flaky@…       500 once, then 200       the retry path: a row that fails
                                           and succeeds later must log once

The returned id is derived from the Idempotency-Key rather than random,
so both tiers get the same `resend_message_id` and the differential can
compare it directly instead of normalising it away. That also makes the
fake genuinely idempotent: the same key twice gets the same id, which is
what the real one does and what the reclaim path depends on.

Usage: fake_resend.py [--port 18095]
"""
from __future__ import annotations

import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Addresses seen before, for the flaky mode. Keyed by idempotency key so
# a retry of the SAME row is what flips it, not a second row.
_seen: set[str] = set()


def _mode(payload: dict) -> str:
    to = payload.get("to")
    if isinstance(to, list):
        to = to[0] if to else ""
    if not isinstance(to, str):
        return "ok"
    return to.split("@", 1)[0].lower()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            payload = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            payload = {}
        key = self.headers.get("Idempotency-Key", "")
        mode = _mode(payload)

        if mode == "flaky":
            # First attempt fails, every later one succeeds.
            if key not in _seen:
                _seen.add(key)
                mode = "apperror"
            else:
                mode = "ok"

        body, code, ctype = self._answer(mode, key)
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _answer(self, mode: str, key: str) -> tuple[bytes, int, str]:
        js = "application/json"
        if mode == "noid":
            return json.dumps({}).encode(), 200, js
        if mode == "validation":
            return json.dumps({"statusCode": 422, "name": "validation_error",
                               "message": "bad address"}).encode(), 422, js
        if mode == "apperror":
            return json.dumps({"statusCode": 500, "name": "application_error",
                               "message": "boom"}).encode(), 500, js
        if mode == "rate":
            return json.dumps({"statusCode": 429, "name": "rate_limit_exceeded",
                               "message": "slow down"}).encode(), 429, js
        if mode == "badkey":
            return json.dumps({"statusCode": 401, "name": "invalid_api_key",
                               "message": "nope"}).encode(), 401, js
        if mode == "empty":
            return b"", 200, js
        if mode == "html":
            return b"<html>nope</html>", 502, "text/html"
        # Deterministic from the key, so the two tiers agree and a
        # retry of one row gets the id its first attempt got.
        return json.dumps({"id": f"msg_{key or 'nokey'}"}).encode(), 200, js

    def do_GET(self):  # noqa: N802
        # `/__reset` forgets which idempotency keys have been seen.
        #
        # The runner calls it before EACH probe, and it has to: the two
        # probes share one fake, and `flaky` fails a key's first attempt
        # only. Without a reset the Python probe consumed that failure
        # and the Rust probe saw a success on its first try — which
        # reads as the two workers disagreeing about retries when they
        # agree completely.
        if self.path == "/__reset":
            _seen.clear()
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=18095)
    args = ap.parse_args()
    ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
