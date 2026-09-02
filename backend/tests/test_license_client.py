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

_UNSET = object()


class _FakeResponse:
    def __init__(self, status_code: int = 200, json_data=_UNSET):
        self.status_code = status_code
        # A sentinel default (not `json_data or {}`) so an explicitly
        # falsy body — None, [], "", 0 — passes through as itself
        # instead of silently becoming {} when a test wants to exercise
        # exactly those malformed-response shapes.
        self._json_data = {} if json_data is _UNSET else json_data

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


# ── Malformed check-in responses (regression: must not crash) ────────


@pytest.mark.parametrize("body", [None, [], "just a string", 42, True])
async def test_checkin_treats_non_dict_200_response_as_unreachable(monkeypatch, db, body):
    """A non-object 200 JSON body must never raise — data.get("valid")
    on a non-dict would previously escape uncaught, skipping every
    Setting write for that tick and freezing whatever state was cached
    before (possibly fail-open) indefinitely."""
    _mock_http(monkeypatch, response=_FakeResponse(200, body))

    await license_client.check_in_with_license_service(db)  # must not raise

    assert Setting.get(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable") == "false"
    assert license_client.is_sentinel_licensed(db) is False


# ── Naive timestamps (regression: must not raise TypeError) ──────────


def test_is_sentinel_licensed_handles_naive_last_ok_at(db):
    """datetime.fromisoformat happily parses a naive (no-offset) string
    without raising — only the later `datetime.now(tz=UTC) - naive_dt`
    subtraction raises TypeError, which used to be uncaught."""
    naive = datetime.now(tz=UTC).replace(tzinfo=None).isoformat()
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_valid", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "false", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at", naive)

    # A naive-but-recent timestamp is normalized to UTC and still
    # grants grace — must not raise.
    assert license_client.is_sentinel_licensed(db) is True


def test_warn_if_in_extended_grace_handles_naive_last_ok_at(db):
    naive = (datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(hours=2))
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "false", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at", naive.isoformat())

    license_client.warn_if_in_extended_grace(db)  # must not raise


def test_parse_iso_or_none_rejects_garbage_without_raising():
    assert license_client._parse_iso_or_none("not a timestamp") is None
    assert license_client._parse_iso_or_none("") is None


# ── Shared license-gate predicate ─────────────────────────────────────


def test_sentinel_blocked_by_license_true_only_for_self_host_and_unlicensed(monkeypatch, db):
    # self_host + unlicensed -> blocked
    assert license_client.sentinel_blocked_by_license("self_host", db) is True

    # self_host + licensed -> not blocked
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_valid", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at", datetime.now(tz=UTC).isoformat())
    assert license_client.sentinel_blocked_by_license("self_host", db) is False


def test_sentinel_blocked_by_license_never_blocks_other_plans(db):
    # The predicate only ever applies to "self_host" — other plans
    # (pro/pro_plus/free_org) are governed purely by plan membership
    # elsewhere and must never be blocked by this predicate.
    for plan in ("pro", "pro_plus", "free_org"):
        assert license_client.sentinel_blocked_by_license(plan, db) is False


# ── probe_sentinel_license (health_probes.py) ─────────────────────────


def test_probe_reports_warn_when_reachable_but_explicitly_unlicensed(db):
    """Regression: status used to be derived from reachability alone,
    so a reachable-but-revoked license reported "ok" — the actual
    licensed:false verdict was buried in the data dict, never
    reflected in the status field that drives the health rollup."""
    from app.core.health_probes import probe_sentinel_license

    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_valid", "false", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_at", datetime.now(tz=UTC).isoformat())

    result = probe_sentinel_license(uptime_seconds=999)

    assert result.data["licensed"] is False
    assert result.status == "warn"


def test_probe_reports_ok_when_reachable_and_licensed(db):
    from app.core.health_probes import probe_sentinel_license

    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_valid", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "true", commit=False)
    now_iso = datetime.now(tz=UTC).isoformat()
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_at", now_iso, commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at", now_iso)

    result = probe_sentinel_license(uptime_seconds=999)

    assert result.status == "ok"


def test_probe_applies_startup_grace_before_first_checkin(db):
    """Regression: with no check-in yet, a fresh boot used to report
    "warn" immediately — probe_email_worker's sibling pattern grants a
    startup grace window instead."""
    from app.core.health_probes import probe_sentinel_license

    result = probe_sentinel_license(uptime_seconds=1.0)

    assert result.status == "ok"
    assert result.data.get("note") == "startup grace"


def test_probe_reports_warn_after_startup_grace_with_no_checkin(db):
    from app.core.health_probes import probe_sentinel_license

    result = probe_sentinel_license(uptime_seconds=999)

    assert result.status == "warn"
