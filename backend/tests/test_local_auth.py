"""Tests for local (self-hosted) admin login (app/core/local_auth.py).

Unlike app/core/clerk.py's client construction, these functions read
settings.* at call time rather than at import time, so monkeypatching
settings attributes works fine within the existing pytest process — no
subprocess isolation needed here (that's reserved for
test_local_auth_boot.py, which exercises app.main's import-time
behavior).
"""

from __future__ import annotations

import time

import jwt
import pytest
from argon2 import PasswordHasher

from app.core import local_auth
from app.core.config import settings

_TEST_HASH = PasswordHasher().hash("correct horse battery staple")


@pytest.fixture(autouse=True)
def _configure_local_admin(monkeypatch):
    monkeypatch.setattr(settings, "LOCAL_ADMIN_USERNAME", "admin")
    monkeypatch.setattr(settings, "LOCAL_ADMIN_PASSWORD_HASH", _TEST_HASH)
    monkeypatch.setattr(settings, "LOCAL_ADMIN_EMAIL", "admin@example.com")
    monkeypatch.setattr(settings, "APP_SECRET_KEY", "a" * 32)
    monkeypatch.setattr(settings, "LOCAL_ORG_ID", "self-host")


# ── verify_local_credentials ─────────────────────────────────────────


def test_verify_local_credentials_accepts_correct_password():
    assert local_auth.verify_local_credentials(
        "admin", "correct horse battery staple"
    )


def test_verify_local_credentials_rejects_wrong_password():
    assert not local_auth.verify_local_credentials("admin", "wrong password")


def test_verify_local_credentials_rejects_wrong_username():
    assert not local_auth.verify_local_credentials(
        "someone-else", "correct horse battery staple"
    )


def test_verify_local_credentials_rejects_non_ascii_username_without_crashing():
    """Regression: hmac.compare_digest on `str` raises TypeError for
    non-ASCII input. `username` here is unvalidated, attacker-controlled
    input from a public login request — verify_local_credentials must
    reject it cleanly (False), not raise."""
    assert not local_auth.verify_local_credentials(
        "admın", "correct horse battery staple"  # Turkish dotless ı, not ASCII
    )


def test_verify_local_credentials_fails_closed_when_unconfigured(monkeypatch):
    monkeypatch.setattr(settings, "LOCAL_ADMIN_USERNAME", "")
    monkeypatch.setattr(settings, "LOCAL_ADMIN_PASSWORD_HASH", "")
    assert not local_auth.verify_local_credentials("admin", "anything")


# ── issue_token / verify_token ────────────────────────────────────────


def test_issue_and_verify_token_roundtrip():
    token = local_auth.issue_token()
    claims = local_auth.verify_token(token)
    assert claims is not None
    assert claims["user_id"] == local_auth.LOCAL_USER_ID
    assert claims["org_id"] == "self-host"
    assert claims["org_role"] == "org:admin"


def test_verify_token_rejects_bad_signature():
    # Sign the same claim shape with a different secret, rather than
    # flipping a character in a real token: base64url's padding bits
    # mean flipping the LAST character of a signature sometimes decodes
    # to the same bytes, making that style of tampering test flaky.
    now = int(time.time())
    payload = {
        "sub": local_auth._JWT_SUBJECT,
        "user_id": local_auth.LOCAL_USER_ID,
        "org_id": settings.LOCAL_ORG_ID,
        "org_role": "org:admin",
        "iat": now,
        "exp": now + 3600,
    }
    forged = jwt.encode(payload, "wrong-secret", algorithm="HS256")
    assert local_auth.verify_token(forged) is None


def test_verify_token_rejects_expired_token(monkeypatch):
    now = int(time.time())
    payload = {
        "sub": local_auth._JWT_SUBJECT,
        "user_id": local_auth.LOCAL_USER_ID,
        "org_id": settings.LOCAL_ORG_ID,
        "org_role": "org:admin",
        "iat": now - 100,
        "exp": now - 50,
    }
    expired = jwt.encode(payload, settings.APP_SECRET_KEY, algorithm="HS256")
    assert local_auth.verify_token(expired) is None


def test_verify_token_rejects_wrong_subject():
    now = int(time.time())
    payload = {
        "sub": "not-the-right-subject",
        "user_id": local_auth.LOCAL_USER_ID,
        "org_id": settings.LOCAL_ORG_ID,
        "org_role": "org:admin",
        "iat": now,
        "exp": now + 3600,
    }
    token = jwt.encode(payload, settings.APP_SECRET_KEY, algorithm="HS256")
    assert local_auth.verify_token(token) is None


def test_verify_token_rejects_empty_or_no_secret(monkeypatch):
    assert local_auth.verify_token("") is None
    monkeypatch.setattr(settings, "APP_SECRET_KEY", "")
    assert local_auth.verify_token("anything.at.all") is None


# ── refresh_token ──────────────────────────────────────────────────────


def test_refresh_token_renews_a_valid_token():
    token = local_auth.issue_token()
    refreshed = local_auth.refresh_token(token)
    assert refreshed is not None
    claims = local_auth.verify_token(refreshed)
    assert claims is not None
    assert claims["user_id"] == local_auth.LOCAL_USER_ID


def test_refresh_token_rejects_expired_token(monkeypatch):
    now = int(time.time())
    payload = {
        "sub": local_auth._JWT_SUBJECT,
        "user_id": local_auth.LOCAL_USER_ID,
        "org_id": settings.LOCAL_ORG_ID,
        "org_role": "org:admin",
        "iat": now - 100,
        "exp": now - 50,
    }
    expired = jwt.encode(payload, settings.APP_SECRET_KEY, algorithm="HS256")
    assert local_auth.refresh_token(expired) is None
