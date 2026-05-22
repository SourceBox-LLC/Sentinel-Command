import asyncio
import logging
import os
import shutil
import threading
import time
from collections import defaultdict
from contextlib import asynccontextmanager
from datetime import UTC, datetime, timedelta
from pathlib import Path
from time import monotonic
from typing import Optional

# Process start markers for ``/api/health/detailed``. Captured at module
# import (i.e. uvicorn cold-start) so uptime is real wall + monotonic
# time, not "ms since the request handler ran". Module-level constants
# are fine — there's only ever one process per Fly machine.
_STARTED_AT_WALL = datetime.now(tz=UTC)
_STARTED_AT_MONO = time.monotonic()
from fastapi import FastAPI, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse, JSONResponse
from fastapi.staticfiles import StaticFiles
from slowapi.errors import RateLimitExceeded
from slowapi.middleware import SlowAPIMiddleware

from app.api import (
    audit,
    cameras,
    gdpr,
    hls,
    incidents,
    install,
    mcp_activity,
    mcp_keys,
    motion,
    nodes,
    notifications,
    sentinel,
    webhooks,
    well_known,
    ws,
)
from app.core.config import settings
from app.core.database import Base, SessionLocal, engine
from app.core.limiter import limiter, tenant_aware_key
from app.core.logging_setup import configure_logging
from app.core.migrations import sync_schema
from app.core.request_context import (
    new_request_id,
    reset_request_id,
    set_request_id,
)
from app.core.sentry import init_sentry
from app.mcp.server import mcp

# Import models so every table registers on Base.metadata before create_all/sync_schema.
from app.models import models  # noqa: F401

# Install context-aware logging BEFORE any logger.info() in this module
# fires.  Idempotent — safe even if main.py is re-imported in tests.
configure_logging()

logger = logging.getLogger(__name__)

# Initialise Sentry as early as possible — before we register routes — so
# any exception raised during app construction is still captured. No-ops
# cleanly when SENTRY_DSN is unset (local dev, tests).
init_sentry(
    dsn=settings.SENTRY_DSN or None,
    traces_sample_rate=settings.SENTRY_TRACES_SAMPLE_RATE,
)

Base.metadata.create_all(bind=engine)
# Patch in any columns that were added to existing models after the table was first
# created. See app/core/migrations.py for the "why" — this is our stand-in for Alembic.
sync_schema(engine, Base.metadata)

# NOTE: ``drop_orphan_tables`` and ``sanitize_existing_codecs`` USED to
# run here on every boot.  Both are one-shot fixes for problems that
# have long since washed through prod (the webhook_endpoints orphan
# from the d4dd2db revert was dropped weeks ago; the avc1.*e0[a-3]
# codec sanitisation rewrote every affected row in May 2026 and stays
# at zero rows on subsequent runs).
#
# They're still importable from ``app.core.migrations`` for the
# next time we need them — the pattern is documented at the top of
# that module — but pulling them out of the hot startup path means
# we no longer pay a metadata round-trip per boot for fixes that
# completed months ago.  If you're restoring an old DB snapshot
# from before either fix landed, run them once by hand:
#
#     >>> from app.core.migrations import drop_orphan_tables, sanitize_existing_codecs
#     >>> drop_orphan_tables(engine); sanitize_existing_codecs(engine)

# Build the MCP ASGI app — path="/" because the mount prefix handles /mcp
mcp_app = mcp.http_app(path="/", stateless_http=True, json_response=True)


# ── Background-loop tunables ──────────────────────────────────────
# These are defined *here* (above the functions that reference them)
# so the f-string in ``lifespan`` doesn't depend on the rest of the
# module having loaded first. lifespan is invoked by uvicorn during
# app startup, after the module is fully imported, so the old layout
# worked — but it was fragile to any refactor that called the
# function during import. Putting the constants up top makes the
# dependency direction obvious.

# Fallback retention for orgs whose plan can't be resolved (Clerk lookup
# failed AND no cached Setting). The per-org tiered retention —
# 30d / 90d / 365d for Free / Pro / Pro Plus — is sourced from
# ``app.core.plans.PLAN_LIMITS[plan]["log_retention_days"]`` and applied
# in ``_log_cleanup_loop`` below. This env var only matters when plan
# resolution breaks entirely; we keep a 90-day default so a transient
# Clerk outage doesn't silently wipe a paid customer's logs.
LOG_RETENTION_DAYS = int(os.getenv("LOG_RETENTION_DAYS", "90"))
LOG_CLEANUP_INTERVAL_HOURS = 24  # Run once per day
# Cameras offline for longer than this get their in-memory caches freed.
# HLS segments are live-only fragments — useless once streaming stops.
INACTIVE_CAMERA_CLEANUP_HOURS = int(os.getenv("INACTIVE_CAMERA_CLEANUP_HOURS", "24"))

# How often to sweep for stale "online" entities and flip them to offline.
# Needs to be shorter than the heartbeat-miss threshold (90s) for
# timely notifications but longer than a few seconds to keep DB load low.
OFFLINE_SWEEP_INTERVAL_SECONDS = int(os.getenv("OFFLINE_SWEEP_INTERVAL_SECONDS", "30"))
# If a node/camera hasn't heart-beat in this many seconds, the sweep
# marks it offline.  Matches the 90s threshold used by the model's
# ``effective_status`` property so the UI and DB agree.
OFFLINE_HEARTBEAT_TIMEOUT_SECONDS = 90

# How often to refresh the GitHub /releases/latest cache used by
# version-compatibility checks.  Matches the cache TTL in
# ``app.core.release_cache`` (10 minutes) so each tick lands just as
# the cache would otherwise go stale.  Kept under GitHub's 60/hour
# unauthenticated rate limit even with multiple replicas.
RELEASE_CACHE_REFRESH_INTERVAL_SECONDS = int(
    os.getenv("RELEASE_CACHE_REFRESH_INTERVAL_SECONDS", "600")
)

# How often to poll disk usage and emit operator-side disk-full alerts.
# 5 min is fast enough that we beat /data filling up between ticks
# (writes are bytes-per-second scale, not megabytes-per-second), but
# slow enough to keep the system call cost trivial.  Per-tick work is
# one ``shutil.disk_usage`` and one in-memory threshold compare.
DISK_CHECK_INTERVAL_SECONDS = int(os.getenv("DISK_CHECK_INTERVAL_SECONDS", "300"))
# Threshold at which we Sentry-alert "your disk is filling up — act now."
# Mirrors the 95% threshold used by /api/health/detailed so the alert
# and the dashboard agree.  Below this we leave it to the inbox + the
# status page to communicate "approaching" warnings.
DISK_CRITICAL_THRESHOLD_PERCENT = float(
    os.getenv("DISK_CRITICAL_THRESHOLD_PERCENT", "95.0")
)
# After emitting a critical alert, re-emit no sooner than this.  6h is
# the right shape for "I emailed you about this; if you haven't fixed
# it in 6h, here's another nudge" — paging cadence without becoming a
# spam trigger.  Per-process state, so a process restart resets the
# debounce; that's deliberate (an operator restarting the server is
# probably already aware of the disk situation).
DISK_CRITICAL_REEMIT_INTERVAL_SECONDS = int(
    os.getenv("DISK_CRITICAL_REEMIT_INTERVAL_SECONDS", str(6 * 3600))
)

# Cadence for the motion-email digest sweep.  60s is fast enough that
# a 15-minute cooldown's digest is delivered within ~1 minute of window
# expiry (acceptable lag for "here's a summary of the last 15 min"),
# but slow enough that the LIKE scan over Setting rows is a rounding
# error.  See app/api/notifications.py::_claim_motion_cooldown_or_silence
# for the per-camera anchor mechanism this loop drains.
MOTION_DIGEST_INTERVAL_SECONDS = int(
    os.getenv("MOTION_DIGEST_INTERVAL_SECONDS", "60")
)

# Stranded-run reaper sweep cadence.  The threshold itself is
# STRANDED_RUN_AGE_MINUTES (defined in app/core/sentinel_dispatch.py)
# — this is just how often we check.  5 min is plenty: a run that
# crashed at minute 0 is found at minute ≤ 25 (threshold + sweep
# interval), which is good enough for a UI-fixup loop.  Tunable
# down for ops or up for quieter environments.
SENTINEL_REAPER_INTERVAL_SECONDS = int(
    os.getenv("SENTINEL_REAPER_INTERVAL_SECONDS", "300")
)


@asynccontextmanager
async def lifespan(app):
    """Application lifespan: startup and shutdown hooks."""
    from app.core.email_worker import email_worker_loop

    cleanup_task = asyncio.create_task(_log_cleanup_loop())
    offline_sweep_task = asyncio.create_task(_offline_sweep_loop())
    viewer_usage_task = asyncio.create_task(_viewer_usage_flush_loop())
    release_refresh_task = asyncio.create_task(_release_cache_refresh_loop())
    # Email worker drains EmailOutbox via Resend.  Ships always-on so
    # the kill-switch can be flipped via env var without a redeploy;
    # when EMAIL_ENABLED=false the transport short-circuits and the
    # worker just logs "would have sent" lines for any outbox row
    # that gets enqueued.  See app/core/email_worker.py.
    email_worker_task = asyncio.create_task(email_worker_loop())
    # Disk-check loop polls /data every 5 min and emits an
    # OPERATOR-SIDE alert via logger.error (Sentry-captured when
    # SENTRY_DSN is set) when usage crosses 95%.  Customer org
    # admins are intentionally NOT in the alert path — see
    # _check_and_emit_disk_critical for the rationale.  Pure in-
    # memory debounce, no DB persistence.
    disk_check_task = asyncio.create_task(_disk_check_loop())
    # Motion-email digest sweep — drains expired per-camera cooldown
    # anchors written by the email-immediate path in
    # app/api/notifications.py::_claim_motion_cooldown_or_silence and
    # emits a single ``motion_digest`` notification per camera that
    # accumulated additional motion events during the cooldown window.
    # See _motion_digest_loop below for the full lifecycle.
    motion_digest_task = asyncio.create_task(_motion_digest_loop())
    # Sentinel stranded-run reaper — sweeps SentinelRun rows stuck in
    # `running` past the agent's wall-clock budget and marks them
    # errored.  See _sentinel_reaper_loop below + reap_stranded_runs
    # in app/core/sentinel_dispatch.py.
    sentinel_reaper_task = asyncio.create_task(_sentinel_reaper_loop())
    print(
        f"[App] Sentinel Command Center started "
        f"(log retention: {LOG_RETENTION_DAYS}d, "
        f"email: {'on' if settings.EMAIL_ENABLED else 'off'})"
    )
    async with mcp_app.lifespan(app):
        yield
    cleanup_task.cancel()
    offline_sweep_task.cancel()
    viewer_usage_task.cancel()
    release_refresh_task.cancel()
    email_worker_task.cancel()
    disk_check_task.cancel()
    motion_digest_task.cancel()
    sentinel_reaper_task.cancel()
    print("[System] Shutdown complete")


app = FastAPI(
    title="Sentinel Command Center API",
    description="FastAPI backend with Clerk authentication for Sentinel Command Center",
    version="2.1.2",
    lifespan=lifespan,
    # Move FastAPI's auto docs off /docs so the React DocsPage can own that path.
    docs_url="/api-docs",
    redoc_url="/api-redoc",
    openapi_url="/api/openapi.json",
)

app.state.limiter = limiter


async def rate_limit_exceeded_handler(request: Request, exc: RateLimitExceeded) -> JSONResponse:
    """Custom 429 response — gives the client everything it needs to retry.

    The slowapi default emits a bare `{"detail": "429 ..."}` string with no
    Retry-After header, which leaves integrators guessing at the backoff
    window and which limit they hit. We return:
      - a stable JSON shape matching the rest of the API's error envelope
      - the exact limit string (e.g. "60 per 1 minute") so callers know what
        bucket they tripped
      - a `Retry-After: 60` header, per RFC 9110, so off-the-shelf HTTP
        clients back off without special handling
    60s is a safe upper bound because our tightest rate windows are minute-
    scoped; callers that honour Retry-After will idle through the window and
    succeed on the next attempt.
    """
    limit_str = str(exc.detail) if getattr(exc, "detail", None) else "rate limit exceeded"
    body = {
        "error": "rate_limit_exceeded",
        "message": (
            "Too many requests. Back off and retry after the Retry-After window. "
            "See /docs#api-rate-limits for per-route limits."
        ),
        "limit": limit_str,
        "retry_after_seconds": 60,
    }
    return JSONResponse(status_code=429, content=body, headers={"Retry-After": "60"})


app.add_exception_handler(RateLimitExceeded, rate_limit_exceeded_handler)
app.add_middleware(SlowAPIMiddleware)


# ── Pydantic validation errors → ApiError envelope ──────────────────
#
# FastAPI's default 422 body for a request that fails Pydantic
# validation is a list of dicts:
#
#     {"detail": [{"loc": [...], "msg": "...", "type": "..."}]}
#
# That's machine-friendly but unreadable to a human, and the frontend's
# error parser used to stringify the array into something like
# "[object Object]" before we taught it about the shape.  Funnel
# validation failures through the same envelope ApiError uses, so the
# REST surface produces one shape regardless of whether the failure
# came from a hand-raised exception or Pydantic's auto-validation.
#
# Behaviour on the frontend stays consistent: services/api.js looks at
# body.detail.message and shows it as-is.  Test code can still inspect
# body.detail.errors for the structured per-field breakdown.
from fastapi.exceptions import RequestValidationError  # noqa: E402


@app.exception_handler(RequestValidationError)
async def validation_exception_handler(request: Request, exc: RequestValidationError):
    """Rewrite Pydantic 422 envelope to match ApiError's shape."""
    errors = exc.errors()
    # Build a one-line summary out of the first failing field — that's
    # what gets shown in the toast.  The full error list is preserved
    # under detail.errors for callers (and tests) that want the breakdown.
    if errors:
        first = errors[0]
        loc = ".".join(str(p) for p in first.get("loc", []) if p != "body")
        msg = first.get("msg", "Validation failed")
        summary = f"{msg} ({loc})" if loc else msg
    else:
        summary = "Request validation failed"
    return JSONResponse(
        status_code=422,
        content={
            "detail": {
                "error": "validation_failed",
                "message": summary,
                "errors": errors,  # full list for clients that want it
            },
        },
    )

# Get frontend URL from environment (set in fly.toml or .env)
frontend_url = os.getenv("FRONTEND_URL", "http://localhost:5173")

# Validate FRONTEND_URL format — a malformed value (missing scheme,
# trailing slash, embedded whitespace) would silently widen CORS or
# quietly fail to match the real origin header.  Log loud and drop it
# so we don't ship an ambiguous allow-list to production.
def _validate_frontend_url(url: str) -> str | None:
    if not url:
        return None
    url = url.strip()
    if not url:
        return None
    if not (url.startswith("http://") or url.startswith("https://")):
        logger.warning(
            "[Startup] FRONTEND_URL=%r must start with http:// or https:// — ignoring",
            url,
        )
        return None
    # Reject trailing slashes — CORS origin compares exact strings.
    if url.endswith("/"):
        url = url.rstrip("/")
    # Reject embedded whitespace or commas (likely misconfigured list).
    if any(c.isspace() for c in url) or "," in url:
        logger.warning(
            "[Startup] FRONTEND_URL=%r contains whitespace or comma — ignoring",
            url,
        )
        return None
    return url


frontend_url = _validate_frontend_url(frontend_url)

# CORS configuration.
#
# Localhost origins are always allowed — developer-convenience baseline
# so `npm run dev` against a Fly-hosted backend works without env config.
# Production origins are sourced from CORS_ALLOWED_ORIGINS (comma-
# separated) so adding a preview/staging origin doesn't require a code
# change.  Default value is the canonical Fly URL so a missing env var
# during a fresh deploy doesn't lock everyone out.
#
# FRONTEND_URL stays honoured for backwards compatibility (some
# deployment scripts set it directly).  Each entry runs through
# _validate_frontend_url so a malformed value produces a single
# warning and is dropped, instead of silently widening CORS.
cors_origins = [
    "http://localhost:5173",
    "http://localhost:8000",
]

extra_origins_raw = os.getenv(
    "CORS_ALLOWED_ORIGINS",
    "https://opensentry-command.fly.dev",
)
for raw in extra_origins_raw.split(","):
    validated = _validate_frontend_url(raw)
    if validated and validated not in cors_origins:
        cors_origins.append(validated)

if frontend_url and frontend_url not in cors_origins:
    cors_origins.append(frontend_url)

app.add_middleware(
    CORSMiddleware,
    allow_origins=cors_origins,
    allow_credentials=True,
    allow_methods=["GET", "POST", "PUT", "DELETE", "OPTIONS"],
    allow_headers=["Authorization", "Content-Type", "X-Node-API-Key", "X-Request-Id"],
    expose_headers=["X-Request-Id"],
)


# ── Request context middleware ─────────────────────────────────────
# Stamp every request with a request_id (honoring an inbound
# X-Request-Id from the client/proxy if it's well-formed; otherwise
# minting one ourselves).  The id is:
#   - stored in a contextvar so the logging filter picks it up on
#     every log record (see app/core/logging_setup.py)
#   - tagged on the Sentry scope so exception traces are searchable
#     by request_id in the Sentry dashboard
#   - returned in the response header so a customer can quote it in
#     a support ticket and we can find their exact request in seconds
#
# Defined as a function-style middleware (the @app.middleware("http")
# decorator) rather than the BaseHTTPMiddleware class because the
# decorator form preserves response streaming better — Starlette's
# BaseHTTPMiddleware has a known issue where it buffers SSE responses
# (we have several SSE endpoints — motion, notifications, MCP activity)
# and breaks live streaming.
@app.middleware("http")
async def request_context(request: Request, call_next):
    inbound = request.headers.get("X-Request-Id", "")
    # Sanity-check the inbound value before trusting it: 8-128 chars,
    # alphanumerics + hyphens only.  Garbage / overly long strings get
    # replaced with a fresh id — we don't want a malicious header
    # injecting weird characters into our log lines or Sentry tags.
    if 8 <= len(inbound) <= 128 and inbound.replace("-", "").isalnum():
        req_id = inbound
    else:
        req_id = new_request_id()

    token = set_request_id(req_id)
    # Best-effort Sentry tag.  Safe no-op when SENTRY_DSN is unset
    # (local dev / tests).
    try:
        import sentry_sdk
        sentry_sdk.get_current_scope().set_tag("request_id", req_id)
    except Exception:
        pass

    try:
        response = await call_next(request)
    finally:
        reset_request_id(token)

    response.headers["X-Request-Id"] = req_id
    return response


@app.middleware("http")
async def security_headers(request: Request, call_next):
    """Add security headers to all responses."""
    response = await call_next(request)
    response.headers["X-Content-Type-Options"] = "nosniff"
    response.headers["X-Frame-Options"] = "DENY"
    response.headers["Referrer-Policy"] = "strict-origin-when-cross-origin"
    response.headers["Permissions-Policy"] = "camera=(), microphone=(), geolocation=()"
    if request.url.scheme == "https" or os.getenv("FLY_APP_NAME"):
        response.headers["Strict-Transport-Security"] = "max-age=63072000; includeSubDomains"
    return response

# Include API routers
app.include_router(cameras.router)
app.include_router(webhooks.router)
app.include_router(nodes.router)
app.include_router(audit.router)
app.include_router(hls.router)
app.include_router(ws.router)
app.include_router(install.router)
app.include_router(mcp_keys.router)
app.include_router(mcp_activity.router)
app.include_router(incidents.router)
app.include_router(motion.router)
app.include_router(notifications.router)
# Sentinel agent — config + run history.  Slice 1 of the 7-slice
# rollout: persistence only, the agent itself isn't yet wired up.
app.include_router(sentinel.router)
# GDPR Article 20 export endpoint.  Article 17 erasure is served
# by the existing /api/settings/danger/full-reset endpoint, which
# now routes through app.core.gdpr.delete_org_data so both paths
# delete identical data sets.
app.include_router(gdpr.router)
# /.well-known/security.txt + the legacy /security.txt alias.
# Mounted before the SPA middleware sees the request — see the
# pass-through whitelist in spa_middleware below.
app.include_router(well_known.router)

# Mount MCP server at /mcp
app.mount("/mcp", mcp_app)


# Background-loop constants moved up to just above the ``lifespan``
# function (search "Background-loop tunables") so the f-string in
# ``lifespan`` doesn't reference a name defined later in the module.


def run_log_cleanup(db, *, default_retention_days: int = LOG_RETENTION_DAYS) -> dict:
    """Delete log rows older than each org's tier-specific retention window.

    Extracted from ``_log_cleanup_loop`` so tests can drive it directly
    without waiting for a background tick. Mirrors ``run_offline_sweep``'s
    "synchronous, takes a session, returns a summary" shape — see the
    Sentry alert OPENSENTRY-COMMAND-1 for why exercising this path
    end-to-end matters (the loop's outer ``try/except`` swallowed an
    AttributeError nightly for an unknown stretch before that fired).

    Retention is tiered (Free 30d / Pro 90d / Pro Plus 365d) so cleanup
    has to iterate orgs instead of running one global cutoff query. Orgs
    without a resolvable plan fall back to ``default_retention_days``
    (a parameter for test override; production passes
    ``LOG_RETENTION_DAYS``) so an org we can't look up isn't silently
    kept forever.

    The function form ``union(a, b, c, ...)`` below is the SQLAlchemy
    2.x-compatible way to compose this. The chained form
    ``select(...).union(...).union(...)`` works on the first call
    (returns a ``CompoundSelect``) but the second ``.union()`` raises
    ``AttributeError: 'CompoundSelect' object has no attribute 'union'``
    — that was the production Sentry bug.

    Returns a dict::

        {
            "orgs_processed": int,
            "totals": {"stream", "mcp", "audit", "motion", "notif", "email_log", "email_outbox"},
            "total_deleted": int,
        }

    EmailOutbox cleanup is bounded to TERMINAL-state rows only —
    pending/sending rows are never deleted regardless of age, because
    deleting an in-flight retry would silently lose the email.  We
    use a fixed 7-day terminal-row window (rather than per-org tier)
    because the outbox is the worker's hot scan target and we want
    it small at all times — the audit trail in EmailLog carries
    long-term history per-org with the same tiered retention as
    other logs.
    """
    from sqlalchemy import select, union

    from app.core.plans import get_plan_limits, resolve_org_plan
    from app.models.models import (
        AuditLog,
        EmailLog,
        EmailOutbox,
        McpActivityLog,
        MotionEvent,
        Notification,
        StreamAccessLog,
    )

    now = datetime.now(tz=UTC).replace(tzinfo=None)

    # One query collects every org_id we're holding logs for.
    # UNION across the log tables — small set, runs once a day.
    # EmailLog joins the union so an org with email-only activity
    # still gets its retention applied.
    org_rows = db.execute(
        union(
            select(StreamAccessLog.org_id).distinct(),
            select(McpActivityLog.org_id).distinct(),
            select(AuditLog.org_id).distinct(),
            select(MotionEvent.org_id).distinct(),
            select(Notification.org_id).distinct(),
            select(EmailLog.org_id).distinct(),
        )
    ).all()
    org_ids = {row[0] for row in org_rows if row[0]}

    totals = {
        "stream": 0, "mcp": 0, "audit": 0, "motion": 0, "notif": 0,
        "email_log": 0,
    }
    for org_id in org_ids:
        try:
            plan = resolve_org_plan(db, org_id)
        except Exception:
            plan = "free_org"
        retention_days = get_plan_limits(plan).get(
            "log_retention_days", default_retention_days,
        )
        cutoff = now - timedelta(days=retention_days)

        totals["stream"] += (
            db.query(StreamAccessLog)
            .filter(StreamAccessLog.org_id == org_id, StreamAccessLog.accessed_at < cutoff)
            .delete(synchronize_session=False)
        )
        totals["mcp"] += (
            db.query(McpActivityLog)
            .filter(McpActivityLog.org_id == org_id, McpActivityLog.timestamp < cutoff)
            .delete(synchronize_session=False)
        )
        totals["audit"] += (
            db.query(AuditLog)
            .filter(AuditLog.org_id == org_id, AuditLog.timestamp < cutoff)
            .delete(synchronize_session=False)
        )
        totals["motion"] += (
            db.query(MotionEvent)
            .filter(MotionEvent.org_id == org_id, MotionEvent.timestamp < cutoff)
            .delete(synchronize_session=False)
        )
        totals["notif"] += (
            db.query(Notification)
            .filter(Notification.org_id == org_id, Notification.created_at < cutoff)
            .delete(synchronize_session=False)
        )
        totals["email_log"] += (
            db.query(EmailLog)
            .filter(EmailLog.org_id == org_id, EmailLog.timestamp < cutoff)
            .delete(synchronize_session=False)
        )

    # EmailOutbox cleanup: cross-org, terminal-state-only, fixed
    # 7-day window.  Per-org tiering doesn't apply here because the
    # outbox is operationally a queue (not a log) — long-term
    # auditability lives in EmailLog above.  Hard-deleting old
    # 'sent'/'failed'/'suppressed' rows keeps the worker's hot scan
    # cheap and prevents the table from being a perpetual store of
    # email subject lines / body content (privacy) for orgs that
    # don't actually have a long retention tier.
    outbox_cutoff = now - timedelta(days=7)
    totals["email_outbox"] = (
        db.query(EmailOutbox)
        .filter(
            EmailOutbox.status.in_(("sent", "failed", "suppressed")),
            EmailOutbox.created_at < outbox_cutoff,
        )
        .delete(synchronize_session=False)
    )

    db.commit()

    return {
        "orgs_processed": len(org_ids),
        "totals": totals,
        "total_deleted": sum(totals.values()),
    }


async def _log_cleanup_loop():
    """Background task: delete old logs per-org using the caller's plan's
    retention setting, and free segment caches for cameras that have been
    offline.

    The actual work lives in ``run_log_cleanup`` (log retention) and the
    inline inactive-camera-cache block below — keeping the loop itself
    a thin scheduler makes the testable parts testable.
    """
    from app.models.models import Camera

    while True:
        await asyncio.sleep(LOG_CLEANUP_INTERVAL_HOURS * 3600)

        # ── Log retention cleanup (per-org, tiered) ───────────────
        try:
            db = SessionLocal()
            try:
                summary = run_log_cleanup(db)
                if summary["total_deleted"] > 0:
                    t = summary["totals"]
                    logger.info(
                        "[Cleanup] Deleted %d old logs across %d orgs "
                        "(stream=%d mcp=%d audit=%d motion=%d notif=%d "
                        "email_log=%d email_outbox=%d)",
                        summary["total_deleted"], summary["orgs_processed"],
                        t["stream"], t["mcp"], t["audit"], t["motion"], t["notif"],
                        t.get("email_log", 0), t.get("email_outbox", 0),
                    )
            finally:
                db.close()
        except Exception:
            logger.exception("[Cleanup] Log cleanup failed")

        # ── Inactive camera cache cleanup ─────────────────────────
        # Free in-memory segment/playlist caches for cameras that
        # have been offline longer than the threshold.
        try:
            from app.api.hls import cleanup_camera_cache
            inactive_cutoff = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(
                hours=INACTIVE_CAMERA_CLEANUP_HOURS
            )
            db = SessionLocal()
            try:
                inactive_cameras = (
                    db.query(Camera)
                    .filter(Camera.last_seen < inactive_cutoff)
                    .all()
                )
                if inactive_cameras:
                    for cam in inactive_cameras:
                        cleanup_camera_cache(cam.camera_id)
                    logger.info(
                        "[Cleanup] Cleared caches for %d inactive cameras (offline >%dh)",
                        len(inactive_cameras), INACTIVE_CAMERA_CLEANUP_HOURS,
                    )
            finally:
                db.close()
        except Exception:
            logger.exception("[Cleanup] Inactive camera cleanup failed")


def run_offline_sweep(db, *, heartbeat_timeout_seconds: int = OFFLINE_HEARTBEAT_TIMEOUT_SECONDS) -> dict:
    """Flip nodes/cameras from ``status='online'`` to ``'offline'`` when their
    last heartbeat is older than the threshold, emitting notifications for
    each transition.

    Extracted from the loop so tests can drive it directly without
    waiting for a background task tick.  Returns a summary dict with
    counts of nodes/cameras flipped — useful for tests and logging.
    """
    from app.api.notifications import emit_camera_transition, emit_node_transition
    from app.models.models import Camera, CameraNode

    cutoff = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(
        seconds=heartbeat_timeout_seconds
    )

    # Find stale nodes first so we can emit admin notifications for them.
    # Queried with last_seen != None — a row with null last_seen hasn't
    # been heard from yet; if its status is "online" that's a bug, but
    # we'll leave it alone rather than emit a spurious "went offline".
    stale_nodes = (
        db.query(CameraNode)
        .filter(
            CameraNode.status == "online",
            CameraNode.last_seen.isnot(None),
            CameraNode.last_seen < cutoff,
        )
        .all()
    )
    node_transitions: list[tuple[str, str, str]] = []  # (node_id, org_id, display_name)
    for node in stale_nodes:
        node.status = "offline"
        node_transitions.append((node.node_id, node.org_id, node.name or node.node_id))

    stale_cameras = (
        db.query(Camera)
        .filter(
            Camera.status == "online",
            Camera.last_seen.isnot(None),
            Camera.last_seen < cutoff,
        )
        .all()
    )
    camera_transitions: list[tuple[str, str, str, Optional[str]]] = []
    for cam in stale_cameras:
        cam.status = "offline"
        parent_node_id = cam.node.node_id if cam.node else None
        display = cam.name or cam.camera_id
        camera_transitions.append((cam.camera_id, cam.org_id, display, parent_node_id))

    db.commit()

    # Emit AFTER commit so the notification never references a row that
    # got rolled back.
    for nid, oid, name in node_transitions:
        try:
            emit_node_transition(
                db, node_id=nid, org_id=oid, display_name=name, new_status="offline"
            )
        except Exception:
            logger.exception("[OfflineSweep] Failed to emit node transition for %s", nid)

    for cid, oid, name, parent in camera_transitions:
        try:
            emit_camera_transition(
                db,
                camera_id=cid,
                org_id=oid,
                display_name=name,
                new_status="offline",
                node_id=parent,
            )
        except Exception:
            logger.exception("[OfflineSweep] Failed to emit camera transition for %s", cid)

    return {
        "nodes_flipped": len(node_transitions),
        "cameras_flipped": len(camera_transitions),
    }


async def _release_cache_refresh_loop():
    """Background task — keep the GitHub /releases/latest cache warm.

    The cache feeds two paths:
      1. ``/downloads/{os}/{arch}`` — needs the asset list to redirect.
      2. ``check_node_version`` on every node heartbeat — needs the
         ``tag_name`` to decide whether ``update_available`` should fire.

    Refreshing here means heartbeats never block on GitHub, and the
    moment a new CloudNode release ships every connected node sees
    ``update_available`` within one tick — no Command Center deploy
    or env var bump required.

    Runs the first refresh immediately on startup so the cache is
    populated before any heartbeat lands; subsequent ticks honour
    ``RELEASE_CACHE_REFRESH_INTERVAL_SECONDS`` (default 600s, matching
    the cache TTL).  Failures are logged but never raise — the env var
    fallback in ``release_cache.latest_node_version`` keeps the
    heartbeat path serving sensible answers during a GitHub outage.
    """
    from app.core.release_cache import refresh_latest_release

    while True:
        try:
            version = await refresh_latest_release()
            if version:
                logger.debug(
                    "[ReleaseCache] Refreshed latest CloudNode version: %s", version
                )
        except Exception:
            logger.exception("[ReleaseCache] Refresh tick failed")
        await asyncio.sleep(RELEASE_CACHE_REFRESH_INTERVAL_SECONDS)


async def _viewer_usage_flush_loop():
    """Background task — flush per-org viewer-second counters to the DB.

    Runs every 60 seconds. Batching keeps the hot HLS-serve path O(1) in
    memory and amortizes writes to one UPSERT per active org per minute
    rather than one per segment.
    """
    # Imported lazily so test environments that don't import app.api.hls
    # at all don't pay the module-import cost.
    while True:
        await asyncio.sleep(60)
        try:
            from app.api.hls import flush_viewer_usage
            # flush_viewer_usage does its own DB session + error handling;
            # we only care whether anything was written so we can log it.
            await asyncio.to_thread(flush_viewer_usage)
        except Exception:
            logger.exception("[ViewerUsage] Flush loop tick failed")


# Per-process debounce for the disk_critical alert.  Reset on
# process restart, which is fine — an operator restarting the
# server is presumably already aware of the disk situation.
_disk_critical_last_emit_monotonic: Optional[float] = None


def _check_and_emit_disk_critical(db) -> bool:
    """Sample disk usage and emit an OPERATOR-SIDE disk_critical alert
    via logger.error on crossing the threshold.  Returns True if an
    alert was emitted this call (used by the test suite to drive the
    function deterministically).

    The ``db`` parameter is kept in the signature for forward-compat
    (and to avoid changing the caller signature in ``_log_cleanup_loop``)
    but is no longer queried — disk-full is platform infrastructure
    state, not customer-org state, and routing it through customer
    notifications was a multi-tenant violation we removed in 2026-05-04.

    Channels operators actually use for this signal:
      - ``/api/health/detailed`` — reports ``disk.percent_used`` and
        flips the ``disk.status`` field to ``critical`` at 95%.  Any
        external monitor (UptimeRobot, BetterStack, status-page polling)
        picks this up in seconds.
      - Sentry — the ``logger.error`` below renders into a Sentry event
        when SENTRY_DSN is configured (which it is in production).
        That's the path that wakes someone up.
      - Fly metrics — Fly's own dashboards show volume usage.

    What we DON'T do anymore: email customer admins.  They cannot
    ``fly volumes extend`` our infrastructure, and getting paged about
    SourceBox's disk is not what they signed up for.
    """
    global _disk_critical_last_emit_monotonic

    disk_path = "/data" if os.path.isdir("/data") else "."
    try:
        usage = shutil.disk_usage(disk_path)
    except OSError:
        logger.warning("[DiskCheck] disk_usage(%s) failed", disk_path, exc_info=True)
        return False

    if usage.total <= 0:
        return False
    pct = (usage.used / usage.total) * 100

    if pct < DISK_CRITICAL_THRESHOLD_PERCENT:
        # Recovered below threshold — clear the debounce so the next
        # critical hit emits immediately rather than waiting out a
        # stale 6h cooldown.
        _disk_critical_last_emit_monotonic = None
        return False

    now = monotonic()
    last = _disk_critical_last_emit_monotonic
    if last is not None and (now - last) < DISK_CRITICAL_REEMIT_INTERVAL_SECONDS:
        # Still within the re-emit window — silent until cooldown clears.
        return False

    pct_rounded = round(pct, 1)
    bytes_free_gb = round(usage.free / (1024 ** 3), 1)

    # Sentry-captured operator alert.  ``logger.error`` ensures the
    # Sentry SDK wraps this as an event (warnings get sampled away
    # by default).  Structured fields make the dashboard readable
    # without parsing the message string.
    logger.error(
        "[DiskCheck] OPERATOR ALERT — Command Center volume %s%% full "
        "(%.1f GB free, threshold %s%%).  Resize the Fly volume or "
        "trigger early log retention cleanup.  This is platform-level "
        "infrastructure state and is intentionally NOT routed to "
        "customer notifications — see /api/health/detailed for the "
        "monitoring contract.",
        pct_rounded, bytes_free_gb, DISK_CRITICAL_THRESHOLD_PERCENT,
        extra={
            "disk_percent_used": pct_rounded,
            "disk_bytes_free": usage.free,
            "disk_path": disk_path,
            "alert_audience": "operator_only",
        },
    )

    _disk_critical_last_emit_monotonic = now
    return True


async def _disk_check_loop():
    """Background task — poll disk usage, alert operators when full.

    Runs every ``DISK_CHECK_INTERVAL_SECONDS`` (5 min default).  Per-tick
    work: one ``shutil.disk_usage`` call and one threshold compare,
    plus (on alert) one ``logger.error`` call which Sentry captures.

    Emits are debounced per-process for 6h via
    ``_disk_critical_last_emit_monotonic`` so a stuck-at-99% volume
    doesn't burn Sentry quota or page-flood the operator.  The
    debounce resets automatically when usage drops back below
    threshold OR when the process restarts (deliberate — see
    ``_check_and_emit_disk_critical``).
    """
    while True:
        try:
            await asyncio.sleep(DISK_CHECK_INTERVAL_SECONDS)
        except asyncio.CancelledError:
            return

        try:
            db = SessionLocal()
            try:
                _check_and_emit_disk_critical(db)
            finally:
                db.close()
        except Exception:
            logger.exception("[DiskCheck] tick failed")


async def _motion_digest_loop():
    """Per-camera motion-email digest sweep.

    Runs every ``MOTION_DIGEST_INTERVAL_SECONDS`` (60s default).  Drains
    cooldown anchors written by the immediate-email path (see
    ``app/api/notifications.py::_claim_motion_cooldown_or_silence``).
    For each anchor whose window has expired, counts MotionEvent rows
    that landed in the window and — if there were extras AND the org
    still has email_motion enabled — emits a single ``motion_digest``
    notification (which itself enqueues an email via the standard
    create_notification path).  The anchor is deleted regardless of
    whether a digest was emitted, so the next motion event for that
    camera triggers a fresh immediate email + new anchor.

    Per-anchor try/except so one corrupt row can't poison the tick;
    the outer try/except catches anything that escapes (DB connection
    drops, session reset) so the loop survives transient failures and
    picks up where it left off on the next tick.
    """
    from app.api.notifications import (
        _motion_cooldown_minutes,
        create_notification,
        email_enabled_for_kind,
    )
    from app.models.models import Camera, MotionEvent, Setting

    while True:
        await asyncio.sleep(MOTION_DIGEST_INTERVAL_SECONDS)

        try:
            db = SessionLocal()
            try:
                now = datetime.now(tz=UTC).replace(tzinfo=None)
                anchors = (
                    db.query(Setting)
                    .filter(Setting.key.like("motion_email_cooldown_start:%"))
                    .all()
                )
                for anchor_row in anchors:
                    try:
                        # Parse camera_id back from the colon-suffixed key.
                        # ``split(":", 1)`` handles the (extremely unlikely)
                        # case of a colon in the camera_id itself.
                        if ":" not in anchor_row.key:
                            db.delete(anchor_row)
                            db.commit()
                            continue
                        camera_id = anchor_row.key.split(":", 1)[1]

                        if not anchor_row.value:
                            db.delete(anchor_row)
                            db.commit()
                            continue
                        try:
                            anchor_ts = datetime.fromisoformat(anchor_row.value)
                        except ValueError:
                            # Corrupt timestamp — drop the row so the next
                            # motion event starts fresh rather than being
                            # silenced forever by an unparseable anchor.
                            db.delete(anchor_row)
                            db.commit()
                            continue

                        cooldown_min = _motion_cooldown_minutes(db, anchor_row.org_id)
                        if (now - anchor_ts).total_seconds() < cooldown_min * 60:
                            # Window still open — leave the anchor alone.
                            continue

                        window_end = anchor_ts + timedelta(minutes=cooldown_min)
                        # Count events strictly AFTER the anchor (the immediate
                        # email already covered the anchor-time event itself)
                        # and up to the window edge.  Events past window_end
                        # belong to a later cycle — but no later cycle exists
                        # yet because the anchor is still here, so this filter
                        # is defensive against clock skew + late-arriving events.
                        extra_count = (
                            db.query(MotionEvent)
                            .filter(
                                MotionEvent.org_id == anchor_row.org_id,
                                MotionEvent.camera_id == camera_id,
                                MotionEvent.timestamp > anchor_ts,
                                MotionEvent.timestamp <= window_end,
                            )
                            .count()
                        )

                        if extra_count > 0 and email_enabled_for_kind(
                            db, anchor_row.org_id, "motion"
                        ):
                            # Re-resolve display name at digest-emit time.
                            # The camera might have been renamed since the
                            # immediate email; show the current name.
                            cam = (
                                db.query(Camera)
                                .filter_by(
                                    camera_id=camera_id,
                                    org_id=anchor_row.org_id,
                                )
                                .first()
                            )
                            display = (
                                cam.name if cam and cam.name else camera_id
                            )
                            create_notification(
                                org_id=anchor_row.org_id,
                                kind="motion_digest",
                                title=(
                                    f"{extra_count} more motion event"
                                    f"{'s' if extra_count != 1 else ''} "
                                    f"on {display}"
                                ),
                                body=(
                                    f"{extra_count} additional motion event"
                                    f"{'s were' if extra_count != 1 else ' was'} "
                                    f'detected on "{display}" in the '
                                    f"{cooldown_min}-minute window after the "
                                    f"first alert."
                                ),
                                severity="info",
                                audience="all",
                                link=f"/dashboard?camera={camera_id}",
                                camera_id=camera_id,
                                meta={
                                    "event_count": extra_count,
                                    "window_start": anchor_ts.isoformat(),
                                    "window_end": window_end.isoformat(),
                                    "cooldown_minutes": cooldown_min,
                                },
                                db=db,
                            )

                        # Always delete the anchor — window has closed.  Next
                        # motion on this camera starts a fresh cycle.
                        db.delete(anchor_row)
                        db.commit()
                    except Exception:
                        logger.exception(
                            "[MotionDigest] anchor processing failed key=%s org=%s",
                            anchor_row.key,
                            anchor_row.org_id,
                        )
                        try:
                            db.rollback()
                        except Exception:
                            pass
            finally:
                db.close()
        except Exception:
            logger.exception("[MotionDigest] tick failed")


async def _offline_sweep_loop():
    """Background task — periodically flip stale 'online' rows to 'offline'.

    A heart-beating node sends status updates every ~30s over WebSocket;
    if it crashes, the DB would sit at ``status='online'`` indefinitely
    and no transition notification would ever fire.  This sweep closes
    that gap.
    """
    # Defer imports so test environments without a full app don't choke.
    while True:
        await asyncio.sleep(OFFLINE_SWEEP_INTERVAL_SECONDS)
        try:
            db = SessionLocal()
            try:
                summary = run_offline_sweep(db)
                total = summary["nodes_flipped"] + summary["cameras_flipped"]
                if total > 0:
                    logger.info(
                        "[OfflineSweep] Flipped %d entities to offline (nodes=%d, cameras=%d)",
                        total, summary["nodes_flipped"], summary["cameras_flipped"],
                    )
            finally:
                db.close()
        except Exception:
            logger.exception("[OfflineSweep] Sweep failed")


async def _sentinel_reaper_loop():
    """Background task — mark long-stranded `running` SentinelRun rows
    as errored.

    The Sentinel agent's wall-clock-timeout cleanup wrapper (in
    ``SourceBox Sentinel/app/processor.py::process_with_timeout``)
    handles the common case: when the 540 s budget fires it
    best-effort POSTs ``/complete`` with ``outcome=error``.  But if
    the agent process crashed (OOM, panic, container kill, network
    partition long enough that the cleanup POST itself failed)
    BEFORE that wrapper ran, the row is stuck at ``outcome='running'``
    forever — list_pending only returns ``pending``, ``/start``
    doesn't re-claim ``running``, and the dashboard shows a
    perpetually-spinning indicator.

    This loop is the last-line backstop: every
    ``SENTINEL_REAPER_INTERVAL_SECONDS`` (5 min default) we sweep
    SentinelRun for rows in ``running`` state with
    ``started_at < now - STRANDED_RUN_AGE_MINUTES`` (20 min default)
    and stamp them ``error`` with a clear summary.

    Forward-compatible with the ``/complete`` error → real outcome
    upgrade path: if the agent later succeeds for a reaped run,
    its real outcome lands instead of being trapped behind the
    reaper's defensive ``error`` stamp.
    """
    from app.core.sentinel_dispatch import reap_stranded_runs
    while True:
        await asyncio.sleep(SENTINEL_REAPER_INTERVAL_SECONDS)
        try:
            db = SessionLocal()
            try:
                summary = reap_stranded_runs(db)
                if summary.get("reaped", 0) > 0:
                    logger.info(
                        "[SentinelReaper] Reaped %d stranded run(s): %s",
                        summary["reaped"],
                        ",".join(summary.get("ids", [])),
                    )
            finally:
                db.close()
        except Exception:
            logger.exception("[SentinelReaper] Sweep failed")


# ── Pre-auth rate limit for the /mcp/ ASGI mount ──────────────────────
#
# /mcp is mounted as an ASGI app (FastMCP), not a FastAPI route, so the
# Redis-backed slowapi @limiter.limit decorator that protects every
# other endpoint doesn't apply.  Each pre-auth POST to /mcp/ costs:
#
#   - 1 SHA256 of the bearer
#   - 1 indexed DB lookup against McpApiKey by key_hash
#
# Cheap, but unbounded.  An attacker who can't brute-force the 128-bit
# osc_<32 hex> keyspace can still chew CPU and DB connection budget by
# spamming random bearers.  This middleware caps the rate per tenant
# bucket BEFORE the request reaches FastMCP.
#
# Bucket key:
#   - Authenticated MCP request → IP (since the bearer is osc_..., not
#     a JWT, ``tenant_aware_key`` falls through to the IP path).
#   - Pre-auth or malformed → IP.
#
# Storage:
#   - In-process dict.  Single-VM today; if we ever go multi-VM, swap to
#     a Redis-backed `slowapi.shared_limit` or equivalent.
#
# Cap:
#   - 600/min per IP.  Generous: a legitimate MCP client makes one or
#     two requests per tool call.  At 10 calls/min steady-state with
#     headroom for bursts, 600/min is roughly 60× the worst-case real
#     usage from a single IP.  An NAT'd office of 50 users still has
#     budget; an attacker spamming with random keys hits the wall well
#     before any meaningful DoS impact.
_MCP_PRE_AUTH_LIMIT_PER_MINUTE = 600
_mcp_pre_auth_buckets: dict[str, list[float]] = defaultdict(list)
_mcp_pre_auth_lock = threading.Lock()


def _check_mcp_pre_auth_rate(request: Request) -> bool:
    """Per-tenant rate gate for /mcp/.  Returns False if over the cap.

    Sliding 60-second window — drop entries older than the cutoff,
    append the current timestamp, refuse if the bucket is full.
    Holding the lock around the whole pop+append keeps this race-free
    under the asyncio executor's thread pool without serialising the
    actual MCP request handling.
    """
    bucket_key = tenant_aware_key(request)
    now = monotonic()
    cutoff = now - 60.0
    with _mcp_pre_auth_lock:
        bucket = _mcp_pre_auth_buckets[bucket_key]
        # Drop stale entries in-place so the bucket size doesn't grow
        # unbounded for a noisy tenant.
        bucket[:] = [t for t in bucket if t > cutoff]
        if len(bucket) >= _MCP_PRE_AUTH_LIMIT_PER_MINUTE:
            return False
        bucket.append(now)
        return True


# Serve static files from the React build
static_dir = Path(__file__).parent.parent / "static"
if static_dir.exists():
    app.mount("/assets", StaticFiles(directory=static_dir / "assets"), name="assets")

    @app.middleware("http")
    async def spa_middleware(request: Request, call_next):
        # Let API, WebSocket, and install routes pass through. /docs is owned by
        # the React DocsPage; FastAPI's auto docs live at /api-docs (see ctor).
        # /downloads/ is the backend binary-redirect route (see install.py);
        # without it, /downloads/linux/x86_64 would fall through to the SPA.
        # /.well-known/ + /security.txt are RFC-9116 contact endpoints owned
        # by app/api/well_known.py — without explicit pass-through they'd be
        # served the React index.html, which would silently break security
        # scanners that grep for the file.
        if request.url.path.startswith((
            "/api", "/ws", "/install.", "/mcp-setup.", "/downloads/",
            "/.well-known/", "/security.txt",
        )):
            return await call_next(request)

        # MCP endpoint: only pass POST requests (JSON-RPC) to the MCP server;
        # GET /mcp should serve the frontend dashboard page.
        #
        # Apply a pre-auth IP/tenant rate limit BEFORE the request hits
        # FastMCP — see ``_check_mcp_pre_auth_rate`` for the rationale.
        if request.url.path.startswith("/mcp") and request.method == "POST":
            if not _check_mcp_pre_auth_rate(request):
                return JSONResponse(
                    {"error": "Too many requests. Slow down and retry shortly."},
                    status_code=429,
                    headers={"Retry-After": "60"},
                )
            return await call_next(request)

        static_file = static_dir / request.url.path.lstrip("/")
        if static_file.exists() and static_file.is_file():
            return FileResponse(static_file)

        if not request.url.path.startswith("/api"):
            index_file = static_dir / "index.html"
            if index_file.exists():
                return FileResponse(index_file)

        return await call_next(request)


@app.get("/api/health")
async def health_check():
    """Liveness check — process is up.  No dependency probes.

    Designed for Fly's per-machine HTTP health check: needs to be
    fast and never time out, otherwise Fly removes the machine from
    rotation when DB / Clerk / etc. are slow — which is the OPPOSITE
    of what we want (no other instance to fail over to; better to
    keep serving from this one + page someone for the dependency).

    Use ``/api/health/ready`` for readiness with dep probes (HTTP
    503 on critical failures, suitable for external uptime monitors).
    Use ``/api/health/detailed`` for the fully-verbose admin/status
    page snapshot.
    """
    return {"status": "healthy", "version": "2.1.2"}


# ── /api/health/ready cache ──────────────────────────────────────
# 30s TTL: long enough that a swarm of external pollers
# (BetterStack + UptimeRobot + a status page + the admin panel)
# don't hammer Clerk, short enough that a real outage surfaces
# within ~30s.  Race-condition note: two parallel cache misses
# both run probes; we accept the duplicate work in exchange for
# avoiding lock contention on the hot path.
_HEALTH_READY_CACHE_TTL_SECONDS = 30.0
# Tuple of (cached_at_monotonic, body_dict, http_status_code) — None
# until the first miss populates it.
_health_ready_cache: Optional[tuple[float, dict, int]] = None


def _reset_health_ready_cache_for_tests() -> None:
    """Tests that need per-test cache freshness call this in setup.
    Production code never calls this — the TTL handles real-world
    invalidation."""
    global _health_ready_cache
    _health_ready_cache = None


@app.get("/api/health/ready")
async def health_check_ready(nocache: bool = False):
    """Readiness check with dependency probes.

    Returns HTTP 200 if the app is ready to serve traffic; HTTP 503
    with details in the body if any critical dependency is unhealthy.

    Designed for external uptime monitors (BetterStack, UptimeRobot,
    Better Uptime, etc.) — they expect HTTP status semantics, not
    JSON-body parsing.  ``/api/health/detailed`` always returns 200
    with status nested in the body for the dashboard polling case.

    Probes:
      - database     : SELECT 1 — critical
      - clerk        : reachability — critical (auth-blocking)
      - disk         : >= 95% used — critical
      - email_worker : tick within 60s when email enabled — critical

    Cached for 30s so a swarm of pollers doesn't hammer Clerk's API
    on every request.  Pass ``?nocache=1`` to bypass (diagnostics
    only — don't wire this into automated polling).
    """
    from app.core.health_probes import run_readiness_probes

    global _health_ready_cache
    now_mono = time.monotonic()
    if not nocache and _health_ready_cache is not None:
        cached_at, cached_body, cached_status = _health_ready_cache
        if now_mono - cached_at < _HEALTH_READY_CACHE_TTL_SECONDS:
            return JSONResponse(content=cached_body, status_code=cached_status)

    uptime_s = now_mono - _STARTED_AT_MONO
    report = await run_readiness_probes(uptime_s)
    body = {
        **report.to_dict(),
        "version": "2.1.2",
        "uptime_seconds": round(uptime_s, 3),
    }
    status_code = 200 if report.ready else 503

    _health_ready_cache = (now_mono, body, status_code)
    return JSONResponse(content=body, status_code=status_code)


@app.get("/api/health/detailed")
async def health_check_detailed():
    """Verbose health/status endpoint — intended for status-page polling
    and on-call diagnostics, not for load balancers (those should poll
    ``/api/health`` so a slow DB doesn't cascade into removing the
    machine from rotation).

    Public on purpose: a status page tool needs to read it from off-net,
    and the surface here is deliberately metric-shaped — DB ping ms,
    cache occupancy counts, queue depths — never org IDs, camera IDs,
    user emails, or anything that would leak business intelligence to
    a competitor scraping the endpoint.

    Always returns 200; the rolled-up status is in the body.  For an
    HTTP-status-driven check (uptime monitors, k8s-style readiness),
    use ``/api/health/ready`` instead.

    Status semantics:
      - "healthy"    — every check passed.
      - "degraded"   — non-critical subsystem reporting a warning (e.g.
                       a viewer-usage flush queue that's growing faster
                       than it drains). The app still serves traffic
                       correctly; just keep an eye on it.
      - "unhealthy"  — DB / Clerk / disk-95% / wedged email worker.
                       The app is up but cannot serve most
                       reads/writes correctly. Pages should fire.
    """
    # Defer import: hls.py pulls in storage helpers that are heavier
    # than this endpoint should pay for on cold start.
    from app.api.hls import (
        _pending_viewer_seconds,
        _playlist_cache,
        _segment_cache,
    )
    from app.api.notifications import notification_broadcaster
    from app.core.health_probes import (
        probe_clerk,
        probe_database,
        probe_disk,
        probe_email_worker,
    )

    now_wall = datetime.now(tz=UTC)
    uptime_s = round(time.monotonic() - _STARTED_AT_MONO, 3)

    # ── Probes shared with /api/health/ready ─────────────────
    # Use the same probe functions so the two endpoints can't
    # disagree ("ready says we're up, detailed says we're down").
    # Run concurrently so the Clerk timeout doesn't serialise
    # behind the DB ping.
    db_probe, clerk_probe, disk_probe, worker_probe = await asyncio.gather(
        asyncio.to_thread(probe_database),
        probe_clerk(),
        asyncio.to_thread(probe_disk),
        asyncio.to_thread(probe_email_worker, uptime_s),
    )
    db_check = db_probe.to_dict()
    clerk = clerk_probe.to_dict()
    disk = disk_probe.to_dict()
    email_worker_check = worker_probe.to_dict()

    # ── In-memory subsystem snapshots ────────────────────────
    # All read without locks: a momentary inconsistency in a count is
    # fine for a status page; not worth blocking the hot path.
    hls_cache = {
        "status": "ok",
        "playlists_cached": len(_playlist_cache),
        "segment_cameras": len(_segment_cache),
    }

    pending_views = sum(_pending_viewer_seconds.values())
    viewer_usage = {
        "status": "ok",
        "pending_writes": pending_views,
    }
    # The flush loop ticks every 60s; if pending grows past a threshold
    # the loop is probably failing silently. ``warn`` doesn't gate
    # liveness — the app keeps serving — but a status page would render
    # this as yellow.
    if pending_views > 100_000:
        viewer_usage["status"] = "warn"

    sse = {
        "status": "ok",
        "subscriber_orgs": len(notification_broadcaster._subscribers),
        "subscriber_total": sum(
            len(s) for s in notification_broadcaster._subscribers.values()
        ),
    }

    # ── Email transport (Resend) ─────────────────────────────
    # Three states surface separately so an operator can tell
    # "I forgot to set the secret" from "I left the kill-switch off"
    # at a glance.  Queue depth is the worker's backlog — a steady
    # non-zero value means the worker is running but sends are
    # failing (or Resend is rate-limiting us); a spike followed by
    # decay is normal.
    resend_status = "ok"
    if not settings.EMAIL_ENABLED:
        resend_status = "disabled"
    elif not (settings.RESEND_API_KEY and settings.EMAIL_FROM_ADDRESS):
        resend_status = "unconfigured"
    queue_depth = 0
    try:
        from app.models.models import EmailOutbox
        db = SessionLocal()
        try:
            queue_depth = (
                db.query(EmailOutbox)
                .filter(EmailOutbox.status == "pending")
                .count()
            )
        finally:
            db.close()
    except Exception:
        # Don't fail the health endpoint over a count query.  Surface
        # the count as -1 so a status page can flag "we don't know."
        logger.warning("[Health] EmailOutbox count query failed", exc_info=True)
        queue_depth = -1
    resend = {"status": resend_status, "queue_depth": queue_depth}

    # ── Roll up overall status ───────────────────────────────
    # Critical-tier probes (any from health_probes.py reporting
    # ``critical``) flip overall to "unhealthy" — pager-grade.  The
    # readiness endpoint also returns 503 in this state, so external
    # uptime monitors page the same set of failures the dashboard
    # renders red.
    #
    # Warn-tier signals (viewer-usage queue building, disk in 80-94%
    # range) flip overall to "degraded" — yellow on the dashboard,
    # nothing pages.
    #
    # Resend "unconfigured" / "disabled" intentionally do NOT degrade
    # the rollup — an org running without email is a deliberate
    # configuration choice, not a failure mode.
    critical_probes = [db_check, clerk, disk, email_worker_check]
    if any(p.get("status") == "critical" for p in critical_probes):
        overall = "unhealthy"
    elif viewer_usage["status"] == "warn" or disk["status"] == "warn":
        overall = "degraded"
    else:
        overall = "healthy"

    return {
        "status": overall,
        "version": "2.1.2",
        "uptime_seconds": uptime_s,
        "started_at": _STARTED_AT_WALL.isoformat(),
        "time": now_wall.isoformat(),
        "checks": {
            "database": db_check,
            "clerk": clerk,
            "disk": disk,
            "email_worker": email_worker_check,
            "hls_cache": hls_cache,
            "viewer_usage": viewer_usage,
            "sse": sse,
            "resend": resend,
        },
    }
