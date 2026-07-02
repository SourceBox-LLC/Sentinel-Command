"""Camera and settings endpoint tests."""

from app.models.models import Setting


def test_list_cameras_empty(admin_client):
    """Empty org returns empty camera list."""
    from app.core.auth import require_view
    from tests.conftest import _make_admin_user
    app = admin_client.app
    app.dependency_overrides[require_view] = lambda: _make_admin_user()

    resp = admin_client.get("/api/cameras")
    assert resp.status_code == 200
    assert resp.json() == []


def test_get_settings(admin_client):
    """`/api/settings` returns the org-level notification block.

    Recording configuration moved per-camera in v0.1.43; the response
    no longer includes a `recording` key.  Pin both: the
    notifications block is intact, the recording block is gone.
    """
    from app.core.auth import require_view
    from tests.conftest import _make_admin_user
    app = admin_client.app
    app.dependency_overrides[require_view] = lambda: _make_admin_user()

    resp = admin_client.get("/api/settings")
    assert resp.status_code == 200
    data = resp.json()

    # Recording config moved per-camera (Camera.recording_policy on
    # /api/cameras + PATCH /api/cameras/{id}/recording-settings).
    # The org-level `recording` key is gone.
    assert "recording" not in data

    # Notifications block intact.  All three default-on for orgs that
    # pre-date the toggle.
    assert "notifications" in data
    notifications = data["notifications"]
    assert notifications["motion_notifications"] is True
    assert notifications["camera_transition_notifications"] is True
    assert notifications["node_transition_notifications"] is True


def test_get_notification_settings_defaults_on(admin_client):
    """New orgs see all notification toggles enabled by default."""
    resp = admin_client.get("/api/settings/notifications")
    assert resp.status_code == 200
    body = resp.json()
    assert body["motion_notifications"] is True
    assert body["camera_transition_notifications"] is True
    assert body["node_transition_notifications"] is True


def test_update_notification_settings(admin_client):
    """Admin can flip motion notifications off and the change persists."""
    resp = admin_client.post("/api/settings/notifications", json={
        "motion_notifications": False,
        "camera_transition_notifications": True,
        "node_transition_notifications": True,
    })
    assert resp.status_code == 200
    assert resp.json()["success"] is True

    # Round-trip: GET must reflect the change
    follow = admin_client.get("/api/settings/notifications")
    assert follow.status_code == 200
    body = follow.json()
    assert body["motion_notifications"] is False
    assert body["camera_transition_notifications"] is True
    assert body["node_transition_notifications"] is True


def test_notification_settings_reflected_in_all_settings(admin_client):
    """After toggling motion off, /api/settings aggregate shows the change."""
    admin_client.post("/api/settings/notifications", json={
        "motion_notifications": False,
        "camera_transition_notifications": True,
        "node_transition_notifications": True,
    })
    resp = admin_client.get("/api/settings")
    assert resp.status_code == 200
    assert resp.json()["notifications"]["motion_notifications"] is False


def test_get_motion_ingestion_default_enabled(admin_client):
    """Default is on — kill switch only filters after explicit opt-out.
    Critical to verify because flipping the default would silently
    disable motion ingestion on every existing org."""
    resp = admin_client.get("/api/settings/motion-ingestion")
    assert resp.status_code == 200
    assert resp.json() == {"motion_ingestion_enabled": True}


def test_motion_ingestion_toggle_persists(admin_client):
    """Admin disables, GET reflects, admin re-enables, GET reflects."""
    off = admin_client.post("/api/settings/motion-ingestion", json={"enabled": False})
    assert off.status_code == 200
    assert off.json()["motion_ingestion_enabled"] is False

    follow = admin_client.get("/api/settings/motion-ingestion")
    assert follow.json()["motion_ingestion_enabled"] is False

    on = admin_client.post("/api/settings/motion-ingestion", json={"enabled": True})
    assert on.json()["motion_ingestion_enabled"] is True

    follow_on = admin_client.get("/api/settings/motion-ingestion")
    assert follow_on.json()["motion_ingestion_enabled"] is True


def test_motion_ingestion_toggle_requires_admin(viewer_client):
    """Members without admin can't flip the kill switch."""
    resp = viewer_client.post(
        "/api/settings/motion-ingestion", json={"enabled": False}
    )
    # Same rejection shape as other admin-gated settings endpoints.
    assert resp.status_code in (401, 403, 404)


def test_update_notification_settings_requires_admin(viewer_client):
    """Non-admin members can't flip the toggles."""
    resp = viewer_client.post("/api/settings/notifications", json={
        "motion_notifications": False,
        "camera_transition_notifications": True,
        "node_transition_notifications": True,
    })
    # require_admin yields 401/403/404 depending on the auth layer's
    # rejection path — viewer_client doesn't override require_admin so
    # the real dependency runs and rejects.
    assert resp.status_code in (401, 403)


# ─── Per-camera recording policy (v0.1.43+) ──────────────────────────
#
# Recording configuration moved from org-level `/api/settings/recording`
# (which never actually drove anything) to per-camera columns on the
# Camera row + a PATCH endpoint.  Heartbeat handler reads them per
# tick and tells CameraNode which cameras should be archiving via
# `recording_state` in the response.


def _seed_camera(db, *, camera_id="cam_rec_test", org_id="org_test123"):
    """Create a node + camera for recording-policy tests."""
    from app.models.models import Camera, CameraNode
    node = CameraNode(
        node_id=f"nd_{camera_id}", org_id=org_id,
        api_key_hash="a" * 64, name=f"node-{camera_id}",
    )
    db.add(node)
    db.flush()
    db.add(Camera(
        camera_id=camera_id, org_id=org_id,
        node_id=node.id, name=camera_id,
    ))
    db.commit()
    return camera_id


def test_patch_recording_policy_persists_continuous(admin_client, db):
    """PATCH flips continuous_24_7 and audit log + response reflect it."""
    cam_id = _seed_camera(db)
    resp = admin_client.patch(
        f"/api/cameras/{cam_id}/recording-settings",
        json={"continuous_24_7": True},
    )
    assert resp.status_code == 200
    body = resp.json()
    assert body["recording_policy"]["continuous_24_7"] is True
    # Other fields untouched (PATCH semantics, not PUT).
    assert body["recording_policy"]["scheduled_recording"] is False


def test_patch_recording_policy_rejects_both_modes_on(admin_client, db):
    """continuous_24_7 and scheduled_recording are mutually exclusive
    — the heartbeat handler would silently ignore scheduled when
    continuous is also on, which is confusing UX.  The frontend
    auto-clears the other mode when the operator toggles one on, so
    this 422 only fires for direct API / MCP callers.
    """
    cam_id = _seed_camera(db, camera_id="cam_rec_conflict")
    # First, get continuous on.
    resp = admin_client.patch(
        f"/api/cameras/{cam_id}/recording-settings",
        json={"continuous_24_7": True},
    )
    assert resp.status_code == 200

    # Now try to ALSO turn scheduled on without clearing continuous.
    resp = admin_client.patch(
        f"/api/cameras/{cam_id}/recording-settings",
        json={"scheduled_recording": True},
    )
    assert resp.status_code == 422
    assert "cannot both" in resp.json()["detail"].lower()

    # The proper way: switch modes by passing both fields.
    resp = admin_client.patch(
        f"/api/cameras/{cam_id}/recording-settings",
        json={"continuous_24_7": False, "scheduled_recording": True},
    )
    assert resp.status_code == 200
    body = resp.json()
    assert body["recording_policy"]["continuous_24_7"] is False
    assert body["recording_policy"]["scheduled_recording"] is True


def test_patch_recording_policy_validates_hhmm(admin_client, db):
    """A malformed scheduled_start should 422 — never silently store
    garbage that the heartbeat handler then has to defend against."""
    cam_id = _seed_camera(db, camera_id="cam_rec_validate")
    resp = admin_client.patch(
        f"/api/cameras/{cam_id}/recording-settings",
        json={"scheduled_recording": True, "scheduled_start": "not-a-time"},
    )
    assert resp.status_code == 422


def test_patch_recording_policy_404_for_unknown_camera(admin_client, db):
    resp = admin_client.patch(
        "/api/cameras/cam_nonexistent/recording-settings",
        json={"continuous_24_7": True},
    )
    assert resp.status_code == 404


def test_post_recording_button_flips_continuous_24_7(admin_client, db):
    """The dashboard's manual record button (POST .../recording) is now
    a thin wrapper that flips continuous_24_7 — no WebSocket command.
    Verify the column changes so the heartbeat reconciler sees it."""
    from app.models.models import Camera
    cam_id = _seed_camera(db, camera_id="cam_rec_button")

    resp = admin_client.post(
        f"/api/cameras/{cam_id}/recording", json={"recording": True}
    )
    assert resp.status_code == 200
    assert resp.json()["recording"] is True

    cam = db.query(Camera).filter_by(camera_id=cam_id).first()
    assert cam.continuous_24_7 is True

    # Stop.
    admin_client.post(f"/api/cameras/{cam_id}/recording", json={"recording": False})
    db.refresh(cam)
    assert cam.continuous_24_7 is False


def test_camera_should_record_now_window_logic():
    """Unit-test the wall-clock window logic in isolation.  Heartbeat
    decisions hinge on this; a bug here silently breaks scheduled
    recording for everyone."""
    from datetime import datetime
    from unittest.mock import patch
    from zoneinfo import ZoneInfo

    from app.api.nodes import _camera_should_record_now
    from app.models.models import Camera

    utc = ZoneInfo("UTC")
    cam = Camera(
        camera_id="x", org_id="o", name="x",
        continuous_24_7=False, scheduled_recording=False,
    )
    # Empty policy → no recording.
    assert _camera_should_record_now(cam, utc) is False

    # Continuous overrides scheduled.
    cam.continuous_24_7 = True
    assert _camera_should_record_now(cam, utc) is True

    # Scheduled, in-window (UTC).
    cam.continuous_24_7 = False
    cam.scheduled_recording = True
    cam.scheduled_start = "08:00"
    cam.scheduled_end = "17:00"
    fake_now = datetime(2026, 4, 29, 12, 0, tzinfo=utc)
    with patch("app.api.nodes.datetime") as mock_dt:
        mock_dt.now.return_value = fake_now
        assert _camera_should_record_now(cam, utc) is True

    # Scheduled, out-of-window.
    fake_now = datetime(2026, 4, 29, 18, 30, tzinfo=utc)
    with patch("app.api.nodes.datetime") as mock_dt:
        mock_dt.now.return_value = fake_now
        assert _camera_should_record_now(cam, utc) is False

    # Wrap-around overnight schedule (22:00–06:00).  Inside window.
    cam.scheduled_start = "22:00"
    cam.scheduled_end = "06:00"
    fake_now = datetime(2026, 4, 29, 23, 30, tzinfo=utc)
    with patch("app.api.nodes.datetime") as mock_dt:
        mock_dt.now.return_value = fake_now
        assert _camera_should_record_now(cam, utc) is True

    # Wrap-around, before window.
    fake_now = datetime(2026, 4, 29, 12, 0, tzinfo=utc)
    with patch("app.api.nodes.datetime") as mock_dt:
        mock_dt.now.return_value = fake_now
        assert _camera_should_record_now(cam, utc) is False


def test_post_org_timezone_persists_and_validates(admin_client, db):
    """The org timezone POST should accept IANA names, persist them
    via the Setting kv table, and reject typos / made-up zones with
    a 422 so a bad value can't land in the DB and silently break
    the heartbeat handler's window check.
    """
    from app.models.models import Setting

    # Happy path.
    resp = admin_client.post(
        "/api/settings/timezone",
        json={"timezone": "America/Los_Angeles"},
    )
    assert resp.status_code == 200
    assert resp.json()["timezone"] == "America/Los_Angeles"
    assert (
        Setting.get(db, "org_test123", "timezone") == "America/Los_Angeles"
    )

    # Made-up zone.
    resp = admin_client.post(
        "/api/settings/timezone", json={"timezone": "Mars/Olympus_Mons"},
    )
    assert resp.status_code == 422

    # Empty body.
    resp = admin_client.post("/api/settings/timezone", json={"timezone": ""})
    assert resp.status_code == 422


def test_get_settings_includes_timezone(admin_client, db):
    """``GET /api/settings`` exposes the org's timezone alongside
    notifications.  Defaults to UTC for a fresh org so the frontend
    has something to render before the operator picks one."""
    resp = admin_client.get("/api/settings")
    assert resp.status_code == 200
    body = resp.json()
    # Default for a fresh org with no Setting row yet.
    assert body["timezone"] == "UTC"

    # After setting it, the getter should reflect the choice.
    from app.models.models import Setting
    Setting.set(db, "org_test123", "timezone", "Europe/London")
    resp = admin_client.get("/api/settings")
    assert resp.json()["timezone"] == "Europe/London"


def test_camera_should_record_now_honours_org_timezone():
    """A schedule of 08:00–17:00 in Los Angeles fires at 8am LA time,
    not 8am UTC.  This is the whole point of the per-org timezone —
    operators get to think in their local wall clock.
    """
    from datetime import datetime
    from unittest.mock import patch
    from zoneinfo import ZoneInfo

    from app.api.nodes import _camera_should_record_now
    from app.models.models import Camera

    la = ZoneInfo("America/Los_Angeles")
    cam = Camera(
        camera_id="x", org_id="o", name="x",
        continuous_24_7=False, scheduled_recording=True,
        scheduled_start="08:00", scheduled_end="17:00",
    )

    # 12:00 UTC on 2026-06-15 = 05:00 PDT (summer, UTC-7).
    # 5am is BEFORE the 08:00 LA window → should NOT record.
    fake_now = datetime(2026, 6, 15, 12, 0, tzinfo=ZoneInfo("UTC"))
    with patch("app.api.nodes.datetime") as mock_dt:
        mock_dt.now.return_value = fake_now.astimezone(la)
        assert _camera_should_record_now(cam, la) is False

    # 18:00 UTC on 2026-06-15 = 11:00 PDT.  Inside window.
    fake_now = datetime(2026, 6, 15, 18, 0, tzinfo=ZoneInfo("UTC"))
    with patch("app.api.nodes.datetime") as mock_dt:
        mock_dt.now.return_value = fake_now.astimezone(la)
        assert _camera_should_record_now(cam, la) is True

    # DST sanity: same wall-clock hour in winter (PST, UTC-8).
    # 18:00 UTC on 2026-12-15 = 10:00 PST.  Inside window.
    # Without DST handling, naive UTC arithmetic would put this at
    # the wrong wall-clock hour.  ZoneInfo handles it for free.
    fake_now = datetime(2026, 12, 15, 18, 0, tzinfo=ZoneInfo("UTC"))
    with patch("app.api.nodes.datetime") as mock_dt:
        mock_dt.now.return_value = fake_now.astimezone(la)
        assert _camera_should_record_now(cam, la) is True


# ─── Danger zone (wipe-logs, full-reset) ─────────────────────────────
#
# These two endpoints are destructive and gated on a paid plan.  The
# audit flagged that the gate read ONLY the JWT `features` claim,
# which has up to ~1 minute of refresh lag after a Clerk plan change.
# A user who downgrades from Pro → Free would still pass the JWT
# check during that window and could fire the destructive action
# despite no longer having entitlement.  v0.1.40+ adds a server-side
# double-check via effective_plan_for_caps (DB-resolved current
# plan).  These tests pin both sides of the gate.


def _force_org_plan(monkeypatch, plan_slug: str) -> None:
    """Override the DB-resolved plan that the danger-zone helper sees.

    Patches ``effective_plan_for_caps`` at its definition site so the
    test doesn't need a Clerk SDK or a populated webhook cache.
    """
    from app.core import plans as plans_mod
    monkeypatch.setattr(
        plans_mod, "effective_plan_for_caps", lambda _db, _org: plan_slug
    )


def test_wipe_logs_requires_paid_plan_in_db_not_just_jwt(
    admin_client, monkeypatch
):
    """admin_client's JWT has features=['admin'], so the JWT gate
    passes.  But if the DB-resolved plan is free_org (e.g. user just
    downgraded but the JWT hasn't refreshed), the second check
    rejects with 403.  Stops the post-downgrade exploit window."""
    _force_org_plan(monkeypatch, "free_org")

    resp = admin_client.post("/api/settings/danger/wipe-logs")
    assert resp.status_code == 403
    assert "paid" in resp.json()["detail"].lower()


def test_wipe_logs_succeeds_when_db_plan_is_paid(admin_client, monkeypatch):
    """Both gates pass: JWT has admin, DB says pro.  Wipe goes through."""
    _force_org_plan(monkeypatch, "pro")

    resp = admin_client.post("/api/settings/danger/wipe-logs")
    assert resp.status_code == 200
    body = resp.json()
    assert body["success"] is True
    assert "deleted_logs" in body


def test_full_reset_works_on_free_plan(admin_client, monkeypatch):
    """Full reset is the GDPR Article 17 right-to-erasure path and is
    NOT gated on plan tier — every plan (Free included) can self-serve
    full erasure of their organization's data.

    Pins the negative direction of the previous test (which required a
    paid plan): downgrading to Free must NOT block the customer from
    deleting their data.  The legal obligation outranks the SaaS plan
    tier.  Sibling ``wipe-logs`` endpoint is still paid-only because
    it's selective audit hygiene, not erasure — see
    test_wipe_logs_requires_paid_plan_in_db_not_just_jwt above.
    """
    _force_org_plan(monkeypatch, "free_org")

    resp = admin_client.post("/api/settings/danger/full-reset")
    assert resp.status_code == 200
    body = resp.json()
    assert body["success"] is True
    assert "nodes_deleted" in body


def test_full_reset_works_on_pro_plus_too(admin_client, monkeypatch):
    """Mirror of the free-plan test — same endpoint, paid plan, same
    success shape.  Pins that lifting the gate didn't break the
    happy-path-for-paid customer."""
    _force_org_plan(monkeypatch, "pro_plus")

    resp = admin_client.post("/api/settings/danger/full-reset")
    assert resp.status_code == 200
    body = resp.json()
    assert body["success"] is True
    assert "nodes_deleted" in body


def test_camera_groups_crud(admin_client):
    """Create, list, and delete camera groups."""
    from app.core.auth import require_view
    from tests.conftest import _make_admin_user
    app = admin_client.app
    app.dependency_overrides[require_view] = lambda: _make_admin_user()

    # Create
    resp = admin_client.post("/api/camera-groups", json={"name": "Front Yard"})
    assert resp.status_code == 200
    group_id = resp.json()["id"]

    # List
    resp = admin_client.get("/api/camera-groups")
    assert resp.status_code == 200
    assert len(resp.json()) == 1
    assert resp.json()[0]["name"] == "Front Yard"

    # Delete
    resp = admin_client.delete(f"/api/camera-groups/{group_id}")
    assert resp.status_code == 200

    # Verify deleted
    resp = admin_client.get("/api/camera-groups")
    assert len(resp.json()) == 0

