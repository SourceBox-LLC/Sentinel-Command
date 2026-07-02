"""Sentry error tracking — backend integration.

Philosophy:
- If ``SENTRY_DSN`` is unset, this module is a no-op. Local dev and tests
  never send events; nothing breaks when credentials are missing.
- PII is OFF by default (``send_default_pii=False``). Auth headers, cookies,
  and request bodies are never transmitted unless we explicitly attach them
  via a scope. The auth layer adds the ``org_id`` / ``user_id`` tags it needs
  for triage; nothing more.
- Performance sampling is modest (10%) to stay inside Sentry's free tier.
  Bump when real traffic justifies the event budget.

See ``init_sentry()`` for the one public entry point; callers should invoke
it once at app startup before the first request is accepted.
"""

from __future__ import annotations

import logging
import os
from typing import Any, Optional

logger = logging.getLogger(__name__)

# Track initialization so tests (and repeat startup paths) don't double-init.
_initialized = False


def init_sentry(
    dsn: Optional[str] = None,
    environment: Optional[str] = None,
    release: Optional[str] = None,
    traces_sample_rate: float = 0.1,
) -> bool:
    """Initialize Sentry if a DSN is configured.

    Returns True when Sentry was actually initialized, False when we skipped
    (no DSN, already initialized, or SDK import failed). Callers usually
    don't care about the return value; it's mostly for tests.

    We catch ImportError because the SDK is a real dependency but we'd
    rather run with error tracking disabled than crash at startup if the
    install is somehow broken. Catching generic ``Exception`` on ``init``
    itself is defensive for the same reason — nothing about a monitoring
    tool should be able to take the app down.
    """
    global _initialized
    if _initialized:
        return False

    resolved_dsn = dsn or os.getenv("SENTRY_DSN", "").strip()
    if not resolved_dsn:
        logger.info("[Sentry] SENTRY_DSN not set — error tracking disabled")
        return False

    try:
        import sentry_sdk
        from sentry_sdk.integrations.fastapi import FastApiIntegration
        from sentry_sdk.integrations.logging import LoggingIntegration
        from sentry_sdk.integrations.sqlalchemy import SqlalchemyIntegration
        from sentry_sdk.integrations.starlette import StarletteIntegration
    except ImportError:
        logger.exception("[Sentry] sentry-sdk import failed — skipping init")
        return False

    # Fly injects these automatically when deployed; fall back to sensible
    # defaults for local dev so dashboards don't group every dev run as
    # "production".
    resolved_env = environment or os.getenv("SENTRY_ENVIRONMENT") or (
        "production" if os.getenv("FLY_APP_NAME") else "development"
    )
    resolved_release = release or os.getenv("SENTRY_RELEASE") or os.getenv("FLY_MACHINE_VERSION")

    try:
        sentry_sdk.init(
            dsn=resolved_dsn,
            environment=resolved_env,
            release=resolved_release,
            # Explicit False — we don't want auth headers, IPs, or cookies
            # leaving the app unless we pin them intentionally via scope.
            send_default_pii=False,
            traces_sample_rate=traces_sample_rate,
            # Keep profiling off until we have a reason; it doubles event
            # count and we're on the free tier.
            profiles_sample_rate=0.0,
            integrations=[
                StarletteIntegration(transaction_style="endpoint"),
                FastApiIntegration(transaction_style="endpoint"),
                SqlalchemyIntegration(),
                # Capture log records at ERROR+ as events, and INFO+ as
                # breadcrumbs — gives us leading context on every error
                # without spamming the event store.
                LoggingIntegration(level=logging.INFO, event_level=logging.ERROR),
            ],
            before_send=_scrub_event,
        )
    except Exception:
        logger.exception("[Sentry] init failed — error tracking disabled")
        return False

    _initialized = True
    logger.info(
        "[Sentry] initialized — env=%s release=%s trace_rate=%.2f",
        resolved_env, resolved_release or "(unset)", traces_sample_rate,
    )
    return True


def _scrub_event(event: dict, hint: dict) -> Optional[dict]:
    """Last-line scrubber for sensitive data Sentry might pick up anyway.

    ``send_default_pii=False`` already strips most of this, but ``request``
    can still carry a query string that includes an API key (the CameraNode
    WebSocket handshake takes ``api_key=`` in the URL, for example).  Drop
    the query string entirely and scrub a few header/body keys by name.
    Dropping the whole event is reserved for cases where scrubbing can't
    guarantee cleanliness.
    """
    request = event.get("request")
    if isinstance(request, dict):
        # Kill query strings — cheap and removes `?api_key=…` entirely.
        if "query_string" in request:
            request["query_string"] = ""
        url = request.get("url")
        if isinstance(url, str) and "?" in url:
            request["url"] = url.split("?", 1)[0]

        headers = request.get("headers")
        if isinstance(headers, dict):
            for name in list(headers.keys()):
                lname = name.lower()
                if lname in {
                    "authorization",
                    "cookie",
                    "x-node-api-key",
                    # Shared service secrets — never let them ride into
                    # Sentry if header capture is ever enabled.
                    "x-sentinel-agent-key",
                    "x-agent-org-override",
                }:
                    headers[name] = "[redacted]"

    # Tags are our own — keep them. Extras are our own — keep them.
    return event


def capture_exception(exc: BaseException, **tags: Any) -> None:
    """Thin wrapper so callers don't import ``sentry_sdk`` directly.

    No-op when Sentry isn't initialized, so it's safe to sprinkle into
    code that runs in tests.
    """
    if not _initialized:
        return
    try:
        import sentry_sdk

        with sentry_sdk.new_scope() as scope:
            for key, value in tags.items():
                scope.set_tag(key, value)
            sentry_sdk.capture_exception(exc)
    except Exception:
        # Monitoring failures never propagate.
        logger.exception("[Sentry] capture_exception failed")


def set_user_context(
    *,
    user_id: Optional[str] = None,
    org_id: Optional[str] = None,
    plan: Optional[str] = None,
) -> None:
    """Attach per-request identity tags to the current Sentry scope.

    Called from the auth layer once a token has been validated. Only the
    fields we care about for triage — no email, no IP, no name.
    No-op when Sentry isn't initialized.
    """
    if not _initialized:
        return
    try:
        import sentry_sdk

        scope = sentry_sdk.get_current_scope()
        if user_id:
            scope.set_tag("user_id", user_id)
            scope.set_user({"id": user_id})
        if org_id:
            scope.set_tag("org_id", org_id)
        if plan:
            scope.set_tag("plan", plan)
    except Exception:
        logger.exception("[Sentry] set_user_context failed")


def is_initialized() -> bool:
    """Mostly for tests and the health endpoint."""
    return _initialized


def _reset_for_tests() -> None:
    """Explicit reset hook used by the test suite. Never call in prod."""
    global _initialized
    _initialized = False
