"""
Shared rate limiter.

Keys requests by tenant where possible so one loud tenant can't starve
the global bucket for everyone else:

  1. ``X-Node-API-Key`` header → ``node:<sha256-prefix>``
     — CameraNodes get their own bucket, scoped to the node.
  2. ``Authorization: Bearer <jwt>`` → ``org:<org_id>``
     — Authenticated end-user requests share a bucket per-org.  The JWT
       payload is base64-decoded WITHOUT verification; it's only used
       as a bucket selector.  Full verification still happens in
       ``get_current_user`` before the endpoint body runs.
  3. Fallback → real client IP — used for sign-in, webhooks, and other
     unauthenticated routes.  Read from ``Fly-Client-IP`` (set by Fly's
     proxy and stripped from incoming requests) so the edge IP isn't
     what we bucket on.  Without this every unauthed request would
     appear to come from a handful of proxy IPs and the per-IP limit
     would collapse into a global one.

Decoding the JWT unverified is safe for rate-limiting because an
attacker who forges a token with a different ``org_id`` only moves
themselves into a different bucket — they can't escape limits
entirely, and real auth still rejects them.

Storage backend: if ``REDIS_URL`` is configured the limiter uses Redis
so counts are shared across VMs.  Without it, slowapi keeps counters
in-process — fine for local dev but **broken for multi-instance
production** (an attacker round-robining across N VMs gets N× the
nominal limit).  We log a loud warning when falling back.
"""

from __future__ import annotations

import base64
import hashlib
import json
import logging

from fastapi import Request
from slowapi import Limiter
from slowapi.util import get_remote_address

from app.core.config import settings

logger = logging.getLogger(__name__)


def _extract_org_from_jwt(token: str) -> str | None:
    """Pull ``org_id`` out of an unverified JWT payload. Returns None on
    anything unparseable — callers fall back to IP in that case."""
    try:
        parts = token.split(".")
        if len(parts) != 3:
            return None
        # Pad to a multiple of 4 so urlsafe_b64decode doesn't choke.
        payload_b64 = parts[1] + "=" * (-len(parts[1]) % 4)
        claims = json.loads(base64.urlsafe_b64decode(payload_b64))
        # V1 flat claim or V2 compact "o" claim.
        org_id = claims.get("org_id") or claims.get("o", {}).get("id")
        if isinstance(org_id, str) and org_id:
            return org_id
    except Exception:
        return None
    return None


def _real_client_ip(request: Request) -> str:
    """Return the real client IP, preferring Fly.io's trusted header.

    Fly's proxy strips ``Fly-Client-IP`` from incoming requests before
    forwarding, so any value we see was set by the proxy itself — it's
    safe to trust.  Falls back to the first ``X-Forwarded-For`` entry
    (next most common proxy convention) and finally the socket remote
    address via slowapi's helper.
    """
    fly_ip = request.headers.get("Fly-Client-IP")
    if fly_ip:
        return fly_ip.strip()

    xff = request.headers.get("X-Forwarded-For")
    if xff:
        # XFF is a comma-separated chain appended as the request hops
        # through proxies; the left-most entry is the originating client.
        first = xff.split(",")[0].strip()
        if first:
            return first

    return get_remote_address(request)


def tenant_aware_key(request: Request) -> str:
    """Rate-limit bucket key for a request.  See module docstring."""
    # CameraNode requests — one bucket per node, identified by the hash
    # prefix of its API key (not the raw key — never log or bucket on that).
    node_key = request.headers.get("X-Node-API-Key")
    if node_key:
        digest = hashlib.sha256(node_key.encode()).hexdigest()[:16]
        return f"node:{digest}"

    # End-user requests — one bucket per org.
    auth = request.headers.get("Authorization", "")
    if auth.startswith("Bearer "):
        org_id = _extract_org_from_jwt(auth[7:])
        if org_id:
            return f"org:{org_id}"

    # Unauthenticated fallback — real client IP, not the proxy IP.
    return _real_client_ip(request)


def _build_limiter() -> Limiter:
    """Build the Limiter, wiring up Redis storage when configured.

    We build the kwargs dict conditionally so we don't pass
    ``storage_uri=None`` (slowapi rejects that) and so the warning
    about missing Redis only fires once per process.
    """
    kwargs: dict = {"key_func": tenant_aware_key}
    if settings.REDIS_URL:
        kwargs["storage_uri"] = settings.REDIS_URL
        logger.info("[Limiter] Using Redis storage for rate limits")
    else:
        logger.warning(
            "[Limiter] REDIS_URL not set — falling back to in-memory storage. "
            "Rate limits will NOT hold across multiple VMs; set REDIS_URL in "
            "production to close this gap.",
        )
    return Limiter(**kwargs)


# Shared rate limiter — per-tenant where possible, per-IP otherwise.
# Import this in any router module that needs rate limiting.
limiter = _build_limiter()
