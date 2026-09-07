"""
Shared test fixtures for Sentinel backend tests.

Runs against a FastAPI test client with Clerk auth bypassed (mocked),
and — by default — an in-memory SQLite database.

The suite is dialect-parametrised on purpose. Hosted Command Center runs
Postgres while self-hosted runs SQLite from the same codebase, so both
have to stay green: testing only one means shipping the other unverified.
Set TEST_DATABASE_URL to point the whole suite at Postgres, e.g.

    TEST_DATABASE_URL=postgresql+psycopg://user:pass@localhost:5432/db \\
        uv run pytest

CI runs it both ways. The SQLite default keeps a bare `uv run pytest`
working with no database to set up.
"""

import os

import pytest
from fastapi.testclient import TestClient
from sqlalchemy.orm import sessionmaker

# Must set env vars BEFORE importing app modules so config.py picks them up.
# The default in-memory DB means main.py's startup code touches no real files.
os.environ["DATABASE_URL"] = os.environ.get("TEST_DATABASE_URL") or "sqlite:///:memory:"
os.environ.setdefault("CLERK_SECRET_KEY", "sk_test_fake")
os.environ.setdefault("CLERK_PUBLISHABLE_KEY", "pk_test_fake")

from app.core.auth import AuthUser
from app.core.database import Base, engine, get_db
from app.main import app

# Reuse the app's engine (which is now in-memory thanks to DATABASE_URL override)
TestSession = sessionmaker(bind=engine)


def _override_get_db():
    db = TestSession()
    try:
        yield db
    finally:
        try:
            db.close()
        except Exception:
            pass  # StaticPool + background threads can cause benign rollback errors


# ── Fixtures ─────────────────────────────────────────────────────

@pytest.fixture(autouse=True)
def setup_db():
    """Clear all table data between tests (tables created once at import)."""
    # Truncate all tables instead of drop/recreate to avoid
    # StaticPool rollback issues with in-memory SQLite.
    session = TestSession()
    for table in reversed(Base.metadata.sorted_tables):
        session.execute(table.delete())
    session.commit()
    session.close()

    app.dependency_overrides[get_db] = _override_get_db
    yield
    app.dependency_overrides.clear()


@pytest.fixture(autouse=True)
def reset_release_cache():
    """Clear the GitHub /releases/latest cache between tests.

    ``app.core.release_cache`` keeps the latest release JSON in module
    state.  Without this reset, a test that hits ``/downloads`` first
    (populating the cache from real GitHub) would change what
    ``check_node_version`` returns in subsequent tests — they
    monkeypatch ``settings.LATEST_NODE_VERSION`` expecting that to be
    the source of truth, which it only is when the cache is empty.
    Cheap to reset, and it restores deterministic test ordering.
    """
    from app.core import release_cache
    release_cache._reset_cache_for_tests()
    yield
    release_cache._reset_cache_for_tests()


@pytest.fixture(autouse=True)
def reset_rate_limiter():
    """Drop all in-memory rate-limit counters between tests.

    The slowapi limiter keeps counters in process memory (no Redis in
    tests), so a test that creates 5 nodes leaves a 5-tick stamp on the
    "20/hour POST /api/nodes" bucket that the next test inherits.  Tests
    are supposed to be order-independent — without this reset, adding a
    new test that creates resources can flake an unrelated suite that
    runs after it.  Best-effort: storage internals differ between
    backends, so any AttributeError just means "nothing to clear".
    """
    from app.core.limiter import limiter
    try:
        storage = getattr(limiter, "_storage", None) or getattr(limiter, "storage", None)
        if storage is not None and hasattr(storage, "storage"):
            storage.storage.clear()
        elif storage is not None and hasattr(storage, "reset"):
            storage.reset()
    except Exception:
        pass
    yield


@pytest.fixture
def db():
    """Direct DB session for test setup/assertions."""
    session = TestSession()
    try:
        yield session
    finally:
        session.close()


def _make_admin_user(org_id="org_test123"):
    return AuthUser(
        user_id="user_test123",
        org_id=org_id,
        org_role="org:admin",
        org_permissions=["org:admin:admin", "org:cameras:manage_cameras", "org:cameras:view_cameras"],
        email="admin@test.com",
        username="testadmin",
        plan="pro",
        features=["admin", "cameras"],
    )


def _make_viewer_user(org_id="org_test123"):
    return AuthUser(
        user_id="user_viewer456",
        org_id=org_id,
        org_role="org:member",
        org_permissions=["org:cameras:view_cameras"],
        email="viewer@test.com",
        username="testviewer",
        plan="pro",
        features=["cameras"],
    )


@pytest.fixture
def admin_client():
    """Test client authenticated as an admin user."""
    from app.core.auth import get_current_user, require_admin

    admin = _make_admin_user()
    app.dependency_overrides[require_admin] = lambda: admin
    app.dependency_overrides[get_current_user] = lambda: admin

    client = TestClient(app)
    yield client
    app.dependency_overrides.pop(require_admin, None)
    app.dependency_overrides.pop(get_current_user, None)


@pytest.fixture
def viewer_client():
    """Test client authenticated as a viewer user."""
    from app.core.auth import get_current_user, require_view

    viewer = _make_viewer_user()
    app.dependency_overrides[require_view] = lambda: viewer
    app.dependency_overrides[get_current_user] = lambda: viewer

    client = TestClient(app)
    yield client
    app.dependency_overrides.pop(require_view, None)
    app.dependency_overrides.pop(get_current_user, None)


@pytest.fixture
def unauthenticated_client():
    """Test client with no auth overrides."""
    return TestClient(app)


@pytest.fixture(autouse=True)
def _fresh_effective_plan_cache():
    """The 30s effective-plan TTL cache (app.core.plans) must never leak
    state across tests — a test that sets past_due/org_plan and then
    asserts cap behavior would otherwise read the previous test's cached
    slug.  Production invalidates explicitly on webhook plan writes."""
    from app.core.plans import invalidate_effective_plan_cache

    invalidate_effective_plan_cache()
    yield
    invalidate_effective_plan_cache()
