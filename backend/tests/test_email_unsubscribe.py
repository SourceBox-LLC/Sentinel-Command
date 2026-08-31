"""
Tests for unsubscribe-token sign/verify (app/core/email_unsubscribe.py).

Token shape and lifecycle (v2 — launch hardening):
  - signed JWT with HS256 + a secret DERIVED from CLERK_SECRET_KEY
    (HMAC with a fixed domain label; the raw Clerk key never signs a
    public surface, and the derived key is 64 hex chars)
  - payload: {org_id, kind, rcpt, iat, exp, sub: 'email-unsubscribe'}
  - rcpt binds the token to ONE recipient address — a click suppresses
    that address (EmailSuppression), not the whole org's toggle
  - exp at 400 days (CAN-SPAM floor is 30; a leaked link must not be
    a forever-credential)
  - no hardcoded fallback secret: unset CLERK_SECRET_KEY → mint raises,
    verify rejects (fail closed on a PUBLIC endpoint)

These tests verify both happy path and the failure modes that
matter — bad signature, wrong subject claim (defends future tokens
signed with the same key), missing claims, expiry, malformed input.
"""

from __future__ import annotations

import time

import jwt
import pytest

from app.core import email_unsubscribe

# ── Round-trip ──────────────────────────────────────────────────────

def test_make_and_verify_token_roundtrip():
    """Sign then verify the same token returns the original claims."""
    token = email_unsubscribe.make_token(
        "org_abc", "camera_offline", "Alice@Example.com",
    )

    decoded = email_unsubscribe.verify_token(token)

    # Recipient is normalized to lowercase at mint time.
    assert decoded == ("org_abc", "camera_offline", "alice@example.com")


def test_token_carries_correct_claims():
    """Subject claim is 'email-unsubscribe' — used by verify_token to
    refuse other JWTs signed with the same key.  rcpt + exp are the v2
    additions; pin them so a refactor that drops either surfaces here."""
    token = email_unsubscribe.make_token("org_x", "node_offline", "a@b.c")
    raw = jwt.decode(token, options={"verify_signature": False})
    assert raw["sub"] == "email-unsubscribe"
    assert raw["org_id"] == "org_x"
    assert raw["kind"] == "node_offline"
    assert raw["rcpt"] == "a@b.c"
    assert "iat" in raw  # issued-at present for audit
    # Expiry present and exactly the configured TTL out.
    assert raw["exp"] - raw["iat"] == email_unsubscribe._TOKEN_TTL_SECONDS


def test_secret_is_derived_not_raw_clerk_key():
    """The signing secret must never be the raw CLERK_SECRET_KEY —
    deriving with a domain label keeps the Clerk key off a public
    token surface and produces a full-length (64 hex char) HMAC key."""
    from app.core.config import settings

    secret = email_unsubscribe._get_secret()
    assert secret is not None
    assert secret != settings.CLERK_SECRET_KEY
    assert len(secret) == 64  # sha256 hexdigest


# ── Verification failure modes ──────────────────────────────────────

def test_verify_rejects_bad_signature():
    """Token signed with a different secret must NOT verify."""
    fake = jwt.encode(
        {
            "org_id": "x", "kind": "camera_offline", "rcpt": "a@b.c",
            "exp": int(time.time()) + 3600, "sub": "email-unsubscribe",
        },
        "wrong-secret",
        algorithm="HS256",
    )

    assert email_unsubscribe.verify_token(fake) is None


def test_verify_rejects_wrong_subject_claim():
    """A JWT with the right signature but a different 'sub' claim
    must be refused — defends against future tokens signed with the
    same key being abused as unsubscribe links."""
    secret = email_unsubscribe._get_secret()
    fake = jwt.encode(
        {
            "org_id": "x", "kind": "camera_offline", "rcpt": "a@b.c",
            "exp": int(time.time()) + 3600, "sub": "session",
        },
        secret,
        algorithm="HS256",
    )

    assert email_unsubscribe.verify_token(fake) is None


def test_verify_rejects_expired_token():
    """v2 tokens expire — a leaked link must not work forever."""
    secret = email_unsubscribe._get_secret()
    fake = jwt.encode(
        {
            "org_id": "x", "kind": "camera_offline", "rcpt": "a@b.c",
            "exp": int(time.time()) - 10, "sub": "email-unsubscribe",
        },
        secret,
        algorithm="HS256",
    )

    assert email_unsubscribe.verify_token(fake) is None


@pytest.mark.parametrize("missing", ["org_id", "kind", "rcpt", "exp"])
def test_verify_rejects_missing_required_claim(missing):
    """Each required claim's absence must fail verification."""
    secret = email_unsubscribe._get_secret()
    payload = {
        "org_id": "x", "kind": "camera_offline", "rcpt": "a@b.c",
        "exp": int(time.time()) + 3600, "sub": "email-unsubscribe",
    }
    del payload[missing]
    fake = jwt.encode(payload, secret, algorithm="HS256")

    assert email_unsubscribe.verify_token(fake) is None


@pytest.mark.parametrize("bad", ["", None, "not-a-jwt", "a.b.c", 12345])
def test_verify_rejects_malformed_input(bad):
    """Random garbage in the URL parameter doesn't crash — returns None."""
    assert email_unsubscribe.verify_token(bad) is None


def test_fail_closed_when_clerk_secret_unset(monkeypatch):
    """No hardcoded fallback: with CLERK_SECRET_KEY empty, minting
    raises and verification rejects.  The old fallback secret made the
    PUBLIC unsubscribe endpoint forgeable on a misconfigured deploy."""
    from app.core.config import settings as app_settings

    token = email_unsubscribe.make_token("org_x", "camera_offline", "a@b.c")
    monkeypatch.setattr(app_settings, "CLERK_SECRET_KEY", "")

    with pytest.raises(RuntimeError):
        email_unsubscribe.make_token("org_x", "camera_offline", "a@b.c")
    assert email_unsubscribe.verify_token(token) is None


def test_hosted_mode_ignores_app_secret_key(monkeypatch):
    """Regression: _get_secret() used to prefer APP_SECRET_KEY over
    CLERK_SECRET_KEY whenever it was merely set, with no AUTH_PROVIDER
    gate at all. A hosted (AUTH_PROVIDER=clerk) deployment must keep
    deriving from CLERK_SECRET_KEY unconditionally — an operator
    setting the generically-named APP_SECRET_KEY for any unrelated
    future purpose must never silently invalidate every outstanding
    unsubscribe link."""
    from app.core.config import settings as app_settings

    assert app_settings.AUTH_PROVIDER == "clerk"  # test suite default
    secret_before = email_unsubscribe._get_secret()

    monkeypatch.setattr(app_settings, "APP_SECRET_KEY", "some-unrelated-future-secret")

    assert email_unsubscribe._get_secret() == secret_before


def test_local_mode_uses_app_secret_key_not_clerk(monkeypatch):
    """Self-hosted installs have no Clerk key at all — APP_SECRET_KEY
    must be the source there, not merely preferred when set."""
    from app.core.config import settings as app_settings

    monkeypatch.setattr(app_settings, "AUTH_PROVIDER", "local")
    monkeypatch.setattr(app_settings, "APP_SECRET_KEY", "local-secret-key-value")

    secret = email_unsubscribe._get_secret()
    assert secret is not None
    # Changing CLERK_SECRET_KEY must have zero effect in local mode.
    monkeypatch.setattr(app_settings, "CLERK_SECRET_KEY", "something-else-entirely")
    assert email_unsubscribe._get_secret() == secret


# ── URL construction ────────────────────────────────────────────────

def test_build_unsubscribe_url_includes_token(monkeypatch):
    """Full URL: <frontend>/api/notifications/email/unsubscribe?t=<token>"""
    from app.core.config import settings as app_settings
    monkeypatch.setattr(app_settings, "FRONTEND_URL", "https://app.test")

    url = email_unsubscribe.build_unsubscribe_url(
        "org_x", "camera_offline", "ops@example.com",
    )

    assert url.startswith("https://app.test/api/notifications/email/unsubscribe?t=")
    # The token portion must round-trip.
    token = url.split("?t=")[1]
    assert email_unsubscribe.verify_token(token) == (
        "org_x", "camera_offline", "ops@example.com",
    )


def test_build_unsubscribe_url_strips_trailing_slash(monkeypatch):
    """FRONTEND_URL with a trailing slash shouldn't produce a //
    in the path — looks broken in email previews."""
    from app.core.config import settings as app_settings
    monkeypatch.setattr(app_settings, "FRONTEND_URL", "https://app.test/")

    url = email_unsubscribe.build_unsubscribe_url(
        "org_x", "camera_offline", "ops@example.com",
    )

    assert "//api" not in url
    assert "/api/notifications" in url
