"""
Dependency probes used by both health endpoints.

Two consumers:

  - ``/api/health/ready``    — readiness check.  Each probe runs;
                                a critical failure returns HTTP 503
                                so external uptime monitors page
                                someone.  Cached for 30s in main.py
                                so a swarm of pollers doesn't
                                hammer Clerk.
  - ``/api/health/detailed`` — verbose status snapshot.  Same
                                probes plus in-process subsystem
                                counters (HLS cache occupancy, SSE
                                subscribers, etc.).  Always returns
                                200 with the status nested in the
                                body — for a future status page or
                                an admin dashboard panel.

Probe contract:
  Every probe returns a ``ProbeResult`` with at minimum
  ``status`` (``"ok"``, ``"warn"``, ``"critical"``, ``"disabled"``,
  ``"unconfigured"``) plus probe-specific fields (latency_ms,
  percent_used, age_s, error_class).  Never raises — failures
  surface as ``status="critical"`` with an ``error_class`` tag so
  ops can grep the type without exposing exception strings to the
  internet.

  Critical-tier probes (DB, Clerk, disk-95%, email-worker-wedged
  when email enabled) flip the readiness rollup to 503.  Warn-tier
  (disk 80%+, viewer-usage queue building) flip the detailed
  endpoint to ``"degraded"`` but don't block readiness.
"""

from __future__ import annotations

import asyncio
import logging
import os
import shutil
import time
from dataclasses import dataclass, field
from typing import Any

from sqlalchemy import text

from app.core.config import settings
from app.core.database import SessionLocal

logger = logging.getLogger(__name__)


# ── Probe result shape ──────────────────────────────────────────────


@dataclass
class ProbeResult:
    """One probe's outcome.

    ``status`` levels:
      - ``ok``           — probe passed.
      - ``warn``         — non-critical degradation; surfaces as yellow
                           on the detailed endpoint, doesn't trigger 503.
      - ``critical``     — readiness rollup flips to 503.
      - ``disabled``     — feature is intentionally off (e.g. email
                           kill-switch); not a failure.
      - ``unconfigured`` — feature is on but missing config (e.g.
                           EMAIL_ENABLED=true but no RESEND_API_KEY).
                           Not critical — operator hasn't finished
                           setup yet.

    ``data`` carries probe-specific fields the consumers render —
    latency_ms for DB, percent_used for disk, age_s for the worker
    tick, error_class for any failure.
    """

    status: str
    data: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {"status": self.status, **self.data}

    @property
    def is_critical(self) -> bool:
        return self.status == "critical"


# ── Probes ──────────────────────────────────────────────────────────


def probe_database() -> ProbeResult:
    """``SELECT 1`` round-trip to confirm the DB is responsive.

    Cheap (sub-ms on SQLite, low-ms on Postgres).  A failure here
    is the most pager-worthy signal in the system: every meaningful
    request reads or writes the DB."""
    try:
        db = SessionLocal()
        try:
            t0 = time.perf_counter()
            db.execute(text("SELECT 1"))
            latency_ms = round((time.perf_counter() - t0) * 1000, 2)
            return ProbeResult(
                status="ok",
                data={"latency_ms": latency_ms},
            )
        finally:
            db.close()
    except Exception as exc:
        # Don't surface exception text — connection strings / hostnames
        # could leak into a public health endpoint.  Class name + log.
        logger.warning("[Health] DB ping failed", exc_info=True)
        return ProbeResult(
            status="critical",
            data={"error_class": type(exc).__name__},
        )


async def probe_clerk(timeout_seconds: float = 5.0) -> ProbeResult:
    """Verify Clerk's API is reachable.

    Lists organizations with ``limit=1`` — cheapest GET we have
    without needing a specific org id to lookup.  Wrapped in
    ``asyncio.to_thread`` because the Clerk SDK is sync; wrapped in
    ``wait_for`` so a hung Clerk endpoint doesn't hang the health
    check past ``timeout_seconds``.

    Critical: if Clerk is down, no JWT verification works → no
    authenticated request can succeed → app is effectively down
    even though /api/health says "ok".
    """
    if settings.is_local_auth():
        # Self-hosted: there's no Clerk account by design, not a
        # forgotten setup step — report distinctly from "unconfigured"
        # so an operator doesn't read this as a misconfiguration.
        return ProbeResult(status="disabled", data={})

    if not settings.CLERK_SECRET_KEY:
        # No way to even attempt the call — surfaces as unconfigured.
        # Probably a dev environment without Clerk wired up; not a
        # production-pageable signal.
        return ProbeResult(status="unconfigured", data={})

    try:
        from app.core.clerk import clerk

        async def _call():
            # Tiny, read-only call.  We don't care about the response
            # contents — just that the API responded.
            return await asyncio.to_thread(
                clerk.organizations.list, limit=1,
            )

        t0 = time.perf_counter()
        await asyncio.wait_for(_call(), timeout=timeout_seconds)
        latency_ms = round((time.perf_counter() - t0) * 1000, 2)
        return ProbeResult(
            status="ok",
            data={"latency_ms": latency_ms},
        )
    except TimeoutError:
        logger.warning("[Health] Clerk probe timed out after %ss", timeout_seconds)
        return ProbeResult(
            status="critical",
            data={"error_class": "Timeout", "timeout_seconds": timeout_seconds},
        )
    except Exception as exc:
        logger.warning("[Health] Clerk probe failed", exc_info=True)
        return ProbeResult(
            status="critical",
            data={"error_class": type(exc).__name__},
        )


# Grace window for a freshly-started process — the background
# check-in loop's immediate boot-time tick (see
# _sentinel_license_reconcile_loop in main.py) hasn't necessarily
# landed yet. Mirrors EMAIL_WORKER_STARTUP_GRACE_SECONDS's pattern
# above: "never ticked" only means "unhealthy" once we're past a
# window where that's actually surprising.
SENTINEL_LICENSE_PROBE_STARTUP_GRACE_SECONDS = 30.0


def probe_sentinel_license(uptime_seconds: float) -> ProbeResult:
    """Report the cached Sentinel AI license state for self-hosted
    installs (see app/core/license_client.py for the full contract).

    Deliberately never "critical" — an unlicensed or grace-degraded
    Sentinel feature is not a Command Center outage, so this must
    never flip /api/health/ready's rollup. It's surfaced on
    /api/health/detailed only, for an operator to notice.

    Status reflects actual license validity, not just reachability —
    a revoked/expired/suspended license with a perfectly reachable
    license service must never report "ok" just because the HTTP
    round-trip succeeded.
    """
    if not settings.is_local_auth():
        return ProbeResult(status="disabled", data={})

    if not settings.SENTINEL_LICENSE_KEY:
        return ProbeResult(status="unconfigured", data={})

    from app.core.license_client import (
        _LAST_CHECK_AT,
        _LAST_CHECK_REACHABLE,
        _LAST_OK_AT,
        SENTINEL_LICENSE_GRACE_HOURS,
        is_sentinel_licensed,
    )
    from app.models.models import Setting

    db = SessionLocal()
    try:
        licensed = is_sentinel_licensed(db)
        reachable = Setting.get(db, settings.LOCAL_ORG_ID, _LAST_CHECK_REACHABLE, "")
        last_check_at = Setting.get(db, settings.LOCAL_ORG_ID, _LAST_CHECK_AT, "")
        last_ok_at = Setting.get(db, settings.LOCAL_ORG_ID, _LAST_OK_AT, "")
    finally:
        db.close()

    data = {
        "licensed": licensed,
        "last_check_reachable": reachable == "true",
        "last_check_at": last_check_at or None,
        "last_ok_at": last_ok_at or None,
        "grace_hours": SENTINEL_LICENSE_GRACE_HOURS,
    }

    if not last_check_at and uptime_seconds < SENTINEL_LICENSE_PROBE_STARTUP_GRACE_SECONDS:
        # No check-in has landed yet, but the process just started —
        # give the boot-time tick a few seconds rather than reporting
        # "warn" on every single restart.
        data["note"] = "startup grace"
        return ProbeResult(status="ok", data=data)

    # Only a confirmed-fresh, confirmed-valid answer is "ok". Coasting
    # on grace (service unreachable, licensed=True from a past check-in)
    # and an explicit denial (licensed=False even though reachable=True)
    # both surface as "warn" — never "critical" per the docstring above
    # — but neither should be silently reported as fully healthy.
    status = "ok" if (licensed and reachable == "true") else "warn"
    return ProbeResult(status=status, data=data)


# Disk-usage thresholds match the existing detailed endpoint's:
#   - 95%+ → critical (write failures imminent; should page)
#   - 80%+ → warn (plan a volume resize; not yet failing)
DISK_CRITICAL_PCT = 95.0
DISK_WARN_PCT = 80.0


def probe_disk() -> ProbeResult:
    """Check the SQLite volume usage.  Returns critical at 95%+,
    warn at 80%+, ok otherwise.

    Path: ``/data`` in production (Fly volume mount), current
    directory in dev — the endpoint stays informative without
    /data existing locally.
    """
    disk_path = "/data" if os.path.isdir("/data") else "."
    try:
        usage = shutil.disk_usage(disk_path)
        pct = round(
            (usage.used / usage.total) * 100, 1
        ) if usage.total else 0.0
        if pct >= DISK_CRITICAL_PCT:
            status = "critical"
        elif pct >= DISK_WARN_PCT:
            status = "warn"
        else:
            status = "ok"
        return ProbeResult(
            status=status,
            data={
                "path": disk_path,
                "bytes_used": usage.used,
                "bytes_free": usage.free,
                "bytes_total": usage.total,
                "percent_used": pct,
            },
        )
    except OSError as exc:
        logger.warning("[Health] disk_usage(%s) failed", disk_path, exc_info=True)
        return ProbeResult(
            status="critical",
            data={"path": disk_path, "error_class": type(exc).__name__},
        )


# Email worker wedge thresholds.  Worker ticks every
# EMAIL_WORKER_INTERVAL_SECONDS (default 5s); ``stale_after_seconds``
# is the cap above which we conclude the worker is hung.  60s gives
# the loop ~12 chances to tick — a wedge below that line is more
# likely transient (one slow Resend call) than fatal.
EMAIL_WORKER_STALE_AFTER_SECONDS = 60.0

# Grace window for a freshly-started process.  Until uptime crosses
# this, "no tick yet" reads as ok — the loop just hasn't had its
# first scheduled tick complete.
EMAIL_WORKER_STARTUP_GRACE_SECONDS = 30.0


def probe_email_worker(uptime_seconds: float) -> ProbeResult:
    """Detect a wedged email worker by reading the in-process
    last-tick timestamp it stamps in ``app.core.email_worker``.

    Status logic:
      - ``EMAIL_ENABLED=False``                         → disabled
      - never ticked + uptime within grace             → ok (startup)
      - never ticked + uptime past grace               → critical
      - last tick within ``stale_after_seconds``        → ok
      - last tick older than ``stale_after_seconds``    → critical

    The criticality only applies WHEN email is enabled.  An org that
    deliberately runs without email shouldn't get paged about a
    worker that's correctly idle.
    """
    if not settings.EMAIL_ENABLED:
        return ProbeResult(status="disabled", data={})

    # Imported lazily so health-probe code doesn't pull the email
    # worker module at app start.
    from app.core import email_worker

    age = email_worker.seconds_since_last_tick()

    if age is None:
        # Brand-new process: give the loop time for its first tick.
        if uptime_seconds < EMAIL_WORKER_STARTUP_GRACE_SECONDS:
            return ProbeResult(
                status="ok",
                data={
                    "tick_age_seconds": None,
                    "note": "startup grace",
                },
            )
        # Past the grace window with zero ticks: worker never
        # started, or the loop crashed before its first iteration.
        # Fatal — page someone.
        return ProbeResult(
            status="critical",
            data={
                "tick_age_seconds": None,
                "error_class": "WorkerNeverTicked",
                "uptime_seconds": uptime_seconds,
            },
        )

    if age > EMAIL_WORKER_STALE_AFTER_SECONDS:
        return ProbeResult(
            status="critical",
            data={
                "tick_age_seconds": round(age, 2),
                "stale_after_seconds": EMAIL_WORKER_STALE_AFTER_SECONDS,
                "error_class": "WorkerStale",
            },
        )

    return ProbeResult(
        status="ok",
        data={
            "tick_age_seconds": round(age, 2),
        },
    )


# ── Composition: run all critical probes ───────────────────────────


@dataclass
class ReadinessReport:
    """Aggregate result of running every readiness probe.

    ``ready`` is True iff no critical-tier probe failed.  Consumers
    typically use it to choose between HTTP 200 + 503.
    """

    ready: bool
    probes: dict[str, ProbeResult]

    def to_dict(self) -> dict[str, Any]:
        return {
            "ready": self.ready,
            "checks": {name: p.to_dict() for name, p in self.probes.items()},
        }


async def run_readiness_probes(uptime_seconds: float) -> ReadinessReport:
    """Run every probe used by ``/api/health/ready``.  Probes run
    concurrently where they're independent so a slow Clerk doesn't
    serialise behind the DB ping.

    Note: probe_database + probe_disk + probe_email_worker are sync
    and fast; only probe_clerk has meaningful I/O wait.  We ``gather``
    them anyway for symmetry — keeps the wiring uniform if a future
    probe gets slower."""
    db_task = asyncio.to_thread(probe_database)
    clerk_task = probe_clerk()
    disk_task = asyncio.to_thread(probe_disk)
    worker_task = asyncio.to_thread(probe_email_worker, uptime_seconds)

    db, clerk, disk, worker = await asyncio.gather(
        db_task, clerk_task, disk_task, worker_task,
    )

    probes: dict[str, ProbeResult] = {
        "database": db,
        "clerk": clerk,
        "disk": disk,
        "email_worker": worker,
    }
    ready = not any(p.is_critical for p in probes.values())
    return ReadinessReport(ready=ready, probes=probes)
