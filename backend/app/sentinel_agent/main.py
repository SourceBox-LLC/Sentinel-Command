"""
SourceBox Sentinel — Starlette ASGI app.

Endpoints:
    GET  /health     — liveness probe (Fly health checks)
    POST /wakeup     — webhook receiver from Command Center.  Each
                       inbound POST drains pending sentinel_runs
                       from CC, processes them with the LLM ↔ MCP
                       agent loop, posts results back, and returns.
                       Fly auto-stops the machine after the request
                       finishes and the idle window passes.
    POST /          — DEV-ONLY direct trigger (skips HMAC).  Useful
                       for `curl localhost:8080/` during local
                       development.  Disabled in production by
                       leaving SENTINEL_AGENT_KEY unset (the wakeup
                       handler then hard-rejects everything anyway).

Webhook signing:
    Command Center signs each webhook payload with HMAC-SHA256 over
    the raw request body using SENTINEL_AGENT_KEY as the secret.
    The signature is sent as ``sha256=<hex>`` in the
    X-Sentinel-Signature header.  We verify with constant-time
    comparison before doing any work.

Auto-stop interaction:
    Command Center's wakeup client times out at ~5s, so the HTTP
    connection that triggered a drain closes long before the drain
    finishes — Fly then sees an idle machine and SIGINTs it mid-
    drain (~30s idle window).  Survival is the kill_timeout grace:
    uvicorn's graceful shutdown waits for the still-running handler,
    and fly.toml pins kill_timeout=300 (Fly's max) before SIGKILL.
    The drain wall-clock (process_with_timeout, 270s) is deliberately
    under that so timeout cleanup always beats SIGKILL; anything still
    pending is re-fired by CC onto a fresh machine.
"""

from __future__ import annotations

import asyncio
import hashlib
import hmac
import json
import logging
import time
from contextlib import asynccontextmanager

from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

from app.sentinel_agent.config import Settings
from app.sentinel_agent.llm import LLMProvider
from app.sentinel_agent.processor import process_with_timeout
from app.sentinel_agent.sentinel_client import SentinelClient

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

# Settings + LLM provider initialised once at module import — both are
# cheap to keep around across requests, and constructing them on every
# wakeup would add latency to a path that's already cold-start sensitive.
settings = Settings()

# Error tracking — initialise as early as possible so import-time and
# boot failures are captured too.  No-ops when SENTRY_DSN is unset, so
# local dev and un-provisioned deploys are unaffected.  logger.exception
# calls across the drain path then surface as Sentry events instead of
# scrolling off an asleep machine's ephemeral logs.
if settings.sentry_dsn:
    import sentry_sdk

    sentry_sdk.init(
        dsn=settings.sentry_dsn,
        environment=settings.sentry_environment,
        traces_sample_rate=settings.sentry_traces_sample_rate,
    )
    logger.info("Sentry error tracking enabled")
llm_provider = LLMProvider(settings)

# Fail closed on the one-flag footgun: with verification off, /wakeup
# accepts anyone and the dev `/` route opens — anonymous LLM-spend
# triggers plus cross-org drain summaries.  If a production agent key
# is configured, signature verification must be on; refuse to boot in
# the contradictory state rather than run wide open.
if settings.sentinel_agent_key and not settings.webhook_verify_signature:
    raise RuntimeError(
        "SENTINEL_AGENT_KEY is configured but WEBHOOK_VERIFY_SIGNATURE is "
        "off — refusing to start with an unauthenticated drain surface. "
        "Enable verification (production) or unset the key (local dev)."
    )

# One drain at a time per process.  Overlapping wakeups (CC re-fires
# for stale pending runs; motion bursts) otherwise ran concurrent
# drains over the same pending list — double LLM spend and, before
# CC's /start returned a claimed flag, duplicate incidents.
_drain_lock = asyncio.Lock()


def _verify_signature(raw_body: bytes, signature_header: str | None, secret: str) -> bool:
    """Constant-time HMAC-SHA256 verification.

    ``signature_header`` shape: ``sha256=<hex>``.  We strip the prefix,
    compute our own digest over the raw body, and ``hmac.compare_digest``
    them.  Returns False on any malformation rather than raising — the
    caller turns that into a 401.
    """
    if not signature_header or not secret:
        return False
    if not signature_header.startswith("sha256="):
        return False
    expected = hmac.new(secret.encode("utf-8"), raw_body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(signature_header[len("sha256="):], expected)


async def _drain_once(reason: str) -> dict | None:
    """Drain pending runs. Returns None if a drain was already running.

    Deliberately the single implementation behind both entry points —
    the webhook and the poll loop. Two copies of this would be two
    places for the concurrency guard to be got subtly wrong, and the
    guard is the part that matters: a second concurrent drain would
    re-claim rows the first is already working.

    The connection is pooled across the whole drain because N runs
    means 1 + 2*N requests to Command Center, all to the same host with
    the same bearer; without reuse that is a TLS handshake per call.
    """
    if _drain_lock.locked():
        # A drain already running will pick up every pending row — it
        # lists fresh from CC. Acknowledging is correct; starting a
        # second concurrent drain is not.
        logger.info("%s: drain already in progress — acknowledged", reason)
        return None

    async with _drain_lock:
        async with SentinelClient(
            base_url=settings.opensentry_api_base,
            agent_key=settings.sentinel_agent_key,
        ) as sentinel:
            summary = await process_with_timeout(settings, llm_provider, sentinel)
    logger.info("%s: drained — %s", reason, summary)
    return summary


async def health(_request: Request) -> JSONResponse:
    # Bare on purpose: this is public, and the old body advertised the
    # model id, which secrets are configured, and whether HMAC
    # verification is on — pure recon for an attacker.
    return JSONResponse({"status": "ok"})


async def wakeup(request: Request) -> JSONResponse:
    """Webhook receiver.  Drains pending runs from Command Center.

    Body is opaque to us — Command Center may send {} or trigger
    metadata; either way our job is "drain pending runs."  We still
    require a valid HMAC signature over the body so a leaked URL
    can't trigger us at random.
    """
    raw_body = await request.body()

    if settings.webhook_verify_signature:
        sig = request.headers.get("X-Sentinel-Signature")
        if not _verify_signature(raw_body, sig, settings.sentinel_agent_key):
            logger.warning("wakeup: signature verification failed")
            return JSONResponse({"error": "invalid signature"}, status_code=401)

        # Replay rejection.  Command Center signs a timestamped body
        # ({"ts": <unix>}) precisely so a captured request can't be
        # replayed forever to force cold-starts.  Reject anything
        # outside a ±5 min skew window.  Back-compat: an old CC sending
        # the legacy static `{}` body has no ts — accept it (signature
        # already verified) so the two services can deploy in either
        # order; once both sides are current, every request carries ts.
        try:
            body_ts = json.loads(raw_body or b"{}").get("ts")
        except ValueError:
            body_ts = None
        if body_ts is not None:
            try:
                skew = abs(time.time() - float(body_ts))
            except (TypeError, ValueError):
                skew = None
            if skew is None or skew > 300:
                logger.warning(
                    "wakeup: stale/invalid timestamp (skew=%s) — possible replay",
                    skew,
                )
                return JSONResponse({"error": "stale timestamp"}, status_code=401)

    if not settings.sentinel_agent_key:
        return JSONResponse(
            {"error": "agent key not configured"},
            status_code=503,
        )
    if not settings.opensentry_mcp_agent_key:
        return JSONResponse(
            {"error": "MCP agent key not configured"},
            status_code=503,
        )

    # Pool the httpx connection across the whole drain — N runs means
    # 1 + 2*N requests against CC, all to the same host with the same
    # bearer.  Without context-manager reuse we'd pay a TLS handshake
    # per call.
    summary = await _drain_once("wakeup")
    if summary is None:
        return JSONResponse({"ok": True, "already_draining": True})
    return JSONResponse({"ok": True, "summary": summary})


async def dev_trigger(request: Request) -> JSONResponse:
    """DEV-ONLY: trigger a drain without HMAC verification.

    Hard-disabled when WEBHOOK_VERIFY_SIGNATURE is set.  Useful for
    `curl -X POST http://localhost:8080/` during local development
    when you want to test the drain path without computing an HMAC
    by hand.
    """
    if settings.webhook_verify_signature:
        return JSONResponse(
            {"error": "dev trigger disabled — verify signature is on"},
            status_code=403,
        )

    if not settings.opensentry_mcp_agent_key:
        return JSONResponse(
            {"error": "MCP agent key not configured"},
            status_code=503,
        )

    async with SentinelClient(
        base_url=settings.opensentry_api_base,
        agent_key=settings.sentinel_agent_key,
    ) as sentinel:
        summary = await process_with_timeout(settings, llm_provider, sentinel)
    return JSONResponse({"ok": True, "summary": summary, "mode": "dev"})


# ── Poll mode ────────────────────────────────────────────────────────
_poll_task: asyncio.Task | None = None


async def _poll_loop() -> None:
    """Ask Command Center for pending work on an interval.

    Exists so the agent can run somewhere Command Center cannot reach
    it — behind NAT on a home or office network. All traffic is
    outbound, which is the same shape CameraNode already uses to talk
    to Command Center.

    The loop must outlive individual failures: Command Center being
    briefly unreachable, a bad gateway, a DNS blip. If an exception
    escaped here the task would die and the agent would go quiet
    forever while runs piled up on CC, with nothing but one stack trace
    at the moment it happened to explain it. So every iteration is
    guarded and the loop continues.

    CancelledError is re-raised rather than swallowed, so shutdown is
    prompt instead of waiting out a full interval.
    """
    interval = settings.poll_interval_seconds
    logger.info("poll: starting — every %.0fs against %s",
                interval, settings.opensentry_api_base)
    while True:
        try:
            await asyncio.sleep(interval)
            await _drain_once("poll")
        except asyncio.CancelledError:
            logger.info("poll: stopping")
            raise
        except Exception:
            # Deliberately broad: see docstring. A transient CC outage
            # must not permanently stop the agent.
            logger.exception("poll: iteration failed — continuing")


@asynccontextmanager
async def _lifespan(_app):
    """Start/stop the poll loop with the app.

    Uses the lifespan protocol rather than on_startup/on_shutdown:
    Starlette removed those kwargs, and `starlette>=0.40.0` in
    requirements.txt has no upper bound, so the older API would break
    the app on boot after an unrelated rebuild. (That is not
    hypothetical — it is exactly how the mcp 2.x rename took this
    service down on 2026-09-07.)
    """
    global _poll_task
    if settings.agent_mode == "poll":
        _poll_task = asyncio.create_task(_poll_loop())
    else:
        logger.info(
            "push mode — waiting for POST /wakeup from %s",
            settings.opensentry_api_base,
        )

    try:
        yield
    finally:
        # Await the cancellation rather than firing and forgetting: a
        # drain started by the final poll tick would otherwise be torn
        # down mid-run, stranding a claimed run on Command Center until
        # its reaper picks it up ~20 minutes later.
        if _poll_task is not None:
            _poll_task.cancel()
            try:
                await _poll_task
            except asyncio.CancelledError:
                pass


app = Starlette(
    routes=[
        Route("/health", health, methods=["GET"]),
        # Still mounted in poll mode: a valid way to force an immediate
        # drain, and harmless when nothing calls it.
        Route("/wakeup", wakeup, methods=["POST"]),
        Route("/", dev_trigger, methods=["POST"]),
    ],
    lifespan=_lifespan,
)
