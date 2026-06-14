"""
Backend tests for the Sentinel surface.

Covers the eight `/api/sentinel/*` routes (5 admin, 3 agent-side) plus
the dispatch helpers in `app.core.sentinel_dispatch`. Today's audit
+ hardening pass added meaningful state-machine surface area to these
routes — plan-gating chain, cap-race protection, idempotent /complete
with ownership cross-check, motion cooldown, schedule minute parsing,
stranded-run reaper — none of which had test coverage. This file is
the regression net for that surface.

Auth model in tests:

  - Admin endpoints: use the existing `admin_client` fixture from
    conftest.py. The default org_id is `org_test123`, default plan
    is `pro`. Tests that need Pro Plus or Free plans set the
    Setting(org_plan) row directly via the `db` fixture before the
    request.

  - Agent-side endpoints: use a fresh TestClient with the
    `X-Sentinel-Agent-Key` header set, after configuring
    `settings.SENTINEL_AGENT_KEY` to a known value via monkeypatch.
"""

from datetime import UTC, datetime, timedelta

import pytest
from fastapi.testclient import TestClient

from app.core.config import settings
from app.core.sentinel_dispatch import (
    MONTHLY_RUN_CAP_BY_PLAN,
    SENTINEL_PLANS,
    STRANDED_RUN_AGE_MINUTES,
    _can_dispatch_for_kind,
    _commit_run_with_cap_check,
    _motion_cooldown_allows,
    _parse_hhmm_to_minutes,
    cap_for_plan,
    cap_remaining,
    dispatch_manual_run,
    reap_stranded_runs,
    runs_used_this_month,
)
from app.main import app
from app.models.models import Incident, SentinelConfig, SentinelRun, Setting

# ── Helpers ──────────────────────────────────────────────────────


def _set_org_plan(db, org_id, plan):
    """Pin an org's plan slug via the Setting cache that
    resolve_org_plan reads first.  Required for any test that needs
    a plan other than the default `pro`."""
    Setting.set(db, org_id, "org_plan", plan)


def _make_run(db, *, org_id="org_test123", trigger="motion", outcome="pending",
              camera_id=None, started_at=None, run_id=None, triggered_at=None):
    """Insert a SentinelRun row directly via the DB session.  Used
    by tests that need pre-existing run state without going through
    the dispatch / start / complete API surface."""
    import uuid
    run = SentinelRun(
        id=run_id or uuid.uuid4().hex,
        org_id=org_id,
        triggered_at=triggered_at or datetime.now(tz=UTC).replace(tzinfo=None),
        trigger_type=trigger,
        camera_id=camera_id,
        tool_call_count=0,
        outcome=outcome,
        started_at=started_at,
    )
    db.add(run)
    db.commit()
    db.refresh(run)
    return run


# ── /api/sentinel/config ─────────────────────────────────────────


class TestSentinelConfigEndpoint:
    """GET creates a default config row on first call.  Plan-gated
    behaviour: free orgs see plan_gated=true with monthly_cap=0; Pro
    sees 100; Pro Plus sees 500.  PATCH is gated to Sentinel-eligible
    plans; non-eligible orgs get 402."""

    def test_get_creates_default_row_first_call(self, admin_client, db):
        # No SentinelConfig row exists yet
        assert db.query(SentinelConfig).count() == 0

        response = admin_client.get("/api/sentinel/config")
        assert response.status_code == 200

        # Row was lazily created
        assert db.query(SentinelConfig).filter_by(org_id="org_test123").count() == 1

        body = response.json()
        assert "config" in body
        assert body["config"]["enabled"] is True  # default
        assert body["config"]["motion_enabled"] is True
        assert body["config"]["incident_opened_enabled"] is True
        assert body["config"]["motion_cooldown_min"] == 5
        assert body["config"]["schedule_mode"] == "always"

    def test_get_returns_pro_cap_for_pro_org(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro")
        response = admin_client.get("/api/sentinel/config")
        assert response.status_code == 200
        body = response.json()
        assert body["plan_gated"] is False
        assert body["monthly_cap"] == 100

    def test_get_returns_pro_plus_cap_for_pro_plus_org(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro_plus")
        response = admin_client.get("/api/sentinel/config")
        assert response.status_code == 200
        body = response.json()
        assert body["plan_gated"] is False
        assert body["monthly_cap"] == 500

    def test_get_plan_gates_free_org(self, admin_client, db):
        _set_org_plan(db, "org_test123", "free_org")
        response = admin_client.get("/api/sentinel/config")
        assert response.status_code == 200
        body = response.json()
        # Free orgs get a read-only payload — UI renders the upgrade banner
        assert body["plan_gated"] is True
        assert body["monthly_cap"] == 0
        assert body["plan_required"] == "pro"

    def test_patch_rejects_free_org_with_402(self, admin_client, db):
        _set_org_plan(db, "org_test123", "free_org")
        response = admin_client.patch(
            "/api/sentinel/config",
            json={"motion_enabled": False},
        )
        assert response.status_code == 402
        assert response.json()["detail"]["error"] == "plan_required"
        assert response.json()["detail"]["plan"] == "pro"

    def test_patch_accepts_pro_org(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro")
        response = admin_client.patch(
            "/api/sentinel/config",
            json={"motion_enabled": False, "motion_cooldown_min": 10},
        )
        assert response.status_code == 200
        body = response.json()
        assert body["config"]["motion_enabled"] is False
        assert body["config"]["motion_cooldown_min"] == 10

    def test_patch_partial_update_leaves_other_fields_alone(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro")
        admin_client.patch("/api/sentinel/config", json={"motion_cooldown_min": 30})
        response = admin_client.patch(
            "/api/sentinel/config", json={"schedule_mode": "scheduled"}
        )
        assert response.status_code == 200
        body = response.json()
        # The earlier cooldown should still be there
        assert body["config"]["motion_cooldown_min"] == 30
        assert body["config"]["schedule_mode"] == "scheduled"

    def test_patch_rejects_invalid_schedule_mode(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro")
        response = admin_client.patch(
            "/api/sentinel/config", json={"schedule_mode": "bogus"}
        )
        assert response.status_code in (400, 422)


# ── /api/sentinel/runs ───────────────────────────────────────────


class TestSentinelRunsListEndpoint:
    """List + stats endpoint returns runs with pagination + stats
    that include the plan-aware monthly_cap."""

    def test_returns_empty_list_for_new_org(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro")
        response = admin_client.get("/api/sentinel/runs")
        assert response.status_code == 200
        body = response.json()
        assert body["runs"] == []
        assert body["total"] == 0
        assert body["stats"]["monthly_cap"] == 100  # pro
        assert body["stats"]["remaining_this_month"] == 100

    def test_returns_pro_plus_cap_in_stats(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro_plus")
        response = admin_client.get("/api/sentinel/runs")
        assert response.status_code == 200
        body = response.json()
        assert body["stats"]["monthly_cap"] == 500

    def test_lists_runs_in_reverse_chronological_order(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro")
        # Insert 3 runs with explicit timestamps
        base = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(hours=2)
        for i in range(3):
            _make_run(
                db,
                run_id=f"run_{i}",
                triggered_at=base + timedelta(minutes=i * 30),
                outcome="incident",
            )
        response = admin_client.get("/api/sentinel/runs")
        body = response.json()
        assert body["total"] == 3
        # Newest first
        assert body["runs"][0]["id"] == "run_2"
        assert body["runs"][2]["id"] == "run_0"

    def test_org_isolation_other_org_runs_invisible(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro")
        _make_run(db, org_id="org_test123", run_id="mine")
        _make_run(db, org_id="org_other", run_id="theirs")
        response = admin_client.get("/api/sentinel/runs")
        body = response.json()
        assert body["total"] == 1
        assert body["runs"][0]["id"] == "mine"

    def test_stats_count_only_current_month(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro")
        # One run from last month
        last_month = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(days=40)
        _make_run(db, run_id="last_month", triggered_at=last_month)
        # Two runs this month
        _make_run(db, run_id="this_a")
        _make_run(db, run_id="this_b")
        response = admin_client.get("/api/sentinel/runs")
        body = response.json()
        assert body["total"] == 3  # All in the list
        assert body["stats"]["runs_this_month"] == 2
        assert body["stats"]["remaining_this_month"] == 100 - 2


# ── /api/sentinel/runs/{id} ──────────────────────────────────────


class TestSentinelRunDetailEndpoint:
    def test_returns_full_run(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro")
        run = _make_run(db, run_id="abc123", outcome="no_action")
        response = admin_client.get(f"/api/sentinel/runs/{run.id}")
        assert response.status_code == 200
        body = response.json()
        assert body["id"] == "abc123"
        assert body["outcome"] == "no_action"

    def test_404_on_unknown_run(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro")
        response = admin_client.get("/api/sentinel/runs/does_not_exist")
        assert response.status_code == 404

    def test_404_on_other_orgs_run(self, admin_client, db):
        """Org isolation — fetching another org's run by id returns
        404, not 403, so we don't leak existence."""
        _set_org_plan(db, "org_test123", "pro")
        other_run = _make_run(db, org_id="org_other", run_id="other_run")
        response = admin_client.get(f"/api/sentinel/runs/{other_run.id}")
        assert response.status_code == 404


# ── /api/sentinel/runs/manual ────────────────────────────────────


class TestSentinelManualRunEndpoint:
    def test_pro_org_can_dispatch_manual_run(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro")
        response = admin_client.post(
            "/api/sentinel/runs/manual",
            json={"prompt": "investigate everything", "camera_id": None},
        )
        # Manual run path also fires the wakeup webhook in a daemon
        # thread; that's a no-op when SENTINEL_AGENT_WEBHOOK_URL is
        # unset (the default in tests).
        assert response.status_code == 200
        body = response.json()
        assert body["trigger_type"] == "manual"
        assert body["outcome"] == "pending"

    def test_free_org_rejected_with_402(self, admin_client, db):
        _set_org_plan(db, "org_test123", "free_org")
        response = admin_client.post(
            "/api/sentinel/runs/manual", json={"prompt": ""}
        )
        assert response.status_code == 402
        assert response.json()["detail"]["error"] == "plan_required"

    def test_429_when_at_cap(self, admin_client, db):
        _set_org_plan(db, "org_test123", "pro")
        # Burn the entire Pro cap (100) directly via the DB
        for i in range(100):
            _make_run(db, run_id=f"used_{i}", outcome="no_action")
        response = admin_client.post(
            "/api/sentinel/runs/manual", json={"prompt": ""}
        )
        assert response.status_code == 429
        body = response.json()
        assert body["detail"]["error"] == "monthly_cap_reached"
        assert body["detail"]["cap"] == 100  # Pro cap reflected in error
        assert body["detail"]["used"] >= 100

    def test_pro_plus_org_gets_500_cap(self, admin_client, db):
        """A Pro Plus org's 429 reflects the 500 cap, not Pro's 100."""
        _set_org_plan(db, "org_test123", "pro_plus")
        for i in range(500):
            _make_run(db, run_id=f"used_{i}", outcome="no_action")
        response = admin_client.post(
            "/api/sentinel/runs/manual", json={"prompt": ""}
        )
        assert response.status_code == 429
        assert response.json()["detail"]["cap"] == 500


# ── Agent-side endpoints (X-Sentinel-Agent-Key) ──────────────────


class TestSentinelAgentEndpoints:
    """The three service-to-service routes the agent uses.  Auth is
    a shared bearer in the X-Sentinel-Agent-Key header, compared
    constant-time against settings.SENTINEL_AGENT_KEY."""

    AGENT_KEY = "test_agent_key_super_secret_value"

    @pytest.fixture
    def agent_client(self, monkeypatch):
        monkeypatch.setattr(settings, "SENTINEL_AGENT_KEY", self.AGENT_KEY)
        client = TestClient(app)
        return client

    def _agent_headers(self):
        return {"X-Sentinel-Agent-Key": self.AGENT_KEY}

    def test_pending_returns_pending_runs_only(self, agent_client, db):
        _make_run(db, run_id="pending_a", outcome="pending")
        _make_run(db, run_id="running_b", outcome="running")
        _make_run(db, run_id="done_c", outcome="incident")
        response = agent_client.get(
            "/api/sentinel/runs/pending", headers=self._agent_headers()
        )
        assert response.status_code == 200
        body = response.json()
        run_ids = [r["id"] for r in body["runs"]]
        assert run_ids == ["pending_a"]

    def test_pending_returns_runs_across_all_orgs(self, agent_client, db):
        """The agent is multi-tenant; pending runs from every org
        should drain through the same /pending call."""
        _make_run(db, org_id="org_a", run_id="from_a", outcome="pending")
        _make_run(db, org_id="org_b", run_id="from_b", outcome="pending")
        response = agent_client.get(
            "/api/sentinel/runs/pending", headers=self._agent_headers()
        )
        assert response.status_code == 200
        run_ids = {r["id"] for r in response.json()["runs"]}
        assert run_ids == {"from_a", "from_b"}

    def test_pending_rejects_wrong_agent_key(self, agent_client):
        response = agent_client.get(
            "/api/sentinel/runs/pending",
            headers={"X-Sentinel-Agent-Key": "wrong"},
        )
        assert response.status_code == 401

    def test_pending_rejects_missing_agent_key(self, agent_client):
        response = agent_client.get("/api/sentinel/runs/pending")
        assert response.status_code == 401

    def test_pending_rejects_when_secret_unset(self, db, monkeypatch):
        """Empty SENTINEL_AGENT_KEY must hard-reject every attempt
        (no auto-allow when secret is unset)."""
        monkeypatch.setattr(settings, "SENTINEL_AGENT_KEY", "")
        client = TestClient(app)
        response = client.get(
            "/api/sentinel/runs/pending",
            headers={"X-Sentinel-Agent-Key": ""},
        )
        assert response.status_code == 401

    def test_start_transitions_pending_to_running(self, agent_client, db):
        run = _make_run(db, run_id="start_me", outcome="pending")
        response = agent_client.post(
            f"/api/sentinel/runs/{run.id}/start", headers=self._agent_headers()
        )
        assert response.status_code == 200
        db.expire_all()
        refreshed = db.query(SentinelRun).filter_by(id=run.id).first()
        assert refreshed.outcome == "running"
        assert refreshed.started_at is not None

    def test_start_idempotent_on_running(self, agent_client, db):
        """Calling /start twice should not error — agent retries
        on transient connection failures.  The second call returns
        the existing row unchanged."""
        run = _make_run(db, run_id="dupe_start", outcome="pending")
        agent_client.post(
            f"/api/sentinel/runs/{run.id}/start", headers=self._agent_headers()
        )
        response = agent_client.post(
            f"/api/sentinel/runs/{run.id}/start", headers=self._agent_headers()
        )
        # Either 200 (idempotent) or 409 (explicit "already running")
        # — both are fine, but it must not 500.
        assert response.status_code in (200, 409)

    def test_complete_marks_terminal(self, agent_client, db):
        run = _make_run(db, run_id="complete_me", outcome="running",
                        started_at=datetime.now(tz=UTC).replace(tzinfo=None))
        response = agent_client.post(
            f"/api/sentinel/runs/{run.id}/complete",
            headers=self._agent_headers(),
            json={
                "outcome": "no_action",
                "summary": "nothing of note",
                "tool_call_count": 3,
            },
        )
        assert response.status_code == 200
        db.expire_all()
        refreshed = db.query(SentinelRun).filter_by(id=run.id).first()
        assert refreshed.outcome == "no_action"
        assert refreshed.summary == "nothing of note"
        assert refreshed.completed_at is not None

    def test_complete_idempotent_on_terminal_row(self, agent_client, db):
        """Second /complete on a terminal row returns the existing
        state without overwriting.  Required for the strand-cleanup
        wrapper in the agent — it best-effort completes a run that
        may have already completed during cancellation unwind."""
        run = _make_run(db, run_id="complete_twice", outcome="running",
                        started_at=datetime.now(tz=UTC).replace(tzinfo=None))
        agent_client.post(
            f"/api/sentinel/runs/{run.id}/complete",
            headers=self._agent_headers(),
            json={"outcome": "incident", "severity": "high",
                  "summary": "first", "tool_call_count": 1},
        )
        # Second call with different outcome — idempotency kicks in,
        # the original "incident" state wins.
        response = agent_client.post(
            f"/api/sentinel/runs/{run.id}/complete",
            headers=self._agent_headers(),
            json={"outcome": "error", "summary": "second", "tool_call_count": 0},
        )
        assert response.status_code == 200
        db.expire_all()
        refreshed = db.query(SentinelRun).filter_by(id=run.id).first()
        assert refreshed.outcome == "incident"
        assert refreshed.summary == "first"

    def test_complete_rejects_foreign_org_incident_id(self, agent_client, db):
        """Today's hardening pass: /complete cross-checks that
        body.incident_id belongs to the run's org.  A leaked agent
        key should not be able to attach a foreign-org incident to
        a run."""
        run = _make_run(db, org_id="org_a", run_id="cross_org",
                        outcome="running",
                        started_at=datetime.now(tz=UTC).replace(tzinfo=None))
        # Incident in a DIFFERENT org
        foreign = Incident(
            org_id="org_other",
            title="not yours",
            summary="x",
            severity="low",
            status="open",
            created_by="mcp:test",
        )
        db.add(foreign)
        db.commit()
        db.refresh(foreign)

        response = agent_client.post(
            f"/api/sentinel/runs/{run.id}/complete",
            headers=self._agent_headers(),
            json={
                "outcome": "incident",
                "severity": "low",
                "incident_id": foreign.id,
                "summary": "trying to cross orgs",
                "tool_call_count": 1,
            },
        )
        assert response.status_code == 400
        assert "does not belong" in response.json()["detail"]


# ── sentinel_dispatch helpers (no HTTP layer) ────────────────────


class TestPlanCapHelpers:
    """The cap_for_plan / cap_remaining functions back the cap-race
    protection.  Verify the per-plan numbers match what the UI / API
    promise."""

    def test_cap_for_plan_returns_correct_caps(self):
        assert cap_for_plan("pro") == 100
        assert cap_for_plan("pro_plus") == 500
        # Ineligible plans get 0 — fail-closed if anything bypasses
        # the upstream gate.
        assert cap_for_plan("free_org") == 0
        assert cap_for_plan("") == 0
        assert cap_for_plan(None) == 0
        assert cap_for_plan("nonsense") == 0

    def test_sentinel_plans_membership(self):
        assert "pro" in SENTINEL_PLANS
        assert "pro_plus" in SENTINEL_PLANS
        assert "free_org" not in SENTINEL_PLANS
        assert "" not in SENTINEL_PLANS

    def test_monthly_cap_by_plan_constants(self):
        # Lock the published numbers — if these change, the docs
        # claim "100 / 500 runs/month" needs to change too.
        assert MONTHLY_RUN_CAP_BY_PLAN["pro"] == 100
        assert MONTHLY_RUN_CAP_BY_PLAN["pro_plus"] == 500


class TestScheduleParsing:
    """_parse_hhmm_to_minutes — earlier today's fix replaced
    int(value.split(':')[0]) with a real HH:MM parser.  Lock it in."""

    def test_parses_hhmm_correctly(self):
        assert _parse_hhmm_to_minutes("00:00", 0) == 0
        assert _parse_hhmm_to_minutes("06:00", 0) == 360
        assert _parse_hhmm_to_minutes("22:30", 0) == 22 * 60 + 30
        assert _parse_hhmm_to_minutes("23:59", 0) == 23 * 60 + 59

    def test_parses_hour_only(self):
        assert _parse_hhmm_to_minutes("9", 0) == 9 * 60
        assert _parse_hhmm_to_minutes("22", 0) == 22 * 60

    def test_clamps_to_valid_range(self):
        # 25:99 would naively be 25*60+99 = 1599, but we clamp to 24h.
        assert _parse_hhmm_to_minutes("25:99", 0) <= 24 * 60

    def test_falls_back_on_garbage(self):
        assert _parse_hhmm_to_minutes("abc", 360) == 360
        assert _parse_hhmm_to_minutes("", 360) == 360
        assert _parse_hhmm_to_minutes(None, 360) == 360


class TestMotionCooldown:
    """Per-camera motion cooldown — earlier today's fix wired
    `motion_cooldown_min` from the SentinelConfig into a real gate."""

    def test_allows_when_no_recent_run(self, db):
        cfg = SentinelConfig(org_id="org_t", motion_cooldown_min=5)
        db.add(cfg)
        db.commit()
        assert _motion_cooldown_allows(cfg, "motion", "cam_a", db) is True

    def test_blocks_when_recent_motion_run_on_same_camera(self, db):
        cfg = SentinelConfig(org_id="org_t", motion_cooldown_min=5)
        db.add(cfg)
        db.commit()
        # A run 2 minutes ago — within the 5-minute cooldown
        _make_run(
            db, org_id="org_t", camera_id="cam_a", trigger="motion",
            triggered_at=datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(minutes=2),
        )
        assert _motion_cooldown_allows(cfg, "motion", "cam_a", db) is False

    def test_allows_when_recent_run_on_different_camera(self, db):
        cfg = SentinelConfig(org_id="org_t", motion_cooldown_min=5)
        db.add(cfg)
        db.commit()
        _make_run(
            db, org_id="org_t", camera_id="cam_other", trigger="motion",
            triggered_at=datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(minutes=2),
        )
        # Different camera — cam_a is free to fire
        assert _motion_cooldown_allows(cfg, "motion", "cam_a", db) is True

    def test_skips_check_for_non_motion_triggers(self, db):
        cfg = SentinelConfig(org_id="org_t", motion_cooldown_min=5)
        db.add(cfg)
        db.commit()
        # Even with a recent motion run, incident_opened triggers fire
        _make_run(
            db, org_id="org_t", camera_id="cam_a", trigger="motion",
            triggered_at=datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(minutes=1),
        )
        assert _motion_cooldown_allows(cfg, "incident_opened", "cam_a", db) is True

    def test_skips_check_when_cooldown_zero(self, db):
        cfg = SentinelConfig(org_id="org_t", motion_cooldown_min=0)
        db.add(cfg)
        db.commit()
        _make_run(
            db, org_id="org_t", camera_id="cam_a", trigger="motion",
            triggered_at=datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(seconds=1),
        )
        assert _motion_cooldown_allows(cfg, "motion", "cam_a", db) is True


class TestStrandedRunReaper:
    """Stranded-run reaper — sweeper that catches the case where the
    agent crashes mid-run before its own cleanup wrapper fires."""

    def test_reaps_run_stuck_in_running_past_threshold(self, db):
        too_old = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(
            minutes=STRANDED_RUN_AGE_MINUTES + 5
        )
        _make_run(db, run_id="stranded", outcome="running", started_at=too_old)
        result = reap_stranded_runs(db)
        assert result["reaped"] == 1
        assert "stranded" in result["ids"]
        db.expire_all()
        refreshed = db.query(SentinelRun).filter_by(id="stranded").first()
        assert refreshed.outcome == "error"
        assert refreshed.completed_at is not None

    def test_does_not_reap_recent_running_run(self, db):
        recent = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(minutes=2)
        _make_run(db, run_id="fresh", outcome="running", started_at=recent)
        result = reap_stranded_runs(db)
        assert result["reaped"] == 0
        db.expire_all()
        refreshed = db.query(SentinelRun).filter_by(id="fresh").first()
        assert refreshed.outcome == "running"

    def test_does_not_reap_terminal_runs(self, db):
        old = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(hours=3)
        _make_run(db, run_id="done_old", outcome="incident", started_at=old)
        result = reap_stranded_runs(db)
        assert result["reaped"] == 0


class TestCanDispatchForKind:
    """The full dispatch gate — plan check, trigger toggle, scope,
    schedule, motion cooldown, monthly cap.  This is the function
    that decides whether a notification turns into a Sentinel run."""

    def test_blocks_when_disabled(self, db):
        cfg = SentinelConfig(org_id="org_t", enabled=False)
        db.add(cfg)
        db.commit()
        ok, reason = _can_dispatch_for_kind(cfg, "motion", "cam_a", db)
        assert ok is False
        assert reason == "sentinel_disabled"

    def test_blocks_when_plan_not_eligible(self, db):
        _set_org_plan(db, "org_t", "free_org")
        cfg = SentinelConfig(org_id="org_t", enabled=True)
        db.add(cfg)
        db.commit()
        ok, reason = _can_dispatch_for_kind(cfg, "motion", "cam_a", db)
        assert ok is False
        assert reason == "plan_not_eligible"

    def test_blocks_when_motion_trigger_off(self, db):
        _set_org_plan(db, "org_t", "pro")
        cfg = SentinelConfig(org_id="org_t", enabled=True, motion_enabled=False)
        db.add(cfg)
        db.commit()
        ok, reason = _can_dispatch_for_kind(cfg, "motion", "cam_a", db)
        assert ok is False
        assert "motion_enabled" in reason

    def test_blocks_unknown_kind(self, db):
        _set_org_plan(db, "org_t", "pro")
        cfg = SentinelConfig(org_id="org_t", enabled=True)
        db.add(cfg)
        db.commit()
        ok, reason = _can_dispatch_for_kind(cfg, "nonsense_kind", "cam_a", db)
        assert ok is False
        assert reason == "kind_not_a_sentinel_trigger"

    def test_passes_for_eligible_pro_org(self, db):
        _set_org_plan(db, "org_t", "pro")
        cfg = SentinelConfig(org_id="org_t", enabled=True, motion_enabled=True)
        db.add(cfg)
        db.commit()
        ok, reason = _can_dispatch_for_kind(cfg, "motion", "cam_a", db)
        assert ok is True
        assert reason == "ok"


class TestCapRaceProtection:
    """`_commit_run_with_cap_check` re-counts AFTER flush() to catch
    the case where two concurrent dispatchers both pass the gate
    pre-INSERT.  The recount inside the txn sees the new row + any
    racer's row + every existing row, so the second writer rolls
    back."""

    def test_commits_under_cap(self, db):
        run = SentinelRun(
            id="under_cap_run",
            org_id="org_t",
            triggered_at=datetime.now(tz=UTC).replace(tzinfo=None),
            trigger_type="motion",
            outcome="pending",
        )
        committed = _commit_run_with_cap_check(db, run, "org_t", cap=100)
        assert committed is True
        assert db.query(SentinelRun).filter_by(id="under_cap_run").count() == 1

    def test_rolls_back_when_recount_exceeds_cap(self, db):
        # Pre-fill: 100 existing runs already at the cap
        for i in range(100):
            _make_run(db, org_id="org_t", run_id=f"existing_{i}")
        # The 101st run — recount inside the commit will see 101, > 100, roll back
        run = SentinelRun(
            id="should_lose_race",
            org_id="org_t",
            triggered_at=datetime.now(tz=UTC).replace(tzinfo=None),
            trigger_type="motion",
            outcome="pending",
        )
        committed = _commit_run_with_cap_check(db, run, "org_t", cap=100)
        assert committed is False
        # Row was rolled back — should not exist in the DB
        assert db.query(SentinelRun).filter_by(id="should_lose_race").count() == 0


class TestDispatchManualRun:
    """The manual-run dispatch path used by /api/sentinel/runs/manual.
    Raises ValueError on cap exhaustion or ineligible plan; the route
    handler converts those to 429 / 402 respectively."""

    def test_raises_plan_not_eligible_for_free_org(self, db, monkeypatch):
        _set_org_plan(db, "org_t", "free_org")
        # Suppress the wakeup-webhook fire-and-forget thread to keep
        # the test deterministic.
        monkeypatch.setattr(
            "app.core.sentinel_dispatch._fire_wakeup_webhook", lambda: None
        )
        with pytest.raises(ValueError, match="plan_not_eligible"):
            dispatch_manual_run(db, org_id="org_t", prompt="x")

    def test_raises_cap_reached_when_pro_at_cap(self, db, monkeypatch):
        _set_org_plan(db, "org_t", "pro")
        monkeypatch.setattr(
            "app.core.sentinel_dispatch._fire_wakeup_webhook", lambda: None
        )
        # Burn the Pro cap
        for i in range(100):
            _make_run(db, org_id="org_t", run_id=f"used_{i}", outcome="no_action")
        with pytest.raises(ValueError, match="monthly_cap_reached"):
            dispatch_manual_run(db, org_id="org_t", prompt="x")

    def test_creates_pending_run_for_pro_org_under_cap(self, db, monkeypatch):
        _set_org_plan(db, "org_t", "pro")
        monkeypatch.setattr(
            "app.core.sentinel_dispatch._fire_wakeup_webhook", lambda: None
        )
        run = dispatch_manual_run(
            db, org_id="org_t", prompt="custom inspection", camera_id="cam_x"
        )
        assert run.outcome == "pending"
        assert run.trigger_type == "manual"
        assert run.camera_id == "cam_x"
        assert run.manual_prompt == "custom inspection"

    def test_creates_default_config_for_org_without_one(self, db, monkeypatch):
        """Manual run on an org that's never opened the Sentinel
        page should still work — the dispatcher creates a default
        config row on the fly."""
        _set_org_plan(db, "org_t", "pro")
        monkeypatch.setattr(
            "app.core.sentinel_dispatch._fire_wakeup_webhook", lambda: None
        )
        assert db.query(SentinelConfig).filter_by(org_id="org_t").count() == 0
        dispatch_manual_run(db, org_id="org_t", prompt="x")
        assert db.query(SentinelConfig).filter_by(org_id="org_t").count() == 1


class TestRunsUsedThisMonth:
    """The cap counter — counts every run regardless of outcome
    (pending + running + terminal all bill against the monthly cap)."""

    def test_counts_all_outcomes(self, db):
        _make_run(db, org_id="org_t", run_id="a", outcome="pending")
        _make_run(db, org_id="org_t", run_id="b", outcome="running")
        _make_run(db, org_id="org_t", run_id="c", outcome="incident")
        _make_run(db, org_id="org_t", run_id="d", outcome="no_action")
        _make_run(db, org_id="org_t", run_id="e", outcome="error")
        assert runs_used_this_month(db, "org_t") == 5

    def test_excludes_runs_from_last_month(self, db):
        last_month = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(days=40)
        _make_run(db, org_id="org_t", run_id="old", triggered_at=last_month)
        _make_run(db, org_id="org_t", run_id="new")
        assert runs_used_this_month(db, "org_t") == 1

    def test_excludes_other_orgs(self, db):
        _make_run(db, org_id="org_a", run_id="mine")
        _make_run(db, org_id="org_b", run_id="theirs")
        assert runs_used_this_month(db, "org_a") == 1
        assert runs_used_this_month(db, "org_b") == 1


class TestCapRemaining:
    def test_pro_org_with_no_runs_has_full_cap(self, db):
        _set_org_plan(db, "org_t", "pro")
        assert cap_remaining(db, "org_t") == 100

    def test_pro_plus_org_with_no_runs_has_full_cap(self, db):
        _set_org_plan(db, "org_t", "pro_plus")
        assert cap_remaining(db, "org_t") == 500

    def test_remaining_decrements_with_runs(self, db):
        _set_org_plan(db, "org_t", "pro")
        for i in range(7):
            _make_run(db, org_id="org_t", run_id=f"r_{i}")
        assert cap_remaining(db, "org_t") == 100 - 7

    def test_returns_zero_for_ineligible_plan(self, db):
        _set_org_plan(db, "org_t", "free_org")
        assert cap_remaining(db, "org_t") == 0


# ── MCP agent-key resolver: paused-org manual-run exemption ──────


class TestAgentResolverPausedManualExemption:
    """_resolve_via_agent_key refuses tool calls for a Sentinel-disabled
    org EXCEPT while a manual run is in flight — dispatch_manual_run
    deliberately allows one-off "Run now" checks on a paused agent, and
    the UI offers the button, so refusing tools mid-run turned every
    such run into junk errors that still burned a monthly-cap slot and
    the LLM spend.

    Regression context: the exemption's first version filtered on a
    nonexistent ``status`` column (SentinelRun's unified state field is
    ``outcome``), so the query raised at build time and the resolver's
    blanket except converted it to "Authentication error" — the
    exemption was dead code and nothing caught it because this path had
    zero test coverage.  These tests are that coverage.
    """

    def _resolve(self, org_id):
        from app.mcp.server import _resolve_via_agent_key
        return _resolve_via_agent_key(
            {"x-agent-org-override": org_id}, "test-agent-key",
        )

    def test_paused_org_with_running_manual_run_is_allowed(self, db):
        org = "org_paused_manual"
        _set_org_plan(db, org, "pro")
        db.add(SentinelConfig(org_id=org, enabled=False))
        db.commit()
        _make_run(db, org_id=org, trigger="manual", outcome="running")

        resolved_org, resolver_db = self._resolve(org)
        assert resolved_org == org
        resolver_db.close()

    def test_paused_org_without_manual_run_is_refused(self, db):
        from fastmcp.exceptions import ToolError

        org = "org_paused_idle"
        _set_org_plan(db, org, "pro")
        db.add(SentinelConfig(org_id=org, enabled=False))
        db.commit()
        # A running NON-manual run must not open the gate (motion runs
        # on a disabled org are stale dispatches, exactly what the
        # defence-in-depth check exists to stop).
        _make_run(db, org_id=org, trigger="motion", outcome="running")

        with pytest.raises(ToolError, match="disabled"):
            self._resolve(org)

    def test_enabled_org_resolves_without_any_run(self, db):
        org = "org_enabled_normal"
        _set_org_plan(db, org, "pro")
        db.add(SentinelConfig(org_id=org, enabled=True))
        db.commit()

        resolved_org, resolver_db = self._resolve(org)
        assert resolved_org == org
        resolver_db.close()
