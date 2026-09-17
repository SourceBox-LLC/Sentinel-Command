#!/usr/bin/env python3
"""A fake Clerk Backend API, just enough for the billing entitlement path.

Exists because `resolve_org_plan` opens with

    if settings.is_local_auth():
        return "self_host"

and both differential tiers run AUTH_PROVIDER=local. Every plan lookup
in every harness here therefore returns one constant, and the entire
entitlement path — the Setting fast path, the throttled live lookup,
the 30-second effective-plan cache, the seven-day past-due grace — has
never executed under test. The ~20 routes blocked on `core.plans`
cannot be ported against a harness that cannot see them.

Serves `GET /v1/organizations/{org_id}/billing/subscription` with
scripted payloads, shaped so the real Clerk SDK deserialises them: a
model validation error inside `fetch_live_plan_slug` is swallowed by
its blanket `except` and returns None, which reads as "Clerk was
unreachable" — a fake that fails this way makes the harness quietly
vacuous, which is the failure mode this whole directory exists to
avoid. `/__scenario` sets what the next lookup returns.

Usage:
    fake_clerk.py [--port 18080]
"""
from __future__ import annotations

import argparse
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MS = 1000

# org_id -> scenario name. Set over HTTP so the Rust probe and the
# Python probe drive the same server.
SCENARIOS: dict[str, str] = {}
CALLS: dict[str, int] = {}
_lock = threading.Lock()


def _item(status: str, slug: str | None, *, period_end_ms: int | None = None,
          plan_present: bool = True) -> dict:
    """One subscription item, with every field the SDK requires."""
    now_ms = int(time.time() * MS)
    item = {
        "object": "commerce_subscription_item",
        "id": f"si_{status}_{slug or 'none'}",
        "instance_id": "ins_test",
        "status": status,
        "plan_id": f"plan_{slug or 'none'}",
        "plan_period": "month",
        "payer_id": "org_test",
        "is_free_trial": False,
        "period_start": now_ms - 30 * 24 * 3600 * MS,
        "period_end": period_end_ms,
        "canceled_at": now_ms if status == "canceled" else None,
        "past_due_at": None,
        "ended_at": None,
    }
    if plan_present:
        # Every field `clerk_backend_api.models.commercesubscriptionitem.Plan`
        # marks required, or the whole subscription fails to
        # deserialise — and `fetch_live_plan_slug` swallows that in its
        # blanket `except` and returns None, which reads as "Clerk was
        # unreachable". A fake that is wrong in that direction turns
        # every case green while testing nothing, so the shape is taken
        # from the model rather than guessed.
        #
        # Note `slug` is a required, non-nullable `str`: the Python's
        # `if not slug: continue` guard is reachable with an EMPTY slug,
        # not a missing one.
        fee = {"amount": 1200, "amount_formatted": "12.00",
               "currency": "USD", "currency_symbol": "$"}
        item["plan"] = {
            "object": "commerce_plan",
            "id": f"plan_{slug or 'none'}",
            "name": slug or "Unnamed",
            "slug": slug if slug is not None else "",
            "description": "",
            "instance_id": "ins_test",
            "product_id": "prod_test",
            "is_default": False,
            "is_recurring": True,
            "has_base_fee": True,
            "publicly_visible": True,
            "fee": fee,
            "annual_fee": {**fee, "amount": 12000, "amount_formatted": "120.00"},
            "annual_monthly_fee": {**fee, "amount": 1000, "amount_formatted": "10.00"},
            "for_payer_type": "org",
            "avatar_url": "",
            "free_trial_enabled": False,
            "free_trial_days": None,
        }
    return item


def _subscription(items: list[dict], *, past_due_ms: int | None = None) -> dict:
    now_ms = int(time.time() * MS)
    return {
        "object": "commerce_subscription",
        "id": "sub_test",
        "instance_id": "ins_test",
        "status": "past_due" if past_due_ms else "active",
        "payer_id": "org_test",
        "created_at": now_ms - 90 * 24 * 3600 * MS,
        "updated_at": now_ms,
        "active_at": now_ms - 90 * 24 * 3600 * MS,
        "past_due_at": past_due_ms,
        "subscription_items": items,
    }


def _hour(delta_hours: float) -> int:
    return int((time.time() + delta_hours * 3600) * MS)


# Every entitlement rule `fetch_live_plan_slug` implements, one
# scenario each. The names are the contract between this server and
# both probes.
def build(name: str) -> tuple[int, dict]:
    if name == "active_pro":
        return 200, _subscription([_item("active", "pro")])
    if name == "active_pro_plus":
        return 200, _subscription([_item("active", "pro_plus")])
    if name == "active_free":
        return 200, _subscription([_item("active", "free_org")])
    if name == "no_items":
        return 200, _subscription([])
    if name == "canceled_future":
        # Cancellation is *scheduled*: the payer keeps the plan until
        # the period ends, so this is still entitled.
        return 200, _subscription([_item("canceled", "pro", period_end_ms=_hour(24))])
    if name == "canceled_past":
        return 200, _subscription([_item("canceled", "pro", period_end_ms=_hour(-24))])
    if name == "canceled_no_period_end":
        return 200, _subscription([_item("canceled", "pro", period_end_ms=None)])
    if name == "canceled_period_end_seconds":
        # period_end below 1e12 is read as epoch *seconds*, not ms.
        return 200, _subscription([
            {**_item("canceled", "pro"), "period_end": int(time.time() + 86400)}
        ])
    if name == "active_after_canceled":
        # The first *active* item wins even when a canceled one
        # precedes it in the list.
        return 200, _subscription([
            _item("canceled", "pro_plus", period_end_ms=_hour(24)),
            _item("active", "pro"),
        ])
    if name == "two_active_items":
        # Pins "the FIRST active item wins". Without this, a port that
        # takes the last active item scores identical on every other
        # case — `active_after_canceled` has only one active item, so
        # first and last are the same thing there.
        return 200, _subscription([
            _item("active", "pro_plus"), _item("active", "pro"),
        ])
    if name == "first_canceled_wins":
        # Among canceled items only the first entitled one is kept.
        return 200, _subscription([
            _item("canceled", "pro", period_end_ms=_hour(24)),
            _item("canceled", "pro_plus", period_end_ms=_hour(48)),
        ])
    if name == "item_without_slug":
        # An EMPTY slug is skipped, not treated as free — `slug` is a
        # required non-nullable str in the SDK model, so this is the
        # only way the `if not slug: continue` branch is reachable.
        return 200, _subscription([
            _item("active", ""), _item("active", "pro"),
        ])
    if name == "item_without_plan":
        return 200, _subscription([
            _item("active", "pro", plan_present=False), _item("active", "pro_plus"),
        ])
    if name == "unknown_slug":
        # A slug with no PLAN_LIMITS entry falls back to free-tier
        # limits while still being reported as itself.
        return 200, _subscription([_item("active", "enterprise_custom")])
    if name == "error_500":
        return 500, {"errors": [{"message": "boom"}]}
    if name == "error_404":
        return 404, {"errors": [{"message": "not found"}]}
    if name == "malformed":
        # Deserialisation fails -> the blanket except -> None, which
        # must NOT downgrade a cached paid plan.
        return 200, {"object": "commerce_subscription"}
    raise KeyError(name)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):  # quiet
        pass

    def _send(self, status: int, payload: dict):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        if self.path.startswith("/__reset"):
            with _lock:
                CALLS.clear()
            self._send(200, {"ok": True})
            return
        if not self.path.startswith("/__scenario"):
            self._send(404, {"errors": [{"message": "no route"}]})
            return
        length = int(self.headers.get("Content-Length") or 0)
        spec = json.loads(self.rfile.read(length) or b"{}")
        with _lock:
            SCENARIOS.update(spec)
            # CALLS is NOT reset here. It was, and the probe's coverage
            # guard duly reported "fake Clerk was called 1 times but 20
            # cases should have gone live" — the counter was being
            # zeroed before every case, so the guard was reading the
            # last case's count. Use /__reset to zero it deliberately.
        self._send(200, {"ok": True, "scenarios": SCENARIOS})

    def do_GET(self):
        if self.path.startswith("/__calls"):
            with _lock:
                self._send(200, dict(CALLS))
            return
        # /v1/organizations/{org_id}/billing/subscription
        parts = [p for p in self.path.split("?")[0].split("/") if p]
        if len(parts) == 5 and parts[0] == "v1" and parts[1] == "organizations" \
                and parts[3] == "billing" and parts[4] == "subscription":
            org_id = parts[2]
            with _lock:
                name = SCENARIOS.get(org_id, "active_free")
                CALLS[org_id] = CALLS.get(org_id, 0) + 1
            try:
                status, payload = build(name)
            except KeyError:
                self._send(500, {"errors": [{"message": f"unknown scenario {name}"}]})
                return
            self._send(status, payload)
            return
        # The health probe lists organizations; harmless to answer.
        if parts[:2] == ["v1", "organizations"]:
            self._send(200, {"data": [], "total_count": 0})
            return
        self._send(404, {"errors": [{"message": "no route"}]})


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=18080)
    args = ap.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"fake clerk on http://127.0.0.1:{args.port}/v1", flush=True)
    server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
