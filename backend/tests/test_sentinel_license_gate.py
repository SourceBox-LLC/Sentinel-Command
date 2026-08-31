"""Tests for the self-host Sentinel AI license gate — the Phase 3 wiring
that requires a valid license (via the separate Sentinel License
Service, cached through app.core.license_client) before a self-hosted
install gets Sentinel AI access, while every other self-host feature
stays unaffected.

Covers the endpoint-level surface (`/api/sentinel/config` GET+PATCH,
`/api/sentinel/runs/manual`) and the underlying dispatch gate
(`_can_dispatch_for_kind`, `dispatch_manual_run`), in both the
unlicensed (blocked) and licensed (full access) states, plus
confirmation that hosted Clerk-mode orgs never consult license state
at all.
"""

from __future__ import annotations

from datetime import UTC, datetime

from app.core.config import settings
from app.core.sentinel_dispatch import (
    SENTINEL_PLANS,
    _can_dispatch_for_kind,
    dispatch_manual_run,
)
from app.models.models import SentinelConfig, Setting


def _mark_licensed(db, valid: bool = True):
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_key", "irrelevant", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_valid", "true" if valid else "false", commit=False)
    if valid:
        Setting.set(
            db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at",
            datetime.now(tz=UTC).isoformat(), commit=False,
        )
    db.commit()


def _self_host(monkeypatch):
    monkeypatch.setattr(settings, "AUTH_PROVIDER", "local")
    monkeypatch.setattr(settings, "SENTINEL_LICENSE_KEY", "slk_test")


# ── /api/sentinel/config GET ─────────────────────────────────────────


def test_self_host_unlicensed_shows_gated_config(monkeypatch, admin_client, db):
    _self_host(monkeypatch)
    # No check-in has ever happened -> never had a good check-in at all.

    response = admin_client.get("/api/sentinel/config")
    assert response.status_code == 200
    body = response.json()
    assert body["plan_gated"] is True
    assert body["monthly_cap"] == 0


def test_self_host_licensed_shows_full_config(monkeypatch, admin_client, db):
    _self_host(monkeypatch)
    _mark_licensed(db, valid=True)

    response = admin_client.get("/api/sentinel/config")
    assert response.status_code == 200
    body = response.json()
    assert body["plan_gated"] is False
    assert body["monthly_cap"] == 500  # self_host tier's MONTHLY_RUN_CAP_BY_PLAN entry


def test_self_host_revoked_license_shows_gated_config(monkeypatch, admin_client, db):
    _self_host(monkeypatch)
    _mark_licensed(db, valid=False)

    response = admin_client.get("/api/sentinel/config")
    body = response.json()
    assert body["plan_gated"] is True
    assert body["monthly_cap"] == 0


# ── /api/sentinel/config PATCH ───────────────────────────────────────


def test_self_host_unlicensed_patch_returns_402_license_required(monkeypatch, admin_client, db):
    _self_host(monkeypatch)

    response = admin_client.patch("/api/sentinel/config", json={"motion_enabled": False})
    assert response.status_code == 402
    assert response.json()["detail"] == {"error": "license_required"}


def test_self_host_licensed_patch_succeeds(monkeypatch, admin_client, db):
    _self_host(monkeypatch)
    _mark_licensed(db, valid=True)

    response = admin_client.patch("/api/sentinel/config", json={"motion_enabled": False})
    assert response.status_code == 200
    assert response.json()["config"]["motion_enabled"] is False


# ── /api/sentinel/runs/manual ─────────────────────────────────────────


def test_self_host_unlicensed_manual_run_returns_402_license_required(monkeypatch, admin_client, db):
    _self_host(monkeypatch)

    response = admin_client.post("/api/sentinel/runs/manual", json={"prompt": "check the front door"})
    assert response.status_code == 402
    assert response.json()["detail"] == {"error": "license_required"}


def test_self_host_licensed_manual_run_succeeds(monkeypatch, admin_client, db):
    _self_host(monkeypatch)
    _mark_licensed(db, valid=True)

    response = admin_client.post("/api/sentinel/runs/manual", json={"prompt": "check the front door"})
    assert response.status_code == 200
    assert response.json()["outcome"] == "pending"


# ── dispatch_manual_run (direct) ──────────────────────────────────────


def test_dispatch_manual_run_raises_license_required_when_unlicensed(monkeypatch, db):
    _self_host(monkeypatch)
    try:
        dispatch_manual_run(db, org_id=settings.LOCAL_ORG_ID, prompt="x")
        raise AssertionError("expected ValueError")
    except ValueError as exc:
        assert str(exc) == "license_required"


def test_dispatch_manual_run_succeeds_when_licensed(monkeypatch, db):
    _self_host(monkeypatch)
    _mark_licensed(db, valid=True)
    run = dispatch_manual_run(db, org_id=settings.LOCAL_ORG_ID, prompt="x")
    assert run.outcome == "pending"


# ── _can_dispatch_for_kind (automatic trigger gate) ───────────────────


def _make_config(db, org_id, **overrides):
    cfg = SentinelConfig(org_id=org_id, **overrides)
    db.add(cfg)
    db.commit()
    db.refresh(cfg)
    return cfg


def test_can_dispatch_blocked_for_unlicensed_self_host(monkeypatch, db):
    _self_host(monkeypatch)
    cfg = _make_config(db, settings.LOCAL_ORG_ID, enabled=True, motion_enabled=True)

    ok, reason = _can_dispatch_for_kind(cfg, "motion", None, db)
    assert ok is False
    assert reason == "license_required"


def test_can_dispatch_allowed_for_licensed_self_host(monkeypatch, db):
    _self_host(monkeypatch)
    _mark_licensed(db, valid=True)
    cfg = _make_config(db, settings.LOCAL_ORG_ID, enabled=True, motion_enabled=True)

    ok, reason = _can_dispatch_for_kind(cfg, "motion", None, db)
    assert ok is True
    assert reason == "ok"


# ── Hosted Clerk mode is completely unaffected ───────────────────────


def test_hosted_pro_org_never_consults_license_state(admin_client, db):
    """AUTH_PROVIDER defaults to "clerk" in the test suite (conftest.py).
    No license Setting rows exist at all; a Pro org must still get
    full access, proving the license gate is a no-op in hosted mode."""
    Setting.set(db, "org_test123", "org_plan", "pro")

    response = admin_client.patch("/api/sentinel/config", json={"motion_enabled": False})
    assert response.status_code == 200


def test_self_host_tier_is_in_sentinel_plans_regardless_of_license():
    """The license gate is layered ON TOP of plan membership, not a
    replacement for it — self_host must stay in SENTINEL_PLANS so the
    monthly-cap machinery (cap_for_plan, RATE_LIMITS) keeps working
    once a license is actually present."""
    assert "self_host" in SENTINEL_PLANS
