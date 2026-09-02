"""Process-isolation boot check for local (self-hosted) auth mode.

``tests/conftest.py`` sets CLERK_SECRET_KEY/CLERK_PUBLISHABLE_KEY via
``os.environ.setdefault`` before importing ``app.main``, and both
``Config`` (app/core/config.py) and the Clerk client (app/core/clerk.py)
read/construct from env vars at *import time*. Because Python caches
``sys.modules``, once ``app.main`` has been imported in this pytest
process with Clerk configured, no amount of monkeypatching
``settings.AUTH_PROVIDER`` afterward re-triggers app.core.clerk's
conditional construction. So "does the app boot with zero Clerk env vars
in local mode" has to run in a genuinely separate interpreter.
"""

import os
import subprocess
import sys

from argon2 import PasswordHasher

_BACKEND_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _clean_env(**overrides) -> dict:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("CLERK_")
    }
    # Explicitly blank rather than merely omit: config.py's load_dotenv()
    # searches upward from the subprocess's cwd (backend/) and finds the
    # repo-root .env — which, in this dev checkout, carries real Clerk
    # dev keys. load_dotenv() doesn't override a key that's already
    # present (even empty), so setting these to "" is what actually
    # keeps a subprocess Clerk-free; omitting them leaves the door open
    # for the file to backfill real values.
    env["CLERK_SECRET_KEY"] = ""
    env["CLERK_PUBLISHABLE_KEY"] = ""
    env["AUTH_PROVIDER"] = "local"
    env["DATABASE_URL"] = "sqlite:///:memory:"
    env["APP_SECRET_KEY"] = "test-secret-for-boot-check"
    env.update(overrides)
    return env


def test_app_boots_in_local_mode_with_no_clerk_env_vars():
    result = subprocess.run(
        [sys.executable, "-c", "import app.main"],
        cwd=_BACKEND_DIR,
        env=_clean_env(),
        capture_output=True,
        text=True,
        timeout=30,
    )

    assert result.returncode == 0, (
        f"app.main failed to import with AUTH_PROVIDER=local and no Clerk "
        f"env vars.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    )


def test_non_exact_auth_provider_value_fails_fast_instead_of_half_working():
    """Regression for the polarity-mismatch bug a code review caught:
    app/core/clerk.py + app/main.py used to test `== "clerk"` (treating
    ANY other value, including a typo like "Local", as local mode) while
    app/core/auth.py tested `== "local"` (treating that same typo as
    Clerk mode). The result was a half-working app: local login
    succeeded (main.py had mounted the local router), but every
    subsequent authenticated request 401'd because auth.py routed to
    the Clerk path against a `clerk = None` client.

    Now that every call site routes through the same is_local_auth()/
    is_clerk_auth() complement, a non-exact value consistently means
    Clerk mode everywhere — so with no Clerk keys configured, the app
    fails LOUDLY at import time (a clear, diagnosable error) instead of
    booting into that confusing half-working state.
    """
    env = _clean_env(AUTH_PROVIDER="Local")  # wrong case — not the exact literal
    del env["APP_SECRET_KEY"]  # irrelevant to this mode now; keep the env minimal

    result = subprocess.run(
        [sys.executable, "-c", "import app.main"],
        cwd=_BACKEND_DIR,
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )

    assert result.returncode != 0, (
        "expected import to fail (no Clerk keys configured, and 'Local' isn't "
        "the exact 'local' literal so this must be treated as Clerk mode)"
    )
    assert "CLERK_SECRET_KEY is required" in result.stderr, result.stderr


_LOGIN_FLOW_SCRIPT = """
from fastapi.testclient import TestClient
from app.main import app

client = TestClient(app)

# Wrong credentials are rejected.
r = client.post("/api/auth/local/login", json={"username": "admin", "password": "nope"})
assert r.status_code == 401, r.text

# Correct credentials issue a token.
r = client.post("/api/auth/local/login", json={"username": "admin", "password": "s3cret-pw"})
assert r.status_code == 200, r.text
token = r.json()["token"]
assert token

# The token authenticates a real protected route, and plan resolves to
# the fully-unlocked self_host tier (not a silent free_org fallback).
r = client.get("/api/nodes/plan", headers={"Authorization": f"Bearer {token}"})
assert r.status_code == 200, r.text
body = r.json()
assert body["plan"] == "self_host", body

# No Authorization header is rejected.
r = client.get("/api/nodes/plan")
assert r.status_code == 401, r.text

# The Clerk-only webhook route must not be mounted in local mode.
r = client.post("/api/webhooks/clerk", json={})
assert r.status_code == 404, r.text

print("LOGIN_FLOW_OK")
"""


def test_local_login_flow_end_to_end():
    """Full round trip in local mode: bad login rejected, good login
    issues a token, the token authenticates a real protected route via
    the local get_current_user dispatch, and plan resolves to
    "self_host" rather than silently falling back to free_org.

    Runs in a subprocess for the same reason as the boot test above —
    app.main must be imported fresh with AUTH_PROVIDER=local and no
    Clerk env vars.
    """
    password_hash = PasswordHasher().hash("s3cret-pw")
    env = _clean_env(
        LOCAL_ADMIN_USERNAME="admin",
        LOCAL_ADMIN_PASSWORD_HASH=password_hash,
        LOCAL_ADMIN_EMAIL="admin@example.com",
    )

    result = subprocess.run(
        [sys.executable, "-c", _LOGIN_FLOW_SCRIPT],
        cwd=_BACKEND_DIR,
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )

    assert result.returncode == 0 and "LOGIN_FLOW_OK" in result.stdout, (
        f"local login flow failed.\nstdout:\n{result.stdout}\n"
        f"stderr:\n{result.stderr}"
    )


_UNCONFIGURED_LOGIN_SCRIPT = """
from fastapi.testclient import TestClient
from app.main import app

client = TestClient(app)
r = client.post("/api/auth/local/login", json={"username": "admin", "password": "anything"})
assert r.status_code == 503, r.text

r = client.post("/api/auth/local/refresh", json={"token": "a.b.c"})
assert r.status_code == 503, r.text

print("UNCONFIGURED_OK")
"""


def test_login_returns_503_when_app_secret_key_is_missing():
    """Regression: issue_token() calls jwt.encode(payload, APP_SECRET_KEY,
    ...) with no prior validation that the secret is non-empty — a code
    review caught that this surfaced as a raw, unhandled 500 (jwt's
    InvalidKeyError) rather than a clear config error. The login/refresh
    routes now check settings.is_local_auth_configured() up front and
    return a proper 503 instead.
    """
    env = _clean_env(
        LOCAL_ADMIN_USERNAME="admin",
        LOCAL_ADMIN_PASSWORD_HASH="irrelevant-not-reached",
    )
    del env["APP_SECRET_KEY"]  # the missing piece this test is about

    result = subprocess.run(
        [sys.executable, "-c", _UNCONFIGURED_LOGIN_SCRIPT],
        cwd=_BACKEND_DIR,
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )

    assert result.returncode == 0 and "UNCONFIGURED_OK" in result.stdout, (
        f"expected a clean 503, not a crash.\nstdout:\n{result.stdout}\n"
        f"stderr:\n{result.stderr}"
    )
