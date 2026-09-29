#!/usr/bin/env python3
"""Drive the real Python loop bodies, and print what they did.

The counterpart of `examples/loops_probe.rs`. Run both against the same
freshly seeded database and diff the output: that is
`tests/differential/loops_run.sh`.

Why a probe rather than HTTP cases: neither loop has an HTTP surface.
Nothing calls them and nothing returns to a caller, which is exactly
what makes them the least verifiable part of this port — the plan says
so in as many words ("Background loops are the least testable part and
the most likely to silently diverge, because nothing calls them and
nothing returns"). Python already extracted both bodies into
`run_offline_sweep` and `run_log_cleanup`, synchronous and returning a
summary, so a probe can call them directly. That extraction was itself
the response to a production Sentry alert: an `AttributeError` in the
cleanup ran nightly, swallowed by the loop's own `try/except`.

Prints two things per body, because the summary alone is not enough:

  * the summary the body returns, which is what the log line reports;
  * the resulting ROWS — every row the body could have touched, so a
    body that returned the right counts while deleting the wrong rows
    is visible. A count-based check would pass that.

Nothing in `backend/` is modified.

Usage: py_loops_probe.py --db postgresql+psycopg://… --body sweep|cleanup
"""
from __future__ import annotations

import argparse
import json
import os
import sys


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default=os.environ.get("PROBE_DATABASE_URL", ""))
    ap.add_argument("--body",
                    choices=("sweep", "cleanup", "reaper", "digest", "license",
                             "reconcile"),
                    required=True)
    ap.add_argument("--license-url", default="")
    ap.add_argument("--clerk-url", default="")
    ap.add_argument("--license-key", default="probe-license-key")
    args = ap.parse_args()
    if args.db:
        os.environ["DATABASE_URL"] = args.db
    os.environ.setdefault("APP_SECRET_KEY", "probe-secret")
    os.environ.setdefault("LOCAL_ORG_ID", "self-host")
    # NOT local auth: `resolve_org_plan` short-circuits to self_host
    # before it reads a setting at all, which would give every org the
    # same 365-day retention and collapse the three tiers this fixture
    # exists to separate.
    os.environ["AUTH_PROVIDER"] = "clerk"
    # The digest's emit branch is behind `email_enabled_for_kind`, whose
    # first gate is the global EMAIL_ENABLED kill-switch. Left off, the
    # branch never runs — and both sides agree on having done nothing,
    # which is the exact failure loops_run.sh's coverage guard exists to
    # catch. It caught it.
    os.environ["EMAIL_ENABLED"] = "true"

    sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "..", "backend"))

    from app.core.database import SessionLocal  # noqa: PLC0415
    import app.main as main_mod  # noqa: PLC0415

    db = SessionLocal()
    try:
        if args.body == "sweep":
            summary = main_mod.run_offline_sweep(db)
        elif args.body == "reaper":
            from app.core.sentinel_dispatch import reap_stranded_runs  # noqa: PLC0415
            summary = reap_stranded_runs(db)
        elif args.body == "reconcile":
            summary = run_plan_reconcile(args, main_mod)
        elif args.body == "license":
            summary = run_license_checkin(args)
        elif args.body == "digest":
            # The digest is the one body Python did NOT extract from its
            # loop — it is all inline. So run the real loop for exactly
            # one tick rather than reimplementing it here, which would
            # only encode what I already believe it does.
            #
            # The loop sleeps FIRST and then works, so the second sleep
            # is the signal that one tick has completed. Counting sleeps
            # and raising on the second is what bounds it.
            summary = run_one_digest_tick(main_mod)
        else:
            summary = main_mod.run_log_cleanup(db)
    finally:
        db.close()

    print(json.dumps({"summary": summary}, sort_keys=True), flush=True)
    if args.body == "license":
        # The summary IS the comparison here: every scenario's resulting
        # rows are in it, and there is nothing else the check-in touches.
        return 0

    db = SessionLocal()
    try:
        for label, rows in snapshot(db, args.body).items():
            print(json.dumps({"rows": label, "value": rows}, sort_keys=True), flush=True)
    finally:
        db.close()
    return 0


# Which answer the fake Clerk gives for each org. The names are the
# fake's own contract; see the header of loops_fixture.sql for what each
# org is meant to prove.
RECONCILE_SCENARIOS = {
    "rec-agree": "active_pro",
    "rec-downgrade": "active_free",
    "rec-upgrade": "active_pro_plus",
    "rec-unreachable": "error_500",
    "rec-free": "active_pro",
}


def run_plan_reconcile(args, main_mod) -> dict:
    """One hourly sweep, against the fake Clerk.

    NOT local auth for this body: `fetch_live_plan_slug` is the whole
    point and a self-host short-circuit would skip it entirely.
    """
    import urllib.request  # noqa: PLC0415

    from clerk_backend_api import Clerk  # noqa: PLC0415
    from clerk_backend_api.utils import BackoffStrategy, RetryConfig  # noqa: PLC0415
    import app.core.clerk as clerk_mod  # noqa: PLC0415

    # `fetch_live_plan_slug` does `from app.core.clerk import clerk`
    # INSIDE the function, so patching the module attribute is enough —
    # nothing in backend/ changes.
    #
    # Retries disabled, and this is not an optimisation. The SDK's
    # default is BackoffStrategy(500, 60000, 1.5, 3600000): it retries a
    # 5xx or a connection failure for up to ONE HOUR before raising. The
    # `error_500` scenario stalled this probe for nineteen minutes
    # before I killed it. The outcome being tested — the lookup returns
    # None, so the cached plan is KEPT — is reached either way; only the
    # latency differs, and latency is not what this compares.
    #
    # PYTHON_BUGS #1 is that same one-hour retry running on the event
    # loop in `get_hls_segment`, which is a rather more serious
    # consequence than a slow probe.
    clerk_mod.clerk = Clerk(
        bearer_auth="sk_test_probe",
        server_url=f"{args.clerk_url.rstrip('/')}/v1",
        retry_config=RetryConfig("backoff", BackoffStrategy(50, 200, 1.5, 1000), False),
    )

    body = json.dumps(RECONCILE_SCENARIOS).encode()
    urllib.request.urlopen(urllib.request.Request(
        f"{args.clerk_url.rstrip('/')}/__scenario", data=body,
        headers={"Content-Type": "application/json"}), timeout=5).read()

    changed = main_mod._reconcile_org_plans()
    return {"changed": changed}


def run_license_checkin(args) -> dict:
    """One check-in per scenario, against the fake licence service.

    The scenarios are selected by URL prefix rather than by a mode
    endpoint, so the fake stays stateless and the two probes cannot
    leave each other a surprise.

    Local auth on purpose: this whole module is a self-host concern and
    the read side short-circuits for hosted orgs.
    """
    import asyncio  # noqa: PLC0415
    import app.core.config as config_mod  # noqa: PLC0415
    from app.core.database import SessionLocal  # noqa: PLC0415
    from app.core.license_client import check_in_with_license_service  # noqa: PLC0415

    settings = config_mod.settings
    settings.SENTINEL_LICENSE_KEY = args.license_key
    out = {}
    for scenario in LICENSE_SCENARIOS:
        settings.SENTINEL_LICENSE_SERVICE_URL = scenario_url(args, scenario)
        db = SessionLocal()
        try:
            # Wipe the cached verdict between scenarios: what is being
            # compared is what THIS check-in wrote, not what survived
            # from the previous one.
            clear_license_settings(db, settings.LOCAL_ORG_ID)
            asyncio.run(check_in_with_license_service(db))
            out[scenario] = read_license_settings(db, settings.LOCAL_ORG_ID)
        finally:
            db.close()
    return out


# Every answer the check-in has to tell apart. `unreachable` points at a
# port nothing is listening on, which is the state the grace window
# exists for and the one a port is most likely to turn into an error.
LICENSE_SCENARIOS = (
    "valid",
    "valid-sync",
    "revoked",
    "sync-without-valid",
    "truthy",
    "falsy",
    "not-an-object",
    "garbage",
    "server-error",
    "missing-valid",
    "unreachable",
)


def scenario_url(args, scenario: str) -> str:
    if scenario == "unreachable":
        return "http://127.0.0.1:1"
    return f"{args.license_url.rstrip('/')}/scenario/{scenario}"


LICENSE_KEYS = (
    "sentinel_license_valid",
    "sentinel_license_last_check_reachable",
    "sentinel_data_sync_enabled",
    "sentinel_install_id",
)


def clear_license_settings(db, org_id: str) -> None:
    from sqlalchemy import text  # noqa: PLC0415

    db.execute(
        text("DELETE FROM settings WHERE org_id = :o AND key LIKE 'sentinel_license_%'"),
        {"o": org_id},
    )
    db.execute(
        text("DELETE FROM settings WHERE org_id = :o AND key = 'sentinel_data_sync_enabled'"),
        {"o": org_id},
    )
    db.commit()


def read_license_settings(db, org_id: str) -> dict:
    """The rows the check-in wrote.

    `install_id` is reported as whether it EXISTS, not as its value: it
    is `secrets.token_hex(16)`, random per mint, and the behaviour under
    test is that one gets minted and then reused — not which one.
    The two timestamps are reported as presence for the same reason.
    """
    from sqlalchemy import text  # noqa: PLC0415

    rows = dict(
        db.execute(
            text("SELECT key, value FROM settings WHERE org_id = :o"), {"o": org_id}
        ).all()
    )
    return {
        "valid": rows.get("sentinel_license_valid"),
        "reachable": rows.get("sentinel_license_last_check_reachable"),
        "sync_enabled": rows.get("sentinel_data_sync_enabled"),
        "has_install_id": bool(rows.get("sentinel_install_id")),
        "has_last_check_at": bool(rows.get("sentinel_license_last_check_at")),
        "has_last_ok_at": bool(rows.get("sentinel_license_last_ok_at")),
    }


class _TickDone(Exception):
    """Raised inside the patched sleep to end the loop after one tick."""


def run_one_digest_tick(main_mod) -> dict:
    """Drive `_motion_digest_loop` for a single tick.

    The loop has no summary to return — it logs and moves on — so the
    probe reports the anchor COUNTS it can see, which is what the Rust
    body returns. The rows are the real comparison either way.
    """
    import asyncio  # noqa: PLC0415

    real_sleep = asyncio.sleep
    ticks = {"n": 0}

    async def counted(delay, *args, **kwargs):
        ticks["n"] += 1
        if ticks["n"] > 1:
            raise _TickDone()
        return await real_sleep(0)

    asyncio.sleep = counted
    try:
        asyncio.run(main_mod._motion_digest_loop())
    except _TickDone:
        pass
    finally:
        asyncio.sleep = real_sleep
    # Deliberately not a count of what the tick did: Python's loop keeps
    # no such tally, and inventing one here would be comparing the
    # probe's arithmetic rather than the code's. The rows below are the
    # comparison.
    return {"ticked": True}


def snapshot(db, body: str) -> dict:
    """Every row either body could have touched, as comparable tuples.

    Ordered explicitly in SQL. The bodies' own queries have no ORDER BY,
    so the order they see is Postgres's physical order — but what is
    compared here is the RESULT, and an unordered snapshot of a result
    is a flake. The same reasoning as `http_diff.py`'s list sorting.
    """
    from sqlalchemy import text  # noqa: PLC0415

    def q(sql: str) -> list:
        return [list(row) for row in db.execute(text(sql)).all()]

    if body == "reconcile":
        return {
            "plans": q("""SELECT org_id, value FROM settings
                           WHERE key = 'org_plan' AND org_id LIKE 'rec-%'
                           ORDER BY org_id"""),
            # The cap's work, which a reconcile that wrote the setting
            # and skipped `enforce_camera_cap` would leave undone.
            "capped": q("""SELECT camera_id, disabled_by_plan FROM cameras
                            WHERE org_id LIKE 'rec-%' ORDER BY camera_id"""),
        }

    if body == "digest":
        return {
            # What survived, and with what value: a re-armed anchor must
            # still be here holding its NEW timestamp, and a closed one
            # must be gone.
            "anchors": q("""SELECT org_id, key,
                                   value = '@rearmed' AS rearmed,
                                   value IS NULL OR value = '' AS blank
                              FROM settings
                             WHERE key LIKE 'motion_email_cooldown_start:%'
                             ORDER BY org_id, key"""),
            "digests": q("""SELECT org_id, title, body, severity, audience, link,
                                   camera_id, meta_json
                              FROM notifications WHERE kind = 'motion_digest'
                             ORDER BY org_id, title"""),
        }

    if body == "reaper":
        return {
            "runs": q("""SELECT id, outcome, summary,
                                completed_at IS NOT NULL AS completed
                           FROM sentinel_runs WHERE org_id LIKE 'loops-%'
                          ORDER BY id"""),
        }

    if body == "sweep":
        return {
            "nodes": q("""SELECT node_id, status FROM camera_nodes
                           WHERE org_id LIKE 'loops-%' ORDER BY node_id"""),
            "cameras": q("""SELECT camera_id, status FROM cameras
                             WHERE org_id LIKE 'loops-%' ORDER BY camera_id"""),
            # The transitions the sweep announced. `meta_json` carries
            # nothing here; the title is what a reader sees and the
            # `name or id` fallback shows up in it.
            "notifications": q("""SELECT org_id, kind, audience, title, body, severity,
                                         link, camera_id, node_id
                                    FROM notifications
                                   WHERE org_id LIKE 'loops-%'
                                     AND kind IN ('node_offline', 'camera_offline')
                                   ORDER BY org_id, kind, title"""),
            # The emit ORDER, reduced to the one thing about it that IS
            # a claim. The row order WITHIN each group is not: the sweep
            # reads its stale rows with no ORDER BY on either side, so
            # two stale cameras may be announced in either order. What
            # the code does claim is that every NODE is announced before
            # every CAMERA, so an operator sees the uplink drop before
            # the cameras behind it. A row_number comparison caught the
            # within-group order too and differed for that reason alone.
            "nodes_before_cameras": q("""
                SELECT COALESCE(
                  (SELECT max(id) FILTER (WHERE kind = 'node_offline')
                        < min(id) FILTER (WHERE kind = 'camera_offline')
                     FROM notifications
                    WHERE org_id LIKE 'loops-%'
                      AND kind IN ('node_offline', 'camera_offline')), false)"""),
        }

    if body == "sweep":
        return {
            "nodes": q("""SELECT node_id, status FROM camera_nodes
                           WHERE org_id LIKE 'loops-%' ORDER BY node_id"""),
            "cameras": q("""SELECT camera_id, status FROM cameras
                             WHERE org_id LIKE 'loops-%' ORDER BY camera_id"""),
            # The transitions the sweep announced. `meta_json` carries
            # nothing here; the title is what a reader sees and the
            # `name or id` fallback shows up in it.
            #
            # `seq` is a row_number over id rather than the id itself: it
            # makes the emit ORDER comparable — nodes before cameras,
            # which is a claim the code makes and nothing else here
            # checks — without pinning absolute ids that any fixture
            # change would shift.
            "notifications": q("""SELECT row_number() OVER (ORDER BY id) AS seq,
                                         org_id, kind, audience, title, body, severity,
                                         link, camera_id, node_id
                                    FROM notifications
                                   WHERE org_id LIKE 'loops-%'
                                     AND kind IN ('node_offline', 'camera_offline')
                                   ORDER BY id"""),
        }

    return {
        # Ages rather than timestamps: the two probes run seconds apart
        # and a literal would differ for that reason alone. Rounded to
        # whole days, which is the granularity every cutoff uses.
        "stream": q("""SELECT org_id, round(extract(epoch FROM now()::timestamp - accessed_at)
                                            / 86400)::int AS age
                         FROM stream_access_logs WHERE org_id LIKE 'loops-%'
                        ORDER BY org_id, age"""),
        "mcp": q("""SELECT org_id, round(extract(epoch FROM now()::timestamp - timestamp)
                                         / 86400)::int FROM mcp_activity_logs
                     WHERE org_id LIKE 'loops-%' ORDER BY 1, 2"""),
        "audit": q("""SELECT org_id, round(extract(epoch FROM now()::timestamp - timestamp)
                                           / 86400)::int FROM audit_log
                       WHERE org_id LIKE 'loops-%' ORDER BY 1, 2"""),
        "motion": q("""SELECT org_id, round(extract(epoch FROM now()::timestamp - timestamp)
                                            / 86400)::int FROM motion_events
                        WHERE org_id LIKE 'loops-%' ORDER BY 1, 2"""),
        "notif": q("""SELECT org_id, round(extract(epoch FROM now()::timestamp - created_at)
                                           / 86400)::int FROM notifications
                       WHERE org_id LIKE 'loops-%' ORDER BY 1, 2"""),
        "email_log": q("""SELECT org_id, round(extract(epoch FROM now()::timestamp - timestamp)
                                               / 86400)::int FROM email_log
                           WHERE org_id LIKE 'loops-%' ORDER BY 1, 2"""),
        # Status as well as age: the whole point of the outbox rule is
        # that a pending row survives an age a sent row does not.
        "email_outbox": q("""SELECT org_id, status,
                                    round(extract(epoch FROM now()::timestamp - created_at)
                                          / 86400)::int
                               FROM email_outbox WHERE org_id LIKE 'loops-%'
                              ORDER BY 1, 2, 3"""),
        "processed_webhooks": q("""SELECT svix_msg_id,
                                          round(extract(epoch FROM now()::timestamp
                                                        - processed_at) / 86400)::int
                                     FROM processed_webhooks
                                    WHERE svix_msg_id LIKE 'loops-%' ORDER BY 1"""),
    }


if __name__ == "__main__":
    sys.exit(main())
