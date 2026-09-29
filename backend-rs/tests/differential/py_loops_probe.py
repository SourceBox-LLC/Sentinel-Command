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
    ap.add_argument("--body", choices=("sweep", "cleanup"), required=True)
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

    sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "..", "backend"))

    from app.core.database import SessionLocal  # noqa: PLC0415
    import app.main as main_mod  # noqa: PLC0415

    db = SessionLocal()
    try:
        if args.body == "sweep":
            summary = main_mod.run_offline_sweep(db)
        else:
            summary = main_mod.run_log_cleanup(db)
    finally:
        db.close()

    print(json.dumps({"summary": summary}, sort_keys=True), flush=True)

    db = SessionLocal()
    try:
        for label, rows in snapshot(db, args.body).items():
            print(json.dumps({"rows": label, "value": rows}, sort_keys=True), flush=True)
    finally:
        db.close()
    return 0


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
