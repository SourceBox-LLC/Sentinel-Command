#!/usr/bin/env python3
"""Drive the real Python health probes with every input supplied.

The counterpart of `examples/health_probe.rs`, reading the same
`health_cases.json`. Run both, diff the output: that is
`tests/differential/health_run.sh`.

Why a probe and not HTTP cases: the HTTP differential has the endpoints,
but it cannot reach most of what they report. The mutator restarts the
tier before every run, so the process is always inside its startup
grace; the harness environment has email on, a valid licence and a
healthy disk; and the real filesystem moves between the two calls, so
comparing a live reading is either flaky or normalised into
invisibility. Seventeen mutations survived a full run for those reasons
and not one of them was a difference of opinion about the code.

Three things are patched, all of them process-local state a harness
cannot arrange from outside: `shutil.disk_usage`, the email worker's
tick age, and the clock the probes measure uptime against — which is
passed in as a parameter on both sides already.

Nothing in `backend/` is modified; the patches live here.

Usage: py_health_probe.py --db postgresql+psycopg://…
"""
from __future__ import annotations

import argparse
import asyncio
import collections
import json
import os
import sys
from datetime import datetime, timedelta, timezone

# Resolved by the runner and handed to both probes, so the two agree on
# the literal rather than each computing its own "now".
RECENT = os.environ.get(
    "HEALTH_PROBE_RECENT",
    (datetime.now(tz=timezone.utc) - timedelta(hours=1))
    .replace(tzinfo=None).isoformat(timespec="seconds"),
)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default=os.environ.get("PROBE_DATABASE_URL", ""))
    ap.add_argument("--cases", default=os.path.join(os.path.dirname(__file__),
                                                    "health_cases.json"))
    args = ap.parse_args()
    if args.db:
        os.environ["DATABASE_URL"] = args.db
    os.environ.setdefault("APP_SECRET_KEY", "probe-secret")
    os.environ.setdefault("LOCAL_ORG_ID", "self-host")

    sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "..", "backend"))

    import shutil  # noqa: PLC0415

    from app.core.database import SessionLocal, engine  # noqa: PLC0415
    from app.models.models import Base, Setting  # noqa: PLC0415
    import app.core.config as config_mod  # noqa: PLC0415
    import app.core.email_worker as email_worker  # noqa: PLC0415
    import app.core.health_probes as probes  # noqa: PLC0415

    Base.metadata.create_all(bind=engine)
    settings = config_mod.settings
    cases = json.load(open(args.cases))

    Usage = collections.namedtuple("Usage", "total used free")

    # One live reading, reported first; see the note in
    # examples/health_probe.rs and the tolerance in health_run.sh.
    live_path = "/data" if os.path.isdir("/data") else "."
    live = shutil.disk_usage(live_path)
    print(json.dumps({
        "scenario": "@live-disk",
        "path": live_path,
        "bytes_total": live.total,
        "bytes_free": live.free,
        "bytes_used": live.used,
    }, sort_keys=True), flush=True)

    for scenario in cases["scenarios"]:
        # The licence probe reads Settings, so the rows are the fixture.
        db = SessionLocal()
        try:
            db.query(Setting).delete()
            for key, value in (scenario.get("settings") or {}).items():
                # "@recent" is an hour ago. A literal would drift out of
                # the 72-hour licence grace within days and the scenario
                # would stop distinguishing "coasting" from "invalid".
                if value == "@recent":
                    value = RECENT
                db.add(Setting(org_id=settings.LOCAL_ORG_ID, key=key, value=value))
            db.commit()
        finally:
            db.close()

        disk = scenario["disk"]
        saved_usage = shutil.disk_usage
        saved_tick = email_worker.seconds_since_last_tick
        saved = (settings.AUTH_PROVIDER, settings.EMAIL_ENABLED,
                 settings.SENTINEL_LICENSE_KEY, settings.CLERK_SECRET_KEY)

        shutil.disk_usage = lambda _p: Usage(  # noqa: ARG005
            total=disk["total"], used=disk["used"], free=disk["free"])
        email_worker.seconds_since_last_tick = lambda: scenario["tick_age"]
        settings.AUTH_PROVIDER = "local" if scenario["local_auth"] else "clerk"
        settings.EMAIL_ENABLED = scenario["email_enabled"]
        settings.SENTINEL_LICENSE_KEY = scenario.get("license_key", "")
        # The Clerk probe must not reach the network: under local auth
        # it short-circuits, and the hosted scenarios leave the secret
        # empty so it reports unconfigured rather than trying.
        settings.CLERK_SECRET_KEY = ""

        uptime = scenario["uptime"]
        try:
            disk_probe = probes.probe_disk()
            worker = probes.probe_email_worker(uptime)
            clerk = asyncio.run(probes.probe_clerk())
            license_probe = probes.probe_sentinel_license(uptime)
            report = asyncio.run(probes.run_readiness_probes(uptime))
        finally:
            shutil.disk_usage = saved_usage
            email_worker.seconds_since_last_tick = saved_tick
            (settings.AUTH_PROVIDER, settings.EMAIL_ENABLED,
             settings.SENTINEL_LICENSE_KEY, settings.CLERK_SECRET_KEY) = saved

        readiness = report.to_dict()
        # The database probe's latency is a measurement, and the only
        # field here that is not a decision. Its STATUS is compared.
        for check in readiness["checks"].values():
            check.pop("latency_ms", None)

        print(json.dumps({
            "scenario": scenario["name"],
            "disk": disk_probe.to_dict(),
            "email_worker": worker.to_dict(),
            "clerk": clerk.to_dict(),
            "sentinel_license": license_probe.to_dict(),
            "readiness": readiness,
        }, sort_keys=True), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
