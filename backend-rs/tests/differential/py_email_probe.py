#!/usr/bin/env python3
"""Drive the real Python email worker over a fake Resend.

The counterpart of `examples/email_probe.rs`, reading the same
`email_cases.json`. Run both, diff the output: that is
`tests/differential/email_run.sh`.

Why a probe and not an HTTP case: the worker has no HTTP surface. It is
a loop, and the write differential can only reach it by waiting for a
timer — which is exactly the race the harness pins every other loop out
of the way to avoid. `run_one_tick` is a pure function over a session,
and Python's own tests drive it directly for the same reason.

`resend.api_url` comes from the environment, so both stacks can be
pointed at the same fake without editing either.

Emits one JSON object per scenario: the tick summaries, then every
outbox and log row left behind.

Usage: py_email_probe.py --resend http://127.0.0.1:18095 [--db ...]
"""
from __future__ import annotations

import argparse
import json
import os
import sys
from datetime import datetime, timedelta, timezone


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--resend", required=True)
    ap.add_argument("--db", default=os.environ.get("PROBE_DATABASE_URL", ""))
    ap.add_argument("--cases", default=os.path.join(os.path.dirname(__file__), "email_cases.json"))
    args = ap.parse_args()

    # Set before app.core.config is imported: it reads os.environ at
    # import time, and a later assignment would be too late.
    os.environ["RESEND_API_URL"] = args.resend
    os.environ["RESEND_API_KEY"] = "re_probe_key"
    os.environ["EMAIL_ENABLED"] = "true"
    os.environ["EMAIL_FROM_ADDRESS"] = "notifications@example.com"
    os.environ["EMAIL_FROM_NAME"] = "Sentinel"
    os.environ["EMAIL_MAX_ATTEMPTS"] = "3"
    os.environ["EMAIL_WORKER_BATCH_SIZE"] = "20"
    if args.db:
        os.environ["DATABASE_URL"] = args.db
    os.environ.setdefault("AUTH_PROVIDER", "local")
    os.environ.setdefault("APP_SECRET_KEY", "probe-secret")

    sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "..", "backend"))

    import resend  # noqa: PLC0415

    resend.api_url = args.resend

    from app.core.database import SessionLocal, engine  # noqa: PLC0415
    from app.core.email_worker import (  # noqa: PLC0415
        _reset_tick_for_tests,
        run_one_tick,
        seconds_since_last_tick,
    )
    from app.models.models import Base, EmailLog, EmailOutbox, EmailSuppression  # noqa: PLC0415
    import app.core.config as config_mod  # noqa: PLC0415
    import app.core.email as email_mod  # noqa: PLC0415

    Base.metadata.create_all(bind=engine)
    cases = json.load(open(args.cases))

    for scenario in cases["scenarios"]:
        db = SessionLocal()
        try:
            _reset(db)
            _seed(db, scenario)

            # Per-scenario overrides. Assigned on the settings object
            # the modules already hold, not the environment, because
            # they read it once at import.
            settings = config_mod.settings
            saved = (settings.EMAIL_ENABLED, settings.RESEND_API_KEY,
                     settings.EMAIL_WORKER_BATCH_SIZE)
            settings.EMAIL_ENABLED = scenario.get("email_enabled", True)
            if "api_key" in scenario:
                settings.RESEND_API_KEY = scenario["api_key"]
                resend.api_key = scenario["api_key"] or None
            else:
                resend.api_key = settings.RESEND_API_KEY
            settings.EMAIL_WORKER_BATCH_SIZE = scenario.get("batch_size", 20)
            email_mod._reset_for_tests()

            # Reset per scenario, so "did this tick stamp" is a
            # question about THIS tick rather than about any earlier
            # one in the same process.
            _reset_tick_for_tests()
            summaries = []
            for _ in range(scenario.get("ticks", 1)):
                summaries.append(run_one_tick(db))
            # The value is wall-clock, so only its presence is
            # compared: the health probe's question is "has the loop
            # been scheduled", not "how long ago".
            ticked = seconds_since_last_tick() is not None

            (settings.EMAIL_ENABLED, settings.RESEND_API_KEY,
             settings.EMAIL_WORKER_BATCH_SIZE) = saved

            print(json.dumps({
                "scenario": scenario["name"],
                "summaries": summaries,
                "ticked": ticked,
                "outbox": _dump_outbox(db, EmailOutbox),
                "log": _dump_log(db, EmailLog),
            }, sort_keys=True), flush=True)
        finally:
            db.close()
    return 0


def _reset(db) -> None:
    from app.models.models import EmailLog, EmailOutbox, EmailSuppression  # noqa: PLC0415
    for model in (EmailLog, EmailOutbox, EmailSuppression):
        db.query(model).delete()
    db.commit()


def _seed(db, scenario) -> None:
    from app.models.models import EmailOutbox, EmailSuppression  # noqa: PLC0415
    now = datetime.now(tz=timezone.utc).replace(tzinfo=None)
    for entry in scenario.get("suppressed", []):
        address, reason = (entry, "bounce") if isinstance(entry, str) else entry
        db.add(EmailSuppression(address=address.lower(), reason=reason,
                                source="probe", created_at=now))
    for row in scenario.get("rows", []):
        db.add(EmailOutbox(
            id=row["id"],
            org_id=row.get("org_id", "self-host"),
            recipient_email=row["recipient"],
            subject=row.get("subject", "Subject"),
            body_text=row.get("body_text", "text body"),
            body_html=row.get("body_html", "<p>html body</p>"),
            kind=row["kind"],
            status=row["status"],
            attempts=row["attempts"],
            created_at=now - timedelta(seconds=row.get("created_offset", 0)),
            last_attempt_at=(
                now - timedelta(seconds=row["last_attempt_offset"])
                if "last_attempt_offset" in row else None
            ),
        ))
    db.commit()


def _dump_outbox(db, model) -> list:
    return [
        {
            "id": r.id, "org_id": r.org_id, "recipient": r.recipient_email,
            "kind": r.kind, "status": r.status, "attempts": r.attempts,
            "resend_message_id": r.resend_message_id, "error": r.error,
            "sent": r.sent_at is not None,
            # The claim is the only writer of last_attempt_at, so this
            # is how "the batch was claimed before sending" is visible
            # at all once the tick has finished.
            "attempted": r.last_attempt_at is not None,
        }
        for r in db.query(model).order_by(model.id).all()
    ]


def _dump_log(db, model) -> list:
    return [
        {
            "org_id": r.org_id, "recipient": r.recipient_email, "kind": r.kind,
            "status": r.status, "resend_message_id": r.resend_message_id,
            "error": r.error,
        }
        for r in db.query(model).order_by(model.id).all()
    ]


if __name__ == "__main__":
    raise SystemExit(main())
