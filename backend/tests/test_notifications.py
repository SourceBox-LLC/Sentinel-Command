"""
Tests for the notification inbox API.

Exercises:
  - list, unread-count, mark-viewed routes
  - per-user read state (last_viewed_at)
  - audience filtering (admin-only notifications hidden from viewers)
  - org isolation (notifications from another org don't leak)
  - notification_broadcaster delivery with audience gating
  - online↔offline transition helpers + debounce
  - offline sweep that flips stale 'online' rows
"""

import time
from datetime import UTC, datetime, timedelta, timezone

import pytest

from app.api.notifications import (
    clear_transition_debounce,
    create_notification,
    emit_camera_transition,
    emit_node_transition,
    notification_broadcaster,
    notifications_enabled,
)
from app.main import run_offline_sweep
from app.models.models import Camera, CameraNode, Notification, Setting, UserNotificationState


@pytest.fixture(autouse=True)
def _reset_transition_debounce():
    """Debounce is module-level state — must be cleared between tests so
    one test's emit doesn't silently suppress another test's emit."""
    clear_transition_debounce()
    yield
    clear_transition_debounce()


# ── List / read-state ──────────────────────────────────────────────

def test_list_notifications_empty(admin_client):
    resp = admin_client.get("/api/notifications")
    assert resp.status_code == 200
    data = resp.json()
    assert data["total"] == 0
    assert data["notifications"] == []


def test_create_and_list_notification(admin_client, db):
    create_notification(
        org_id="org_test123",
        kind="motion",
        title="Motion on Front Door",
        body="Scene change at 42% intensity.",
        severity="info",
        audience="all",
        camera_id="cam_abc",
        meta={"score": 42},
        db=db,
    )

    resp = admin_client.get("/api/notifications")
    assert resp.status_code == 200
    data = resp.json()
    assert data["total"] == 1
    item = data["notifications"][0]
    assert item["kind"] == "motion"
    assert item["title"] == "Motion on Front Door"
    assert item["camera_id"] == "cam_abc"
    assert item["meta"] == {"score": 42}


def test_audience_filter_hides_admin_only_from_viewer(viewer_client, db):
    # Admin-only notification — viewers should not see it.
    create_notification(
        org_id="org_test123",
        kind="node_offline",
        title="Node crashed",
        severity="warning",
        audience="admin",
        node_id="node_xyz",
        db=db,
    )
    # Everyone-notification — viewers should see it.
    create_notification(
        org_id="org_test123",
        kind="motion",
        title="Motion on cam_abc",
        audience="all",
        camera_id="cam_abc",
        db=db,
    )

    resp = viewer_client.get("/api/notifications")
    assert resp.status_code == 200
    data = resp.json()
    assert data["total"] == 1
    assert data["notifications"][0]["kind"] == "motion"


def test_audience_filter_admin_sees_all(admin_client, db):
    create_notification(org_id="org_test123", kind="node_offline",
                       title="Node down", audience="admin", db=db)
    create_notification(org_id="org_test123", kind="motion",
                       title="Motion", audience="all", db=db)

    resp = admin_client.get("/api/notifications")
    assert resp.status_code == 200
    assert resp.json()["total"] == 2


def test_org_isolation(admin_client, db):
    # Notification in a different org should never be visible.
    create_notification(org_id="other_org", kind="motion",
                       title="Someone else's motion", db=db)
    create_notification(org_id="org_test123", kind="motion",
                       title="My motion", db=db)

    resp = admin_client.get("/api/notifications")
    data = resp.json()
    assert data["total"] == 1
    assert data["notifications"][0]["title"] == "My motion"


# ── Unread count + mark-viewed ─────────────────────────────────────

def test_unread_count_new_user_sees_zero(admin_client, db):
    # Pre-existing notifications from before the user first opened the app
    # should NOT be counted as unread — first-view semantics initialises
    # last_viewed_at to "now".
    create_notification(org_id="org_test123", kind="motion", title="old", db=db)

    resp = admin_client.get("/api/notifications/unread-count")
    assert resp.status_code == 200
    assert resp.json()["unread"] == 0


def test_unread_count_bumps_on_new_notification(admin_client, db):
    # Prime last_viewed_at by hitting the endpoint once
    admin_client.get("/api/notifications/unread-count")

    # Sleep a beat so created_at > last_viewed_at is true
    time.sleep(0.05)

    create_notification(org_id="org_test123", kind="motion", title="new1", db=db)
    create_notification(org_id="org_test123", kind="motion", title="new2", db=db)

    resp = admin_client.get("/api/notifications/unread-count")
    assert resp.json()["unread"] == 2


def test_mark_viewed_clears_unread(admin_client, db):
    admin_client.get("/api/notifications/unread-count")
    time.sleep(0.05)
    create_notification(org_id="org_test123", kind="motion", title="x", db=db)
    assert admin_client.get("/api/notifications/unread-count").json()["unread"] == 1

    resp = admin_client.post("/api/notifications/mark-viewed")
    assert resp.status_code == 200
    assert admin_client.get("/api/notifications/unread-count").json()["unread"] == 0


# ── Clear all (per-user soft-hide) ─────────────────────────────────

def test_clear_all_hides_existing_notifications_from_list(admin_client, db):
    # Initialise the user's state, then create rows that would normally
    # show up in their inbox.
    admin_client.get("/api/notifications")
    time.sleep(0.05)
    create_notification(org_id="org_test123", kind="motion", title="a", db=db)
    create_notification(org_id="org_test123", kind="motion", title="b", db=db)
    assert admin_client.get("/api/notifications").json()["total"] == 2

    resp = admin_client.post("/api/notifications/clear-all")
    assert resp.status_code == 200
    body = resp.json()
    assert body["success"] is True
    assert "cleared_at" in body

    # List is empty for this user, but the rows are still in the DB
    # (soft-hide, not hard-delete).
    assert admin_client.get("/api/notifications").json()["total"] == 0
    from app.models.models import Notification as _N
    assert db.query(_N).count() == 2


def test_clear_all_also_zeroes_unread_count(admin_client, db):
    admin_client.get("/api/notifications/unread-count")
    time.sleep(0.05)
    create_notification(org_id="org_test123", kind="motion", title="x", db=db)
    assert admin_client.get("/api/notifications/unread-count").json()["unread"] == 1

    admin_client.post("/api/notifications/clear-all")
    assert admin_client.get("/api/notifications/unread-count").json()["unread"] == 0


def test_clear_all_does_not_hide_later_notifications(admin_client, db):
    # After clearing, new notifications should still appear — the clear
    # hides the snapshot at that point in time, it doesn't mute the
    # user's inbox forever.
    admin_client.get("/api/notifications")
    time.sleep(0.05)
    create_notification(org_id="org_test123", kind="motion", title="old", db=db)
    admin_client.post("/api/notifications/clear-all")
    assert admin_client.get("/api/notifications").json()["total"] == 0

    time.sleep(0.05)
    create_notification(org_id="org_test123", kind="motion", title="new", db=db)
    body = admin_client.get("/api/notifications").json()
    assert body["total"] == 1
    assert body["notifications"][0]["title"] == "new"


def test_clear_all_is_per_user_not_per_org(viewer_client, db):
    # One user clearing their inbox must not affect other users in the
    # same org — that's the whole point of per-user soft-hide vs.
    # hard-delete.  We simulate the "other user cleared" by stamping a
    # UserNotificationState row directly rather than juggling two
    # TestClient fixtures (which would clobber each other's auth
    # dependency overrides on the shared FastAPI app).
    viewer_client.get("/api/notifications")  # init viewer's state
    time.sleep(0.05)
    create_notification(org_id="org_test123", kind="motion", title="visible-to-all",
                       audience="all", db=db)

    # Simulate a different user in the same org clearing their inbox.
    now = datetime.now(tz=UTC).replace(tzinfo=None)
    other = UserNotificationState(
        clerk_user_id="user_someone_else",
        org_id="org_test123",
        cleared_at=now,
        last_viewed_at=now,
    )
    db.add(other)
    db.commit()

    # Viewer didn't clear → still sees the notification.
    assert viewer_client.get("/api/notifications").json()["total"] == 1


def test_unread_count_respects_audience(viewer_client, db):
    # Admin-only notifications never count toward a viewer's unread count.
    viewer_client.get("/api/notifications/unread-count")
    time.sleep(0.05)
    create_notification(org_id="org_test123", kind="node_offline",
                       title="admin only", audience="admin", db=db)
    assert viewer_client.get("/api/notifications/unread-count").json()["unread"] == 0


# ── Broadcaster ────────────────────────────────────────────────────

@pytest.mark.asyncio
async def test_broadcaster_delivers_to_matching_audience():
    admin_q = notification_broadcaster.subscribe("org_b", is_admin=True)
    viewer_q = notification_broadcaster.subscribe("org_b", is_admin=False)

    notification_broadcaster.notify("org_b", {"type": "notification",
                                               "audience": "all",
                                               "title": "everyone"})
    notification_broadcaster.notify("org_b", {"type": "notification",
                                               "audience": "admin",
                                               "title": "admin only"})

    admin_events = []
    while not admin_q.empty():
        admin_events.append(await admin_q.get())
    viewer_events = []
    while not viewer_q.empty():
        viewer_events.append(await viewer_q.get())

    assert [e["title"] for e in admin_events] == ["everyone", "admin only"]
    assert [e["title"] for e in viewer_events] == ["everyone"]

    notification_broadcaster.unsubscribe("org_b", admin_q)
    notification_broadcaster.unsubscribe("org_b", viewer_q)


# ── Status transitions ─────────────────────────────────────────────

def test_emit_camera_transition_offline_creates_warning(db):
    emit_camera_transition(
        db,
        camera_id="cam_front",
        org_id="org_test123",
        display_name="Front Door",
        new_status="offline",
        node_id="node_1",
    )
    rows = db.query(Notification).all()
    assert len(rows) == 1
    row = rows[0]
    assert row.kind == "camera_offline"
    assert row.audience == "all"
    assert row.severity == "warning"
    assert "Front Door" in row.title
    assert row.camera_id == "cam_front"
    assert row.node_id == "node_1"


def test_emit_camera_transition_online_creates_info(db):
    emit_camera_transition(
        db,
        camera_id="cam_front",
        org_id="org_test123",
        display_name="Front Door",
        new_status="online",
    )
    row = db.query(Notification).one()
    assert row.kind == "camera_online"
    assert row.severity == "info"
    assert row.audience == "all"


def test_emit_node_transition_is_admin_only(db):
    emit_node_transition(
        db,
        node_id="node_1",
        org_id="org_test123",
        display_name="Garage Pi",
        new_status="offline",
    )
    row = db.query(Notification).one()
    assert row.kind == "node_offline"
    assert row.audience == "admin"  # viewers don't need to know about node ops
    assert row.severity == "warning"
    assert "Garage Pi" in row.title


def test_transition_debounced_same_direction(db):
    # Two back-to-back offline emits should produce one notification.
    for _ in range(2):
        emit_camera_transition(
            db,
            camera_id="cam_x",
            org_id="org_test123",
            display_name="Cam X",
            new_status="offline",
        )
    assert db.query(Notification).count() == 1


def test_transition_not_debounced_across_directions(db):
    # offline → online are separate debounce keys: both fire.
    emit_camera_transition(
        db, camera_id="cam_x", org_id="org_test123",
        display_name="Cam X", new_status="offline",
    )
    emit_camera_transition(
        db, camera_id="cam_x", org_id="org_test123",
        display_name="Cam X", new_status="online",
    )
    kinds = [n.kind for n in db.query(Notification).order_by(Notification.id).all()]
    assert kinds == ["camera_offline", "camera_online"]


def test_transition_ignores_unknown_status(db):
    emit_camera_transition(
        db, camera_id="cam_x", org_id="org_test123",
        display_name="Cam X", new_status="pending",
    )
    assert db.query(Notification).count() == 0


def test_transition_different_cameras_not_debounced(db):
    # Debounce is per-(camera, direction) — different cameras don't interfere.
    emit_camera_transition(
        db, camera_id="cam_a", org_id="org_test123",
        display_name="Cam A", new_status="offline",
    )
    emit_camera_transition(
        db, camera_id="cam_b", org_id="org_test123",
        display_name="Cam B", new_status="offline",
    )
    assert db.query(Notification).count() == 2


def test_transition_emits_on_fresh_boot_low_monotonic(db, monkeypatch):
    # Regression: GitHub Actions runners can have a very small ``time.monotonic()``
    # value (freshly-booted VM), which would collide with the 60s debounce window
    # if the "never-emitted" sentinel was 0.0.  Simulate that here and make sure
    # the first emit still fires.
    monkeypatch.setattr("app.api.notifications._time.monotonic", lambda: 5.0)
    emit_camera_transition(
        db, camera_id="cam_fresh_boot", org_id="org_test123",
        display_name="Fresh Boot", new_status="offline",
    )
    assert db.query(Notification).count() == 1


# ── Offline sweep ─────────────────────────────────────────────────

def _make_node(db, *, node_id, org_id, status, last_seen_minutes_ago, name=None):
    now = datetime.now(tz=UTC).replace(tzinfo=None)
    node = CameraNode(
        node_id=node_id,
        org_id=org_id,
        api_key_hash="x" * 64,
        name=name or f"Node {node_id}",
        status=status,
        last_seen=now - timedelta(minutes=last_seen_minutes_ago) if last_seen_minutes_ago is not None else None,
    )
    db.add(node)
    db.commit()
    db.refresh(node)
    return node


def _make_camera(db, *, camera_id, org_id, status, last_seen_minutes_ago, node=None, name=None):
    now = datetime.now(tz=UTC).replace(tzinfo=None)
    cam = Camera(
        camera_id=camera_id,
        org_id=org_id,
        name=name or f"Cam {camera_id}",
        status=status,
        last_seen=now - timedelta(minutes=last_seen_minutes_ago) if last_seen_minutes_ago is not None else None,
        node_id=node.id if node else None,
    )
    db.add(cam)
    db.commit()
    db.refresh(cam)
    return cam


def test_offline_sweep_flips_stale_camera_and_emits(db):
    _make_camera(
        db, camera_id="cam_stale", org_id="org_test123",
        status="online", last_seen_minutes_ago=5,
    )

    summary = run_offline_sweep(db)

    assert summary["cameras_flipped"] == 1
    assert summary["nodes_flipped"] == 0

    cam = db.query(Camera).filter_by(camera_id="cam_stale").one()
    assert cam.status == "offline"

    notifs = db.query(Notification).all()
    assert len(notifs) == 1
    assert notifs[0].kind == "camera_offline"
    assert notifs[0].audience == "all"


def test_offline_sweep_skips_fresh_entities(db):
    _make_camera(
        db, camera_id="cam_fresh", org_id="org_test123",
        status="online", last_seen_minutes_ago=0,  # seconds-old heartbeat
    )

    summary = run_offline_sweep(db)

    assert summary["cameras_flipped"] == 0
    assert db.query(Notification).count() == 0
    cam = db.query(Camera).filter_by(camera_id="cam_fresh").one()
    assert cam.status == "online"


def test_offline_sweep_skips_already_offline(db):
    _make_camera(
        db, camera_id="cam_dead", org_id="org_test123",
        status="offline", last_seen_minutes_ago=60,
    )

    summary = run_offline_sweep(db)

    assert summary["cameras_flipped"] == 0
    # No new notification — entity was already known-offline.
    assert db.query(Notification).count() == 0


def test_offline_sweep_handles_null_last_seen(db):
    # A status=online row with null last_seen is weird but shouldn't blow up.
    _make_camera(
        db, camera_id="cam_never", org_id="org_test123",
        status="online", last_seen_minutes_ago=None,
    )

    summary = run_offline_sweep(db)

    assert summary["cameras_flipped"] == 0
    cam = db.query(Camera).filter_by(camera_id="cam_never").one()
    # Left alone — we have no evidence the camera ever came online to go offline from.
    assert cam.status == "online"


def test_offline_sweep_flips_stale_node_with_admin_notification(db):
    _make_node(
        db, node_id="node_dead", org_id="org_test123",
        status="online", last_seen_minutes_ago=5,
    )

    summary = run_offline_sweep(db)

    assert summary["nodes_flipped"] == 1
    node = db.query(CameraNode).filter_by(node_id="node_dead").one()
    assert node.status == "offline"

    notifs = db.query(Notification).all()
    assert len(notifs) == 1
    assert notifs[0].kind == "node_offline"
    assert notifs[0].audience == "admin"
    assert notifs[0].node_id == "node_dead"


def test_offline_sweep_debounces_repeat_runs(db):
    _make_camera(
        db, camera_id="cam_flap", org_id="org_test123",
        status="online", last_seen_minutes_ago=5,
    )

    run_offline_sweep(db)

    # Simulate the camera briefly coming back and going stale again
    # within the debounce window.  Reset the row to 'online' + stale
    # and run again — should NOT emit a second notification.
    cam = db.query(Camera).filter_by(camera_id="cam_flap").one()
    cam.status = "online"
    cam.last_seen = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(minutes=5)
    db.commit()

    run_offline_sweep(db)

    notifs = db.query(Notification).filter_by(kind="camera_offline").all()
    assert len(notifs) == 1  # debounced — no duplicate


def test_offline_sweep_mixed_fresh_and_stale(db):
    node = _make_node(
        db, node_id="node_1", org_id="org_test123",
        status="online", last_seen_minutes_ago=5,
    )
    _make_camera(
        db, camera_id="cam_stale", org_id="org_test123",
        status="online", last_seen_minutes_ago=5, node=node,
    )
    _make_camera(
        db, camera_id="cam_fresh", org_id="org_test123",
        status="online", last_seen_minutes_ago=0, node=node,
    )

    summary = run_offline_sweep(db)

    assert summary["nodes_flipped"] == 1
    assert summary["cameras_flipped"] == 1
    assert db.query(Camera).filter_by(camera_id="cam_fresh").one().status == "online"
    assert db.query(Camera).filter_by(camera_id="cam_stale").one().status == "offline"

    # Two notifications: one camera_offline (audience=all) and one node_offline (admin).
    notifs = {n.kind: n for n in db.query(Notification).all()}
    assert notifs["camera_offline"].node_id == "node_1"  # linked to parent node
    assert notifs["node_offline"].audience == "admin"


# ── Preference gate ────────────────────────────────────────────────
# The Settings UI lets operators silence specific notification kinds
# without disabling the underlying event pipeline.  These tests pin the
# gate's behaviour so a refactor can't silently stop respecting the
# toggle (which would be noisy in the worst way — spam returns).

def test_notifications_enabled_defaults_true(db):
    # No Setting row exists → gate returns the per-kind default, which is
    # True for every kind we ship today.  Legacy orgs shouldn't lose
    # notifications just because they never visited the settings page.
    assert notifications_enabled(db, "org_test123", "motion") is True
    assert notifications_enabled(db, "org_test123", "camera_offline") is True
    assert notifications_enabled(db, "org_test123", "node_online") is True


def test_notifications_enabled_honors_false_flag(db):
    Setting.set(db, "org_test123", "motion_notifications", "false")
    assert notifications_enabled(db, "org_test123", "motion") is False
    # Other kinds unaffected — each toggle is scoped to its setting key.
    assert notifications_enabled(db, "org_test123", "camera_offline") is True


def test_notifications_enabled_unknown_kind_defaults_true(db):
    # A future notification kind added without a settings migration
    # should default to delivered, not silently dropped.
    assert notifications_enabled(db, "org_test123", "brand_new_kind") is True


def test_create_notification_respects_motion_toggle(db):
    Setting.set(db, "org_test123", "motion_notifications", "false")

    result = create_notification(
        org_id="org_test123",
        kind="motion",
        title="Motion on cam_abc",
        body="Scene change detected at 42% intensity.",
        audience="all",
        camera_id="cam_abc",
        db=db,
    )

    # Gate returns None when disabled — no row persisted, no broadcast.
    assert result is None
    assert db.query(Notification).filter_by(kind="motion").count() == 0


def test_create_notification_motion_allowed_when_toggle_on(db):
    # Explicitly enable (belt-and-suspenders; default is also True).
    Setting.set(db, "org_test123", "motion_notifications", "true")

    result = create_notification(
        org_id="org_test123",
        kind="motion",
        title="Motion on cam_abc",
        audience="all",
        camera_id="cam_abc",
        db=db,
    )

    assert result is not None
    assert db.query(Notification).filter_by(kind="motion").count() == 1


def test_create_notification_camera_transition_gated(db):
    # Camera online/offline share one toggle.  Turning it off suppresses
    # BOTH directions — the user either wants camera transition alerts or
    # doesn't.
    Setting.set(db, "org_test123", "camera_transition_notifications", "false")

    offline = create_notification(
        org_id="org_test123", kind="camera_offline",
        title="cam_abc offline", audience="all", db=db,
    )
    online = create_notification(
        org_id="org_test123", kind="camera_online",
        title="cam_abc online", audience="all", db=db,
    )
    assert offline is None
    assert online is None
    assert db.query(Notification).count() == 0


# ── Email side-channel ─────────────────────────────────────────────
# Cover the integration between create_notification() and the
# EmailOutbox table.  Resend transport is stubbed via the
# get_recipient_emails patch so no real Clerk lookup happens; the
# tests just verify the rows that land in EmailOutbox.

from app.api import notifications as notifications_mod
from app.models.models import EmailOutbox


@pytest.fixture
def stub_recipients(monkeypatch):
    """Stub recipient lookup so tests don't depend on Clerk.

    Tests set ``stub.return_value`` (list of addresses).  Default is
    one admin so the simple "create notification → enqueue email"
    path works without configuration."""
    class Stub:
        def __init__(self):
            self.return_value: list[str] = ["admin@org.test"]
            self.calls: list[tuple[str, str]] = []

        def __call__(self, org_id, audience):
            self.calls.append((org_id, audience))
            return list(self.return_value)

    stub = Stub()
    monkeypatch.setattr(notifications_mod, "get_recipient_emails", stub)
    return stub


def _enable_email(monkeypatch):
    """Helper — flip the global kill-switch on for tests that need
    to exercise the enqueue path.  Defaults are intentionally OFF
    so accidentally-on tests that don't stub recipients can't
    enqueue real-looking rows."""
    monkeypatch.setattr(notifications_mod.settings, "EMAIL_ENABLED", True)


# Per-kind setting gate ─────────────────────────────────────────────

def test_email_enabled_for_kind_respects_kill_switch(db, monkeypatch):
    """Global kill-switch off → no kind is email-enabled, even if the
    per-kind setting says yes.  Defense in depth: an operator turning
    off email globally must take precedence over any per-org config."""
    monkeypatch.setattr(notifications_mod.settings, "EMAIL_ENABLED", False)
    Setting.set(db, "org_test123", "email_camera_offline", "true")

    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "camera_offline",
    ) is False


def test_email_enabled_for_kind_uses_default_when_unset(db, monkeypatch):
    """No Setting row for the org → fall back to the per-kind default
    in _EMAIL_KIND_TO_SETTING.  Operator-critical events default True
    so a freshly-created org that hasn't visited the settings page
    still gets the alerts."""
    _enable_email(monkeypatch)

    for kind in ("camera_offline", "node_offline", "incident_created"):
        assert notifications_mod.email_enabled_for_kind(
            db, "org_test123", kind,
        ) is True, f"{kind} should default to enabled"

    # disk_critical was REMOVED as a customer-facing kind on
    # 2026-05-04 — see project_notification_channels memory.  The
    # email-enabled gate now refuses it (returns False) the same way
    # it refuses any kind not in _EMAIL_KIND_TO_SETTING.
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "disk_critical",
    ) is False, (
        "disk_critical must NOT email customers — it's a platform-"
        "infrastructure signal routed via Sentry instead"
    )


def test_email_enabled_for_kind_respects_per_org_setting(db, monkeypatch):
    """Per-org Setting=='false' overrides the default and stops emails
    for that kind in that org only."""
    _enable_email(monkeypatch)
    Setting.set(db, "org_test123", "email_camera_offline", "false")

    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "camera_offline",
    ) is False
    # Other kinds for the same org are still enabled.
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "node_offline",
    ) is True


def test_email_enabled_for_kind_unknown_kind_returns_false(db, monkeypatch):
    """Unknown kind → False.  Inverted from the inbox gate (which
    defaults unknown=True for forward-compat) because emailing on
    every new kind by default would be a worse failure mode than
    missing one alert until the per-kind UI catches up."""
    _enable_email(monkeypatch)

    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "some_brand_new_kind",
    ) is False


# Enqueue path ──────────────────────────────────────────────────────

def test_create_notification_enqueues_email_when_enabled(db, monkeypatch, stub_recipients):
    """Happy path: kill-switch on, kind in map, recipients found →
    one EmailOutbox row per recipient, status='pending', linked back
    to the notification id."""
    _enable_email(monkeypatch)
    stub_recipients.return_value = ["admin@org.test", "ops@org.test"]

    notif = create_notification(
        org_id="org_test123",
        kind="camera_offline",
        title="Front Door went offline",
        body="No heartbeat in 90s.",
        audience="all",
        db=db,
    )

    assert notif is not None
    rows = db.query(EmailOutbox).all()
    assert len(rows) == 2
    addrs = sorted(r.recipient_email for r in rows)
    assert addrs == ["admin@org.test", "ops@org.test"]
    for row in rows:
        assert row.status == "pending"
        assert row.kind == "camera_offline"
        assert row.notification_id == notif.id
        assert row.org_id == "org_test123"
        # Subject is template-driven — ``camera_offline.subject.txt.j2``
        # produces "[Sentinel] Camera offline: <name>".
        assert row.subject.startswith("[Sentinel]")
        assert "Front Door" in row.subject
        # Body templates surface notification.body verbatim.
        assert "No heartbeat" in row.body_text
        # HTML wrap brings in the layout's brand header + severity bar.
        assert "<h2" in row.body_html
        assert "Sentinel" in row.body_html


def test_create_notification_does_not_enqueue_when_kill_switch_off(db, monkeypatch, stub_recipients):
    """Kill-switch off → EmailOutbox stays empty even though the kind
    is in the email map.  Inbox notification still persists."""
    monkeypatch.setattr(notifications_mod.settings, "EMAIL_ENABLED", False)

    notif = create_notification(
        org_id="org_test123", kind="camera_offline",
        title="x", body="y", audience="all", db=db,
    )

    assert notif is not None  # inbox row created
    assert db.query(EmailOutbox).count() == 0
    assert len(stub_recipients.calls) == 0  # didn't even look up recipients


def test_create_notification_does_not_enqueue_for_unmapped_kind(db, monkeypatch, stub_recipients):
    """A kind not in _EMAIL_KIND_TO_SETTING (e.g. 'motion') skips the
    email enqueue path entirely.  Motion gets deferred to v1.1; we
    don't want it to silently slip through to the outbox today."""
    _enable_email(monkeypatch)

    notif = create_notification(
        org_id="org_test123", kind="motion",
        title="Motion on cam_abc", body="42% intensity", audience="all", db=db,
    )

    assert notif is not None
    assert db.query(EmailOutbox).count() == 0


def test_create_notification_no_recipients_no_outbox_rows(db, monkeypatch, stub_recipients):
    """get_recipient_emails returns [] (Clerk outage, empty org) →
    enqueue is a no-op.  Notification still committed to inbox."""
    _enable_email(monkeypatch)
    stub_recipients.return_value = []

    notif = create_notification(
        org_id="org_test123", kind="camera_offline",
        title="x", body="y", audience="all", db=db,
    )

    assert notif is not None
    assert db.query(EmailOutbox).count() == 0


def test_create_notification_email_failure_does_not_break_inbox(db, monkeypatch, stub_recipients):
    """If recipient lookup raises, the inbox notification still gets
    persisted.  Email is best-effort; the bell-icon panel must never
    go silent because Resend or Clerk is having a bad day."""
    _enable_email(monkeypatch)

    def boom(*a, **kw):
        raise RuntimeError("Clerk down")
    monkeypatch.setattr(notifications_mod, "get_recipient_emails", boom)

    notif = create_notification(
        org_id="org_test123", kind="camera_offline",
        title="x", body="y", audience="all", db=db,
    )

    assert notif is not None
    assert db.query(Notification).count() == 1
    assert db.query(EmailOutbox).count() == 0


def test_create_notification_passes_audience_to_recipient_lookup(db, monkeypatch, stub_recipients):
    """audience='admin' → recipient lookup gets 'admin'.  This is the
    bridge between the existing inbox audience field and the email
    recipient filter — the same notification that hides from non-admin
    users in the inbox should also only email admins."""
    _enable_email(monkeypatch)

    create_notification(
        org_id="org_test123", kind="node_offline",
        title="Node down", audience="admin", db=db,
    )

    assert stub_recipients.calls == [("org_test123", "admin")]


def test_email_content_escapes_html_in_title_and_body(db, monkeypatch, stub_recipients):
    """A camera name that happens to contain `<script>` must not
    break out of the email's HTML structure.  Operator-controlled
    strings, but defense in depth.

    Plain-text templates do NOT escape (otherwise the text body
    would render ``&amp;`` instead of ``&``); the .html.j2 templates
    do (autoescape selected per-template by extension)."""
    _enable_email(monkeypatch)

    create_notification(
        org_id="org_test123", kind="camera_offline",
        title="<script>alert(1)</script>",
        body="Body with <b>html</b> & ampersand",
        audience="all", db=db,
    )

    row = db.query(EmailOutbox).first()
    # Raw script tag must not appear anywhere in the HTML body.
    assert "<script>alert(1)</script>" not in row.body_html
    # Escaped form must.
    assert "&lt;script&gt;alert(1)&lt;/script&gt;" in row.body_html
    assert "&amp;" in row.body_html
    # Plain text body keeps original characters — not HTML, no XSS
    # surface.  The .txt.j2 templates intentionally don't autoescape
    # so the user reads "&" not "&amp;".
    assert "Body with <b>html</b>" in row.body_text
    assert "& ampersand" in row.body_text


# ── Email preferences endpoints ────────────────────────────────────

def test_get_email_preferences_returns_defaults(admin_client):
    """First load: no Setting rows → response uses _EMAIL_KIND_TO_SETTING
    defaults so the toggle UI shows the right initial state."""
    resp = admin_client.get("/api/notifications/email/preferences")

    assert resp.status_code == 200
    data = resp.json()
    assert "email_globally_enabled" in data
    prefs = data["preferences"]
    # Six setting keys default to enabled, ONE (motion) defaults OFF.
    # Some keys gate multiple notification kinds:
    #   - ``email_camera_offline`` → camera_offline + camera_online
    #   - ``email_node_offline``   → node_offline + node_online
    #   - ``email_mcp_key_audit``  → mcp_key_created + mcp_key_revoked
    #   - ``email_member_audit``   → member_added + member_role_changed
    #                                + member_removed
    #   - ``email_motion``         → motion + motion_digest
    # See _EMAIL_KIND_TO_SETTING for the full kind→setting fan-out.
    assert prefs["email_camera_offline"] is True
    assert prefs["email_node_offline"] is True
    assert prefs["email_incident_created"] is True
    assert prefs["email_mcp_key_audit"] is True
    assert prefs["email_cloudnode_disk_low"] is True
    assert prefs["email_member_audit"] is True
    # Motion is the ONLY email kind that defaults OFF — see the
    # comment block above _EMAIL_KIND_TO_SETTING for rationale
    # (per-org volume variance + sender-reputation protection).
    assert prefs["email_motion"] is False
    # disk_critical was REMOVED from this map on 2026-05-04 — it's
    # platform-infrastructure state, routed via Sentry instead.
    assert "email_disk_critical" not in prefs


def test_recovery_kinds_share_setting_with_offline_kinds(db, monkeypatch):
    """Camera/Node ONLINE emails are gated by the SAME setting key
    as their OFFLINE counterpart.  One toggle controls the whole
    transition pair so users don't have to opt in twice for "I want
    to know about my camera's connection state."  Pin the routing
    so a future refactor that splits the keys apart surfaces here."""
    _enable_email(monkeypatch)

    # Default: both directions enabled.
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "camera_offline",
    ) is True
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "camera_online",
    ) is True

    # Flip the offline setting to false → recovery emails ALSO stop.
    Setting.set(db, "org_test123", "email_camera_offline", "false")
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "camera_offline",
    ) is False
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "camera_online",
    ) is False

    # Same pairing for nodes.
    Setting.set(db, "org_test123", "email_node_offline", "false")
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "node_offline",
    ) is False
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "node_online",
    ) is False


def test_mcp_key_kinds_share_audit_setting(db, monkeypatch):
    """MCP key created/revoked share email_mcp_key_audit — one
    toggle for the whole security-audit pair."""
    _enable_email(monkeypatch)

    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "mcp_key_created",
    ) is True
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "mcp_key_revoked",
    ) is True

    Setting.set(db, "org_test123", "email_mcp_key_audit", "false")
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "mcp_key_created",
    ) is False
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "mcp_key_revoked",
    ) is False


def test_member_kinds_share_audit_setting(db, monkeypatch):
    """All three member lifecycle kinds (added, role_changed,
    removed) share email_member_audit.  Admins consistently want
    the whole audit trail or none of it — there's no "I care about
    additions but not removals" use case."""
    _enable_email(monkeypatch)

    for kind in ("member_added", "member_role_changed", "member_removed"):
        assert notifications_mod.email_enabled_for_kind(
            db, "org_test123", kind,
        ) is True, f"{kind} should default to enabled"

    Setting.set(db, "org_test123", "email_member_audit", "false")
    for kind in ("member_added", "member_role_changed", "member_removed"):
        assert notifications_mod.email_enabled_for_kind(
            db, "org_test123", kind,
        ) is False, f"{kind} should be disabled when email_member_audit is off"


def test_cloudnode_disk_low_routes_to_dedicated_setting(db, monkeypatch):
    """cloudnode_disk_low is a single-event setting (no pair).
    Verify it gates correctly and isn't accidentally bound to
    another kind's setting."""
    _enable_email(monkeypatch)

    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "cloudnode_disk_low",
    ) is True

    Setting.set(db, "org_test123", "email_cloudnode_disk_low", "false")
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "cloudnode_disk_low",
    ) is False

    # Other settings are unaffected.
    assert notifications_mod.email_enabled_for_kind(
        db, "org_test123", "camera_offline",
    ) is True


def test_get_email_preferences_reflects_overrides(admin_client, db):
    """Setting rows override the defaults."""
    Setting.set(db, "org_test123", "email_camera_offline", "false")
    Setting.set(db, "org_test123", "email_node_offline", "false")

    resp = admin_client.get("/api/notifications/email/preferences")
    prefs = resp.json()["preferences"]

    assert prefs["email_camera_offline"] is False
    assert prefs["email_node_offline"] is False
    # Untouched ones stay at default.
    assert prefs["email_incident_created"] is True


def test_get_email_preferences_surfaces_global_kill_switch(admin_client, monkeypatch):
    """email_globally_enabled mirrors settings.EMAIL_ENABLED so the
    frontend can render "emails are off platform-wide" copy alongside
    individually-still-enabled toggles."""
    from app.core.config import settings as app_settings
    monkeypatch.setattr(app_settings, "EMAIL_ENABLED", False)

    resp = admin_client.get("/api/notifications/email/preferences")
    assert resp.json()["email_globally_enabled"] is False

    monkeypatch.setattr(app_settings, "EMAIL_ENABLED", True)
    resp = admin_client.get("/api/notifications/email/preferences")
    assert resp.json()["email_globally_enabled"] is True


def test_post_email_preferences_updates_settings(admin_client, db):
    """POST a partial pref dict → only those keys flip."""
    resp = admin_client.post(
        "/api/notifications/email/preferences",
        json={"email_camera_offline": False, "email_incident_created": False},
    )

    assert resp.status_code == 200
    data = resp.json()
    assert data["preferences"]["email_camera_offline"] is False
    assert data["preferences"]["email_incident_created"] is False
    # Unspecified keys keep their default.
    assert data["preferences"]["email_node_offline"] is True
    # Confirm via direct Setting query.
    assert Setting.get(db, "org_test123", "email_camera_offline") == "false"
    assert Setting.get(db, "org_test123", "email_incident_created") == "false"


def test_post_email_preferences_partial_update_does_not_clobber(admin_client, db):
    """Partial update: omitted fields stay untouched.  Tests can't
    rely on field default == None vs False; pydantic exclude_unset
    is what makes this work."""
    Setting.set(db, "org_test123", "email_node_offline", "false")

    resp = admin_client.post(
        "/api/notifications/email/preferences",
        json={"email_camera_offline": False},
    )

    assert resp.status_code == 200
    # node_offline should STILL be false from the prior set.
    assert Setting.get(db, "org_test123", "email_node_offline") == "false"


def test_post_email_preferences_writes_audit(admin_client, db):
    """Admin pref changes audited — so a "who turned off our alerts?"
    investigation can find the answer in the audit log."""
    from app.models.models import AuditLog

    admin_client.post(
        "/api/notifications/email/preferences",
        json={"email_camera_offline": False},
    )

    audit = (
        db.query(AuditLog)
        .filter_by(event="email_prefs_updated")
        .first()
    )
    assert audit is not None
    assert audit.org_id == "org_test123"
    import json
    parsed = json.loads(audit.details or "{}")
    assert "email_camera_offline=False" in parsed.get("changes", [])


def test_post_email_preferences_requires_admin(viewer_client):
    """Non-admins can READ prefs but not change them."""
    # Viewer can read.
    r1 = viewer_client.get("/api/notifications/email/preferences")
    assert r1.status_code == 200

    # But not write.
    r2 = viewer_client.post(
        "/api/notifications/email/preferences",
        json={"email_camera_offline": False},
    )
    assert r2.status_code == 403


# ── Unsubscribe endpoint ────────────────────────────────────────────

def test_unsubscribe_endpoint_suppresses_recipient_address(unauthenticated_client, db):
    """Click the link → the RECIPIENT ADDRESS is suppressed (the send
    worker skips all future emails to it).  The org-wide kind toggle
    must NOT flip: tokens outlive membership, and the v1 behaviour let
    any past recipient disable a whole org's security-alert emails."""
    from app.core.email_unsubscribe import make_token
    from app.models.models import EmailSuppression

    token = make_token("org_test123", "camera_offline", "clicker@example.com")

    resp = unauthenticated_client.get(
        f"/api/notifications/email/unsubscribe?t={token}"
    )

    assert resp.status_code == 200
    assert "text/html" in resp.headers.get("content-type", "")
    row = (
        db.query(EmailSuppression)
        .filter_by(address="clicker@example.com")
        .first()
    )
    assert row is not None
    assert row.reason == "unsubscribe"
    assert row.source == "unsubscribe_link"
    # Org-wide toggle untouched — other members keep their emails.
    assert Setting.get(db, "org_test123", "email_camera_offline") is None


def test_unsubscribe_endpoint_writes_audit(unauthenticated_client, db):
    """Anonymous unsubscribes are audited too — useful for triaging
    "all my users are unsubscribing" support tickets later."""
    from app.core.email_unsubscribe import make_token
    from app.models.models import AuditLog

    token = make_token("org_test123", "node_offline", "auditee@example.com")
    unauthenticated_client.get(
        f"/api/notifications/email/unsubscribe?t={token}"
    )

    audit = (
        db.query(AuditLog)
        .filter_by(event="email_unsubscribed")
        .first()
    )
    assert audit is not None
    assert audit.org_id == "org_test123"
    # details is a JSON-serialised dict — parse it rather than substring-
    # matching so a key rename surfaces here as a clear failure.
    import json
    parsed = json.loads(audit.details or "{}")
    assert parsed["kind"] == "node_offline"
    assert parsed["via_link"] is True
    # Address is MASKED in the audit trail — not an email directory.
    assert parsed["address"] == "a***@example.com"
    assert "auditee@example.com" not in (audit.details or "")


def test_unsubscribe_endpoint_renders_friendly_html(unauthenticated_client):
    """End-user-facing — HTML page, not a JSON response.  Pin a
    couple of recognisable phrases so a copy refactor surfaces here
    rather than silently producing a worse page."""
    from app.core.email_unsubscribe import make_token

    token = make_token("org_test123", "camera_offline", "reader@example.com")
    resp = unauthenticated_client.get(
        f"/api/notifications/email/unsubscribe?t={token}"
    )

    body = resp.text
    assert "unsubscribed" in body.lower()
    # Pretty-prints "camera_offline" as "camera offline" for the user.
    assert "camera offline" in body.lower()


def test_unsubscribe_endpoint_rejects_invalid_token(unauthenticated_client):
    """Bad token → friendly error page, NOT a 500.  The user clicked
    a link expecting a result; show them one."""
    resp = unauthenticated_client.get(
        "/api/notifications/email/unsubscribe?t=this-is-not-a-jwt"
    )

    assert resp.status_code == 400
    body = resp.text.lower()
    assert "not recognised" in body or "invalid" in body or "expired" in body


def test_unsubscribe_endpoint_idempotent(unauthenticated_client, db):
    """Clicking the same link twice produces the same outcome — no
    error on the second click.  Email clients sometimes pre-fetch
    links for security scans, so a non-idempotent unsubscribe would
    flip the setting AND then 500 when the user actually clicks."""
    from app.core.email_unsubscribe import make_token
    from app.models.models import EmailSuppression

    token = make_token("org_test123", "camera_offline", "twice@example.com")

    r1 = unauthenticated_client.get(
        f"/api/notifications/email/unsubscribe?t={token}"
    )
    r2 = unauthenticated_client.get(
        f"/api/notifications/email/unsubscribe?t={token}"
    )

    assert r1.status_code == 200
    assert r2.status_code == 200
    # Exactly one suppression row — the second click is a no-op, not
    # an IntegrityError (address is UNIQUE).
    rows = (
        db.query(EmailSuppression)
        .filter_by(address="twice@example.com")
        .all()
    )
    assert len(rows) == 1


def test_unsubscribe_endpoint_html_escapes_kind(unauthenticated_client):
    """The unsubscribe HTML page builds via str.format() (not Jinja2,
    so autoescape doesn't apply).  Even though the kind value flows
    from a verified JWT — making this practically unreachable without
    the Clerk secret — anything substituted into the page must be
    HTML-escaped as defense in depth.

    This test forges a token by signing a kind containing HTML; it
    succeeds only because the test process knows the secret.  In
    production an attacker would need CLERK_SECRET_KEY to reach
    this code path, at which point they have everything anyway —
    but the escape protects against future paths that might surface
    less-trusted input here."""
    # Use an unknown kind so we hit the "not in _EMAIL_KIND_TO_SETTING"
    # path that interpolates the raw kind verbatim (the known-kind
    # path uses a pretty-printed version which would obscure the
    # escape).
    import time as _time

    import jwt

    from app.core.email_unsubscribe import _get_secret

    bad_kind = "<script>alert('xss')</script>"
    token = jwt.encode(
        {
            "org_id": "org_x",
            "kind": bad_kind,
            "rcpt": "x@example.com",
            "exp": int(_time.time()) + 3600,
            "sub": "email-unsubscribe",
        },
        _get_secret(),
        algorithm="HS256",
    )

    resp = unauthenticated_client.get(
        f"/api/notifications/email/unsubscribe?t={token}"
    )

    body = resp.text
    # Escaped form must be present.
    assert "&lt;script&gt;" in body
    # Raw form must NOT appear in the rendered HTML.
    assert "<script>alert" not in body
