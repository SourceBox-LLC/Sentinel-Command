"""Local (self-hosted) admin login — used only when AUTH_PROVIDER=local.

Self-hosted installs have exactly one fixed admin account (no invite
flow, no local User table, no multi-org) configured via env vars:
LOCAL_ADMIN_USERNAME + LOCAL_ADMIN_PASSWORD_HASH (an argon2 hash — see
backend/scripts/hash_local_admin_password.py for generating one) and
LOCAL_ADMIN_EMAIL for notification recipients.

Session tokens are HS256 JWTs signed with APP_SECRET_KEY, mirroring the
existing pattern in app/core/email_unsubscribe.py (which already signs
JWTs with pyjwt — no new crypto library needed there).

Token lifetime is intentionally long (30 days, not a short-lived
access token): this is a security-camera dashboard plausibly left open
unattended on a wall-mounted display. Clerk refreshes its own tokens
invisibly forever; a short local expiry would hard-log-out such a
session the next time a long-lived SSE connection reconnects. See
``refresh_token`` for renewing a token before it expires instead of
forcing a fresh login.
"""

from __future__ import annotations

import hmac
import logging
import time
from typing import Optional

import jwt
from argon2 import PasswordHasher
from argon2.exceptions import VerifyMismatchError

from app.core.config import settings

logger = logging.getLogger(__name__)

_JWT_ALGORITHM = "HS256"
_TOKEN_TTL_SECONDS = 30 * 24 * 3600
# refresh_token() below re-signs any still-valid token unconditionally —
# it has no threshold of its own. The "only refresh once life is short"
# decision is made client-side (frontend/src/auth/local.jsx's
# REFRESH_THRESHOLD_MS), which decides WHEN to call this endpoint at
# all. Keep the two in sync if you change one.

LOCAL_USER_ID = "local-admin"
_JWT_SUBJECT = "sentinel-local-auth"

_hasher = PasswordHasher()


def verify_local_credentials(username: str, password: str) -> bool:
    """Check a username/password pair against the configured local admin.

    Returns False (never raises) on any mismatch, missing config, or
    malformed stored hash — a misconfigured install should fail closed,
    not 500.
    """
    if not settings.LOCAL_ADMIN_USERNAME or not settings.LOCAL_ADMIN_PASSWORD_HASH:
        logger.warning(
            "[LocalAuth] login attempted but LOCAL_ADMIN_USERNAME/"
            "LOCAL_ADMIN_PASSWORD_HASH is not configured"
        )
        return False

    # Encode to bytes before comparing: hmac.compare_digest on `str`
    # rejects non-ASCII input with a TypeError, and `username` here is
    # unvalidated attacker-controlled input from a public login request.
    username_ok = hmac.compare_digest(
        username.encode("utf-8"), settings.LOCAL_ADMIN_USERNAME.encode("utf-8")
    )

    # Always run the (deliberately slow, ~100ms) argon2 verify, even
    # when the username already doesn't match — short-circuiting here
    # would make a wrong username return near-instantly while a correct
    # one takes ~100ms, letting an attacker find the valid username by
    # response-timing alone before ever guessing at the password.
    try:
        _hasher.verify(settings.LOCAL_ADMIN_PASSWORD_HASH, password)
        password_ok = True
    except VerifyMismatchError:
        password_ok = False
    except Exception:
        logger.exception("[LocalAuth] password verification failed unexpectedly")
        password_ok = False

    return username_ok and password_ok


def issue_token() -> str:
    """Mint a fresh session token for the local admin."""
    now = int(time.time())
    payload = {
        "sub": _JWT_SUBJECT,
        "user_id": LOCAL_USER_ID,
        "org_id": settings.LOCAL_ORG_ID,
        "org_role": "org:admin",
        "iat": now,
        "exp": now + _TOKEN_TTL_SECONDS,
    }
    return jwt.encode(payload, settings.APP_SECRET_KEY, algorithm=_JWT_ALGORITHM)


def verify_token(token: str) -> Optional[dict]:
    """Decode + verify a local session token.

    Returns the claims dict on success, None on any failure (expired,
    bad signature, malformed, wrong subject, no secret configured).
    """
    if not token or not settings.APP_SECRET_KEY:
        return None
    try:
        claims = jwt.decode(
            token,
            settings.APP_SECRET_KEY,
            algorithms=[_JWT_ALGORITHM],
            options={"require": ["sub", "user_id", "org_id", "org_role", "exp"]},
        )
    except jwt.InvalidTokenError as exc:
        logger.info("[LocalAuth] token verify failed: %s", type(exc).__name__)
        return None
    if claims.get("sub") != _JWT_SUBJECT:
        return None
    return claims


def refresh_token(token: str) -> Optional[str]:
    """Re-sign a still-valid token with a renewed expiry.

    Returns None if the token doesn't verify (expired or invalid) —
    callers should send the client back to /sign-in in that case.
    """
    claims = verify_token(token)
    if claims is None:
        return None
    return issue_token()
