"""Tests for app/core/license_client.py — the self-host Sentinel AI
license check-in and its fail-open/fail-closed cache read.

Covers all four states from the module docstring:
  1. Never configured -> hard fail-closed, no grace.
  2. Reachable + valid -> full access.
  3. Unreachable -> grace window from the last known-good check-in,
     then fail closed once the window is exhausted.
  4. Reachable + explicitly invalid -> fail closed immediately, no grace.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta

import httpx
import pytest

from app.core import license_client
from app.core.config import settings
from app.models.models import Setting


class _FakeResponse:
    def __init__(self, status_code: int = 200, json_data: dict | None = None):
        self.status_code = status_code
        self._json_data = json_data or {}

    def raise_for_status(self):
        if self.status_code >= 400:
            raise httpx.HTTPStatusError("error", request=None, response=self)

    def json(self):
        return self._json_data


class _FakeAsyncClient:
    def __init__(self, response=None, exc=None, **kwargs):
        self._response = response
        self._exc = exc

    async def __aenter__(self):
        return self

    async def __aexit__(self, *args):
        return False

    async def post(self, url, headers=None, json=None):
        if self._exc is not None:
            raise self._exc
        return self._response


def _mock_http(monkeypatch, *, response=None, exc=None):
    def factory(*args, **kwargs):
        return _FakeAsyncClient(response=response, exc=exc)

    monkeypatch.setattr(license_client.httpx, "AsyncClient", factory)


@pytest.fixture(autouse=True)
def _local_self_host(monkeypatch):
    monkeypatch.setattr(settings, "AUTH_PROVIDER", "local")
    monkeypatch.setattr(settings, "SENTINEL_LICENSE_KEY", "slk_test_key")
    monkeypatch.setattr(settings, "SENTINEL_LICENSE_SERVICE_URL", "https://license.test")
    monkeypatch.setattr(settings, "LOCAL_ORG_ID", "self-host-test-org")


# ── State 1: never configured ───────────────────────────────────────


def test_hosted_clerk_orgs_are_always_licensed(monkeypatch, db):
    """This whole module is a self-host-only concern — hosted orgs
    never consult it at all."""
    monkeypatch.setattr(settings, "AUTH_PROVIDER", "clerk")
    assert license_client.is_sentinel_licensed(db) is True


def test_no_key_configured_fails_closed_with_no_grace(monkeypatch, db):
    monkeypatch.setattr(settings, "SENTINEL_LICENSE_KEY", "")
    assert license_client.is_sentinel_licensed(db) is False


async def test_checkin_is_a_noop_when_no_key_configured(monkeypatch, db):
    monkeypatch.setattr(settings, "SENTINEL_LICENSE_KEY", "")
    # No fake transport installed — if this tried to make a real HTTP
    # call, it would raise (unmocked httpx.AsyncClient against a fake
    # host). Reaching the end without error proves it short-circuited.
    await license_client.check_in_with_license_service(db)


def test_never_checked_in_at_all_fails_closed(db):
    """A key is configured but no check-in has ever completed — no
    last-known-good state to lean on for grace."""
    assert license_client.is_sentinel_licensed(db) is False


# ── State 2: reachable + valid ──────────────────────────────────────


async def test_reachable_and_valid_grants_access(monkeypatch, db):
    _mock_http(monkeypatch, response=_FakeResponse(200, {
        "valid": True, "reason": None, "tier": "self_host_standard",
        "status": "active", "monthly_run_cap": 500, "renews_at": None,
        "server_time": "2026-01-01T00:00:00Z",
    }))

    await license_client.check_in_with_license_service(db)

    assert license_client.is_sentinel_licensed(db) is True
    assert Setting.get(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable") == "true"
    assert Setting.get(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at") is not None


# ── State 4: reachable + explicitly invalid ─────────────────────────


@pytest.mark.parametrize("reason", ["revoked", "expired", "suspended", "not_found"])
async def test_reachable_and_invalid_fails_closed_immediately(monkeypatch, db, reason):
    _mock_http(monkeypatch, response=_FakeResponse(200, {
        "valid": False, "reason": reason, "tier": None, "status": None,
        "monthly_run_cap": None, "renews_at": None,
        "server_time": "2026-01-01T00:00:00Z",
    }))

    await license_client.check_in_with_license_service(db)

    assert license_client.is_sentinel_licensed(db) is False


async def test_invalid_verdict_does_not_advance_last_ok_at(monkeypatch, db):
    """A prior good check-in's last_ok_at must not be touched by a
    subsequent invalid verdict — it's the anchor a LATER unreachable
    period's grace window depends on staying accurate."""
    _mock_http(monkeypatch, response=_FakeResponse(200, {"valid": True, "renews_at": None, "server_time": "x"}))
    await license_client.check_in_with_license_service(db)
    first_ok_at = Setting.get(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at")

    _mock_http(monkeypatch, response=_FakeResponse(200, {"valid": False, "reason": "revoked", "server_time": "x"}))
    await license_client.check_in_with_license_service(db)

    assert Setting.get(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at") == first_ok_at
    assert license_client.is_sentinel_licensed(db) is False


# ── State 3: unreachable ─────────────────────────────────────────────


async def test_unreachable_within_grace_window_stays_licensed(monkeypatch, db):
    recent = datetime.now(tz=UTC).isoformat()
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_valid", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at", recent)

    _mock_http(monkeypatch, exc=httpx.ConnectError("connection refused"))
    await license_client.check_in_with_license_service(db)

    assert license_client.is_sentinel_licensed(db) is True
    assert Setting.get(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable") == "false"


async def test_unreachable_past_grace_window_fails_closed(monkeypatch, db):
    stale = (
        datetime.now(tz=UTC) - timedelta(hours=license_client.SENTINEL_LICENSE_GRACE_HOURS + 1)
    ).isoformat()
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_valid", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at", stale)

    _mock_http(monkeypatch, exc=httpx.ConnectError("connection refused"))
    await license_client.check_in_with_license_service(db)

    assert license_client.is_sentinel_licensed(db) is False


async def test_a_reachable_check_in_recovers_from_grace(monkeypatch, db):
    """After an unreachable period, a subsequent reachable+valid
    check-in should immediately restore full trust (not stay stuck
    evaluating grace math)."""
    recent = datetime.now(tz=UTC).isoformat()
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_valid", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "false", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at", recent)

    _mock_http(monkeypatch, response=_FakeResponse(200, {"valid": True, "renews_at": None, "server_time": "x"}))
    await license_client.check_in_with_license_service(db)

    assert Setting.get(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable") == "true"
    assert license_client.is_sentinel_licensed(db) is True


# ── warn_if_in_extended_grace ────────────────────────────────────────


def test_warn_if_in_extended_grace_is_silent_when_reachable(db):
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "true")
    # Must not raise; nothing to assert on output since it's log-only.
    license_client.warn_if_in_extended_grace(db)


def test_warn_if_in_extended_grace_is_silent_when_too_fresh(db):
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "false", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at", datetime.now(tz=UTC).isoformat())
    license_client.warn_if_in_extended_grace(db)
    assert Setting.get(db, settings.LOCAL_ORG_ID, "sentinel_license_last_grace_warning_at") is None


def test_warn_if_in_extended_grace_warns_and_records_once_stale(db, caplog):
    stale = (datetime.now(tz=UTC) - timedelta(hours=2)).isoformat()
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "false", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at", stale)

    with caplog.at_level("WARNING"):
        license_client.warn_if_in_extended_grace(db)

    assert any("unreachable" in r.message for r in caplog.records)
    assert Setting.get(db, settings.LOCAL_ORG_ID, "sentinel_license_last_grace_warning_at") is not None


# ── install_id ───────────────────────────────────────────────────────


def test_install_id_is_generated_once_and_stable(db):
    first = license_client._get_or_create_install_id(db)
    second = license_client._get_or_create_install_id(db)
    assert first == second
    assert len(first) == 32  # secrets.token_hex(16)
