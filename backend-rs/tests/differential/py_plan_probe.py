#!/usr/bin/env python3
"""Resolve plans through the real Python `plans.py`, over a fake Clerk.

The counterpart of `py_claims_probe.py`, and for the same reason: a
hand-written test encodes what I *think* `effective_plan_for_caps`
does, and if I misread it the port and the test are wrong in the same
direction. This drives the real module.

It runs the functions directly rather than over HTTP, which sidesteps
the problem that makes the HTTP differential blind here: reaching this
code through a request would mean AUTH_PROVIDER=clerk on both tiers,
and therefore real RS256 session tokens from a Clerk instance neither
tier has. The entitlement logic needs neither.

`app.core.clerk.clerk` is monkeypatched to an SDK client pointed at
`fake_clerk.py`. Nothing in `backend/` is modified: the patch is
test-local, and the SDK already accepts a `server_url`.

Emits one JSON object per line: {"case": ..., "plan": ..., "limits": ...}.

Usage: py_plan_probe.py --clerk http://127.0.0.1:18080/v1 [--db ...]
"""
from __future__ import annotations

import argparse
import json
import os
import pathlib
import sys
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
BACKEND = HERE.parents[2] / "backend"
sys.path.insert(0, str(BACKEND))

# The case list lives in plan_cases.json, read by BOTH probes.
#
# It started as a literal here and was lifted out for one reason: the
# Rust probe would have carried its own copy, and two copies of a case
# list drift. A harness where the two sides quietly test different
# inputs reports agreement it has not earned.
CASE_FILE = HERE / "plan_cases.json"


def load_cases() -> tuple[str, list[tuple[str, str, dict]]]:
    spec = json.loads(CASE_FILE.read_text())
    return spec["org_id"], [
        (c["case"], c["scenario"], c["settings"]) for c in spec["cases"]
    ]


def resolve_timestamp(spec: str) -> str:
    """Expand the `@-Nd` shorthand into an ISO timestamp."""
    from datetime import UTC, datetime, timedelta  # noqa: PLC0415

    if not spec.startswith("@"):
        return spec
    body = spec[1:]
    suffix = ""
    if body.endswith("Z"):
        body, suffix = body[:-1], "Z"
    naive = body.endswith("naive")
    if naive:
        body = body[: -len("naive")]
    days = float(body.rstrip("d"))
    dt = datetime.now(tz=UTC) + timedelta(days=days)
    if naive:
        return dt.replace(tzinfo=None).isoformat()
    if suffix == "Z":
        return dt.replace(tzinfo=None).isoformat() + "Z"
    return dt.isoformat()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--clerk", required=True)
    ap.add_argument("--db", default=os.getenv("PROBE_DATABASE_URL"))
    args = ap.parse_args()

    if args.db:
        os.environ["DATABASE_URL"] = args.db
    # Must be set before app.core.config is imported.
    os.environ["AUTH_PROVIDER"] = "clerk"
    os.environ.setdefault("CLERK_SECRET_KEY", "sk_test_fake")
    os.environ.setdefault("CLERK_PUBLISHABLE_KEY", "pk_test_fake")

    from clerk_backend_api import Clerk  # noqa: PLC0415

    from app.core import clerk as clerk_mod  # noqa: PLC0415
    from app.core import plans  # noqa: PLC0415
    from app.core.config import settings  # noqa: PLC0415
    from app.core.database import SessionLocal, engine  # noqa: PLC0415
    from app.models.models import Base, Setting  # noqa: PLC0415

    # This probe never imports app.main, so nothing has created the
    # schema. It runs against its own scratch database on purpose: the
    # write differential snapshots the `settings` table and compares it
    # literally, so writing probe rows into the shared test Postgres
    # would show up there as a phantom side effect.
    Base.metadata.create_all(bind=engine)

    if settings.is_local_auth():
        print("REFUSING: AUTH_PROVIDER resolved to local, so every lookup "
              "short-circuits to self_host and this probe would prove nothing",
              file=sys.stderr)
        return 2

    # Point the SDK at the fake. `plans.fetch_live_plan_slug` does
    # `from app.core.clerk import clerk` *inside* the function, so the
    # module attribute is what it picks up — patching it here is enough.
    # Retries disabled, deliberately. The SDK's default is
    # `BackoffStrategy(500, 60000, 1.5, 3600000)` — it retries a 5xx or
    # a connection failure for up to ONE HOUR before raising. The
    # `error_500` case would otherwise stall this probe for an hour,
    # and the outcome it is testing (`fetch_live_plan_slug` returns
    # None, so the cached plan is kept) is reached either way. Only the
    # latency differs, and latency is not what this compares.
    #
    # See PYTHON_BUGS.md: that same one-hour retry runs on the event
    # loop in `get_hls_segment`, which is a rather more serious
    # consequence than a slow test.
    from clerk_backend_api.utils import BackoffStrategy, RetryConfig  # noqa: PLC0415

    clerk_mod.clerk = Clerk(
        bearer_auth="sk_test_fake",
        server_url=args.clerk,
        retry_config=RetryConfig("backoff", BackoffStrategy(50, 200, 1.5, 1000), False),
    )

    org, cases = load_cases()
    root = args.clerk.rsplit("/v1", 1)[0]
    # Zero the fake's call counter so the coverage number below
    # describes *this* run. Without it the count accumulates across
    # runs and the guard degrades into "some run once called Clerk".
    urllib.request.urlopen(
        urllib.request.Request(root + "/__reset", data=b"", method="POST"), timeout=5
    ).read()
    db = SessionLocal()
    live_lookups = 0
    try:
        for name, scenario, rows in cases:
            urllib.request.urlopen(
                urllib.request.Request(
                    root + "/__scenario",
                    data=json.dumps({org: scenario}).encode(),
                    method="POST",
                ),
                timeout=5,
            ).read()

            for key, value in rows.items():
                Setting.set(db, org, key, resolve_timestamp(value))
            # Clear every key a previous case may have left behind, so
            # cases cannot leak into each other.
            for key in ("payment_past_due", "payment_past_due_at"):
                if key not in rows:
                    Setting.set(db, org, key, "")
            db.commit()

            # Both in-process caches are dropped between cases: they are
            # exercised deliberately further down, not incidentally here.
            plans.invalidate_effective_plan_cache()
            with plans._resolve_lock:
                plans._last_resolve_at.clear()

            plan = plans.effective_plan_for_caps(db, org, use_cache=False)
            limits = plans.get_plan_limits(plan)
            print(json.dumps({
                "case": name,
                "plan": plan,
                "limits": limits,
                "display": plans.get_plan_display_name(plan),
            }, sort_keys=True))

        calls_before = json.loads(urllib.request.urlopen(
            root + "/__calls", timeout=5).read())
        live_lookups = calls_before.get(org, 0)
    finally:
        db.close()

    # Coverage guard. If the fake was never called, every case above
    # took the cached fast path or short-circuited, and a green
    # comparison would mean nothing — the exact failure this harness was
    # written to prevent.
    expected_live = sum(1 for _, _, rows in cases
                        if rows.get("org_plan", "") not in ("pro", "pro_plus", "self_host"))
    if live_lookups < expected_live:
        print(f"REFUSING: fake Clerk was called {live_lookups} times but "
              f"{expected_live} cases should have gone live — the live path "
              f"is not being exercised", file=sys.stderr)
        return 2
    print(f"coverage: {live_lookups} live Clerk lookups across {len(cases)} cases",
          file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
