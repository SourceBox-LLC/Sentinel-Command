#!/usr/bin/env python3
"""Print what list_runs computes as "today" for fixed zones and instants.

`local_midnight_utc` in src/zoneinfo.rs is tested against these lines.
They run the route's own expression with `datetime.now(tz=...)` swapped
for `datetime.fromtimestamp(ts, tz=...)`, which is the same call with
the clock pinned — both go through `tz.fromutc`, which is where fold
is decided.

Usage: backend/.venv/bin/python tests/differential/midnight_probe.py
"""
from datetime import UTC, datetime
from zoneinfo import ZoneInfo

CASES = [
    ("UTC", "2026-05-07T15:00:00Z"),
    ("America/Los_Angeles", "2026-05-07T15:00:00Z"),
    ("America/Los_Angeles", "2026-05-07T06:00:00Z"),
    ("Asia/Kolkata", "2026-05-07T20:00:00Z"),
    ("Pacific/Kiritimati", "2026-05-07T11:00:00Z"),
    ("America/Santiago", "2026-09-06T12:00:00Z"),
    ("America/Havana", "2026-11-01T04:30:00Z"),
    ("America/Havana", "2026-11-01T05:30:00Z"),
    ("America/Havana", "2026-11-01T12:00:00Z"),
]

for zone, now in CASES:
    org_tz = ZoneInfo(zone)
    ts = datetime.fromisoformat(now).timestamp()
    now_local = datetime.fromtimestamp(ts, tz=org_tz)
    today_start = (
        now_local.replace(hour=0, minute=0, second=0, microsecond=0)
        .astimezone(UTC)
        .replace(tzinfo=None)
    )
    print(f'("{zone}", "{now}", "{today_start:%Y-%m-%d %H:%M:%S}"),  # fold={now_local.fold}')
