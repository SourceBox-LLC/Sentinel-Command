"""
Signed unsubscribe tokens for email footers.

Each email's footer includes an unsubscribe link of the form

    ``{frontend}/api/notifications/email/unsubscribe?t=<jwt>``

The token carries (org_id, kind, rcpt, exp) signed with a derived
server secret, so clicking the link needs no authentication.

Semantics (v2, launch hardening): the click suppresses **that
recipient address** (an ``EmailSuppression`` row the send worker
already honours), NOT the whole org's toggle.  v1 flipped the org-wide
``email_<kind>`` Setting — which meant any past recipient, including a
member since REMOVED from the org, could permanently disable an org's
security-alert emails from an old message in their inbox.  Per-address
suppression is also what the unsubscribe promise actually means
("stop emailing ME"), and matches CAN-SPAM's intent.

Token hygiene (v2):
  - Signed with a secret DERIVED from CLERK_SECRET_KEY via HMAC with a
    fixed domain label — the raw Clerk key itself never signs a
    public-facing surface, and the derived key is 64 hex chars (kills
    the short-HMAC-key warning).  No hardcoded test fallback: an
    instance with no Clerk secret refuses to mint and rejects all
    tokens (fail closed).
  - ``exp`` at 400 days.  CAN-SPAM requires the link to work for at
    least 30 days after send; 400 covers any realistic inbox archaeology
    while ensuring a leaked link is not a forever-credential.

Why JWT and not a database token table:
  - Tokens are issued at email-send time and consumed at click time;
    rate of issuance dwarfs rate of consumption (every send vs. one
    click).  Persisting them would mean a write per send for an
    artifact almost no one ever touches.
  - JWT signed-with-secret achieves the same security as DB-token
    lookup at zero storage cost.  Forging a token requires the secret;
    rotating CLERK_SECRET_KEY invalidates every outstanding link, which
    is the right behaviour after a key compromise.
"""

from __future__ import annotations

import hashlib
import hmac as _hmac
import logging
import time
from typing import Optional

import jwt

from app.core.config import settings

logger = logging.getLogger(__name__)


_JWT_ALGORITHM = "HS256"
# CAN-SPAM floor is 30 days; 400 days ≈ "any email you can still find"
# without being a forever-credential.
_TOKEN_TTL_SECONDS = 400 * 24 * 3600
# Domain-separation label — bump the suffix to rotate every outstanding
# unsubscribe link without touching the Clerk key.
_DERIVE_LABEL = b"sentinel-email-unsubscribe-v1"


def _get_secret() -> Optional[str]:
    """Derive the signing secret from CLERK_SECRET_KEY.

    Returns None when the Clerk secret is unset — callers fail closed
    (mint raises, verify rejects).  The old behaviour fell back to a
    hardcoded ``test-secret-not-for-production`` string, which made the
    PUBLIC unsubscribe endpoint forgeable on any misconfigured deploy.
    """
    base = settings.CLERK_SECRET_KEY
    if not base:
        return None
    return _hmac.new(base.encode("utf-8"), _DERIVE_LABEL, hashlib.sha256).hexdigest()


def make_token(org_id: str, kind: str, recipient: str) -> str:
    """Sign an unsubscribe JWT for ``(org_id, kind, recipient)``.

    ``recipient`` is the destination address — the click suppresses
    exactly that address.  Raises RuntimeError when no signing secret
    is configured (an email could never have been sent in that state
    anyway).
    """
    secret = _get_secret()
    if secret is None:
        raise RuntimeError(
            "email_unsubscribe: CLERK_SECRET_KEY unset — cannot sign tokens"
        )
    now = int(time.time())
    payload = {
        "org_id": org_id,
        "kind": kind,
        "rcpt": (recipient or "").strip().lower(),
        "iat": now,
        "exp": now + _TOKEN_TTL_SECONDS,
        "sub": "email-unsubscribe",
    }
    return jwt.encode(payload, secret, algorithm=_JWT_ALGORITHM)


def verify_token(token: str) -> Optional[tuple[str, str, str]]:
    """Decode + verify an unsubscribe token.

    Returns ``(org_id, kind, recipient)`` on success, ``None`` on any
    failure (bad signature, expired, malformed, missing claims, no
    secret configured).  Failure is logged at INFO so a stream of bad
    tokens is visible without hitting WARN noise floors — most likely
    cause in production is an old email after a secret rotation, not
    an attack.
    """
    if not token or not isinstance(token, str):
        return None
    secret = _get_secret()
    if secret is None:
        logger.info("[Unsubscribe] no signing secret configured — rejecting")
        return None
    try:
        claims = jwt.decode(
            token,
            secret,
            algorithms=[_JWT_ALGORITHM],
            options={"require": ["org_id", "kind", "rcpt", "exp", "sub"]},
        )
    except jwt.InvalidTokenError as exc:
        logger.info("[Unsubscribe] token verify failed: %s", type(exc).__name__)
        return None

    if claims.get("sub") != "email-unsubscribe":
        # Wrong subject — refuse even with a valid signature.  Defends
        # against future tokens with the same key being misused.
        logger.info("[Unsubscribe] token sub mismatch")
        return None

    org_id = claims.get("org_id")
    kind = claims.get("kind")
    rcpt = claims.get("rcpt")
    if not org_id or not kind or not rcpt:
        return None
    return (org_id, kind, rcpt)


def build_unsubscribe_url(org_id: str, kind: str, recipient: str) -> str:
    """Construct the full clickable URL for one recipient's footer."""
    token = make_token(org_id, kind, recipient)
    base = (settings.FRONTEND_URL or "").rstrip("/")
    return f"{base}/api/notifications/email/unsubscribe?t={token}"
