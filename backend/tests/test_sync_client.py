"""Tests for app/core/sync_client.py — the self-host cloud data-sync tier.

Covers: the is_sync_enabled entitlement gate (separate from, and
layered on top of, is_sentinel_licensed), the incremental-push cursor
mechanics, deletion-reconciliation for identity tables only, and the
fail-open "one table's failure doesn't block the others" contract.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta

import httpx
import pytest

from app.core import sync_client
from app.core.config import settings
from app.models.models import Camera, CameraNode, MotionEvent, Setting

_UNSET = object()


class _FakeResponse:
    def __init__(self, status_code: int = 200, json_data=_UNSET):
        self.status_code = status_code
        self._json_data = {} if json_data is _UNSET else json_data

    def raise_for_status(self):
        if self.status_code >= 400:
            raise httpx.HTTPStatusError("error", request=None, response=self)

    def json(self):
        return self._json_data


class _RecordingAsyncClient:
    """Records every POST's (url, json) and always returns 200 — lets
    tests inspect exactly what was pushed for each table across a
    multi-table sync cycle, unlike license_client's tests which only
    ever need one mocked call at a time."""

    def __init__(self, calls: list, status_code: int = 200, exc: Exception | None = None):
        self._calls = calls
        self._status_code = status_code
        self._exc = exc

    async def __aenter__(self):
        return self

    async def __aexit__(self, *args):
        return False

    async def post(self, url, headers=None, json=None):
        self._calls.append((url, json))
        if self._exc is not None:
            raise self._exc
        return _FakeResponse(self._status_code, {"accepted": True})


def _mock_http(monkeypatch, calls: list, *, status_code: int = 200, exc: Exception | None = None):
    def factory(*args, **kwargs):
        return _RecordingAsyncClient(calls, status_code=status_code, exc=exc)

    monkeypatch.setattr(sync_client.httpx, "AsyncClient", factory)


@pytest.fixture(autouse=True)
def _local_self_host(monkeypatch):
    monkeypatch.setattr(settings, "AUTH_PROVIDER", "local")
    monkeypatch.setattr(settings, "SENTINEL_LICENSE_KEY", "slk_test_key")
    monkeypatch.setattr(settings, "SENTINEL_SYNC_SERVICE_URL", "https://sync.test")
    monkeypatch.setattr(settings, "LOCAL_ORG_ID", "self-host-test-org")


def _grant_sync_entitlement(db):
    """Puts the Setting cache into the exact state a reachable+valid+
    sync_enabled check-in would leave it in — sync_client never talks
    to License-Service itself, it only reads what license_client's
    check-in loop already cached."""
    now_iso = datetime.now(tz=UTC).isoformat()
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_valid", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at", now_iso, commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_data_sync_enabled", "true")


# ── is_sync_enabled gate ──────────────────────────────────────────────


def test_hosted_clerk_orgs_are_never_sync_enabled(monkeypatch, db):
    monkeypatch.setattr(settings, "AUTH_PROVIDER", "clerk")
    _grant_sync_entitlement(db)
    assert sync_client.is_sync_enabled(db) is False


def test_no_key_configured_is_never_sync_enabled(monkeypatch, db):
    monkeypatch.setattr(settings, "SENTINEL_LICENSE_KEY", "")
    _grant_sync_entitlement(db)
    assert sync_client.is_sync_enabled(db) is False


def test_sync_disabled_by_default_even_with_valid_license(db):
    # Valid license, but sentinel_data_sync_enabled was never set —
    # sync is a separate opt-in entitlement, not implied by AI licensing.
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_valid", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_ok_at", datetime.now(tz=UTC).isoformat())
    assert sync_client.is_sync_enabled(db) is False


def test_invalid_license_blocks_sync_even_if_flag_was_previously_true(db):
    # Regression: a stale cached "sync_enabled=true" from before
    # revocation must not outlive the license's own validity.
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_valid", "false", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_license_last_check_reachable", "true", commit=False)
    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_data_sync_enabled", "true")
    assert sync_client.is_sync_enabled(db) is False


def test_valid_license_plus_flag_enables_sync(db):
    _grant_sync_entitlement(db)
    assert sync_client.is_sync_enabled(db) is True


# ── check_in_with_license_service populates the sync flag ─────────────


async def test_checkin_caches_sync_enabled_from_response(monkeypatch, db):
    from app.core import license_client

    class _Resp:
        status_code = 200

        def raise_for_status(self):
            return None

        def json(self):
            return {
                "valid": True, "reason": None, "tier": "self_host_standard",
                "status": "active", "monthly_run_cap": 500, "sync_enabled": True,
                "renews_at": None, "server_time": "2026-01-01T00:00:00Z",
            }

    class _Client:
        async def __aenter__(self):
            return self

        async def __aexit__(self, *a):
            return False

        async def post(self, *a, **kw):
            return _Resp()

    monkeypatch.setattr(license_client.httpx, "AsyncClient", lambda *a, **kw: _Client())
    await license_client.check_in_with_license_service(db)

    assert sync_client.is_sync_enabled(db) is True


async def test_checkin_clears_sync_enabled_when_license_invalid(monkeypatch, db):
    from app.core import license_client

    Setting.set(db, settings.LOCAL_ORG_ID, "sentinel_data_sync_enabled", "true")

    class _Resp:
        status_code = 200

        def raise_for_status(self):
            return None

        def json(self):
            return {"valid": False, "reason": "revoked", "server_time": "x"}

    class _Client:
        async def __aenter__(self):
            return self

        async def __aexit__(self, *a):
            return False

        async def post(self, *a, **kw):
            return _Resp()

    monkeypatch.setattr(license_client.httpx, "AsyncClient", lambda *a, **kw: _Client())
    await license_client.check_in_with_license_service(db)

    assert Setting.get(db, settings.LOCAL_ORG_ID, "sentinel_data_sync_enabled") == "false"


# ── push_pending_changes: gating ───────────────────────────────────────


async def test_push_is_a_noop_when_not_sync_enabled(db):
    # No fake transport installed — if this tried to make a real HTTP
    # call it would raise. Reaching the end proves it short-circuited.
    await sync_client.push_pending_changes(db)


# ── push_pending_changes: incremental push + cursor advancement ───────


def _make_camera(db, camera_id="cam-1", org_id="self-host-test-org"):
    node = CameraNode(node_id="node-1", org_id=org_id, api_key_hash="x", name="Node 1")
    db.add(node)
    db.commit()
    db.refresh(node)
    cam = Camera(camera_id=camera_id, org_id=org_id, node_id=node.id, name="Cam 1")
    db.add(cam)
    db.commit()
    db.refresh(cam)
    return cam


async def test_push_sends_rows_and_advances_cursor(monkeypatch, db):
    _grant_sync_entitlement(db)
    _make_camera(db)
    calls: list = []
    _mock_http(monkeypatch, calls)

    await sync_client.push_pending_changes(db)

    camera_calls = [c for c in calls if c[1]["table"] == "cameras"]
    assert len(camera_calls) == 1
    body = camera_calls[0][1]
    assert len(body["rows"]) == 1
    assert body["rows"][0]["data"]["name"] == "Cam 1"
    # api_key_hash must never leave the box — CameraNode.to_dict()
    # already omits it, which is exactly why sync_client reuses to_dict()
    # rather than hand-serializing columns.
    node_calls = [c for c in calls if c[1]["table"] == "camera_nodes"]
    assert "api_key_hash" not in node_calls[0][1]["rows"][0]["data"]

    cursor = Setting.get(db, settings.LOCAL_ORG_ID, "sentinel_sync_cursor_cameras")
    assert cursor is not None


async def test_payload_carries_raw_columns_so_a_restore_can_rebuild_the_row(monkeypatch, db):
    """The property the whole mirror depends on: what lands in the cloud
    must be enough to reconstruct the local row.

    Regression against the original design, which reused each model's
    to_dict(). That silently dropped 11 of Camera's 21 columns — the
    recording-policy fields nested under a "recording_policy" key
    instead of being flat, and the codec columns disappeared entirely —
    so a restore would have looked like it worked while quietly losing
    configuration.
    """
    _grant_sync_entitlement(db)
    cam = _make_camera(db)
    cam.continuous_24_7 = True
    cam.scheduled_start = "22:00"
    cam.video_codec = "avc1.42e01e"
    db.add(cam)
    db.commit()

    calls: list = []
    _mock_http(monkeypatch, calls)
    await sync_client.push_pending_changes(db)

    data = [c for c in calls if c[1]["table"] == "cameras"][0][1]["rows"][0]["data"]

    # Every column present and flat — not nested under a display key.
    column_names = {c.name for c in Camera.__table__.columns}
    assert column_names <= set(data), f"missing columns: {sorted(column_names - set(data))}"
    assert data["continuous_24_7"] is True
    assert data["scheduled_start"] == "22:00"
    assert data["video_codec"] == "avc1.42e01e"
    assert "recording_policy" not in data, "display-shaped nesting must not reappear"


async def test_payload_is_json_serialisable(monkeypatch, db):
    # Datetimes have no JSON representation; if one ever reaches the
    # wire unconverted the whole sync cycle dies on encode.
    import json

    _grant_sync_entitlement(db)
    _make_camera(db)
    calls: list = []
    _mock_http(monkeypatch, calls)
    await sync_client.push_pending_changes(db)

    for _url, body in calls:
        json.dumps(body)  # must not raise
    data = [c for c in calls if c[1]["table"] == "cameras"][0][1]["rows"][0]["data"]
    assert isinstance(data["created_at"], str), "datetime should serialise to ISO string"


async def test_denylisted_columns_never_leave_the_install(monkeypatch, db):
    """api_key_hash authenticates a node to this Command Center. It is
    useless to a restore (a restored node re-registers for a fresh key)
    and dangerous in a cloud mirror, so it must be absent even though
    the payload is now raw columns rather than a curated to_dict()."""
    _grant_sync_entitlement(db)
    _make_camera(db)  # also creates a CameraNode with an api_key_hash
    calls: list = []
    _mock_http(monkeypatch, calls)
    await sync_client.push_pending_changes(db)

    node_data = [c for c in calls if c[1]["table"] == "camera_nodes"][0][1]["rows"][0]["data"]
    assert "api_key_hash" not in node_data
    # ...while the rest of the row still rides along, so a restore can
    # rebuild everything except the credential.
    assert node_data["node_id"] == "node-1"
    assert "storage_used_bytes" in node_data


async def test_incident_evidence_blob_is_excluded_without_loading_it(monkeypatch, db):
    """IncidentEvidence.data is deferred() precisely so listing rows
    doesn't pull tens of MB per row into RAM. The denylist check has to
    happen before the attribute is read, or iterating columns would
    re-create exactly the bulk-materialisation the deferral prevents."""
    from sqlalchemy import inspect as sa_inspect

    from app.models.models import Incident, IncidentEvidence

    _grant_sync_entitlement(db)
    incident = Incident(
        org_id=settings.LOCAL_ORG_ID, title="t", summary="s", created_by="test"
    )
    db.add(incident)
    db.commit()
    db.refresh(incident)
    ev = IncidentEvidence(
        incident_id=incident.id, kind="snapshot", data=b"x" * 2048, data_mime="image/jpeg"
    )
    db.add(ev)
    db.commit()

    calls: list = []
    _mock_http(monkeypatch, calls)
    await sync_client.push_pending_changes(db)

    ev_rows = [c for c in calls if c[1]["table"] == "incident_evidence"]
    data = ev_rows[0][1]["rows"][0]["data"]
    assert "data" not in data, "blob must never be synced"
    assert data["data_mime"] == "image/jpeg", "metadata around the blob still syncs"

    # The blob stayed unloaded: a fresh instance must still report
    # `data` as an unloaded deferred attribute.
    fresh = db.query(IncidentEvidence).filter_by(id=ev.id).first()
    assert "data" in sa_inspect(fresh).unloaded


async def test_second_push_with_no_new_rows_sends_nothing_for_that_table(monkeypatch, db):
    _grant_sync_entitlement(db)
    _make_camera(db)
    calls: list = []
    _mock_http(monkeypatch, calls)
    await sync_client.push_pending_changes(db)

    calls.clear()
    await sync_client.push_pending_changes(db)

    camera_calls = [c for c in calls if c[1]["table"] == "cameras"]
    assert camera_calls == []


async def test_updated_row_is_pushed_again_after_cursor_advances(monkeypatch, db):
    _grant_sync_entitlement(db)
    cam = _make_camera(db)
    calls: list = []
    _mock_http(monkeypatch, calls)
    await sync_client.push_pending_changes(db)

    cam.name = "Renamed"
    db.add(cam)
    db.commit()
    calls.clear()
    await sync_client.push_pending_changes(db)

    camera_calls = [c for c in calls if c[1]["table"] == "cameras"]
    assert len(camera_calls) == 1
    assert camera_calls[0][1]["rows"][0]["data"]["name"] == "Renamed"


# ── Deletion reconciliation: identity tables only ──────────────────────


async def test_identity_tables_include_known_ids_for_deletion_reconciliation(monkeypatch, db):
    _grant_sync_entitlement(db)
    _make_camera(db)
    calls: list = []
    _mock_http(monkeypatch, calls)

    await sync_client.push_pending_changes(db)

    camera_calls = [c for c in calls if c[1]["table"] == "cameras"]
    assert "known_ids" in camera_calls[0][1]


async def test_log_tables_never_include_known_ids(monkeypatch, db):
    """Regression: motion_events (and other high-volume/log tables) must
    never propagate deletions — local retention deletes old rows on
    purpose, and the cloud copy is meant to outlive that."""
    _grant_sync_entitlement(db)
    event = MotionEvent(org_id=settings.LOCAL_ORG_ID, camera_id="cam-1", node_id="node-1", score=50)
    db.add(event)
    db.commit()
    calls: list = []
    _mock_http(monkeypatch, calls)

    await sync_client.push_pending_changes(db)

    motion_calls = [c for c in calls if c[1]["table"] == "motion_events"]
    assert len(motion_calls) == 1
    assert "known_ids" not in motion_calls[0][1]


# ── Fail-open: one table's failure doesn't block the others ────────────


async def test_one_table_failure_does_not_block_others(monkeypatch, db):
    _grant_sync_entitlement(db)
    _make_camera(db)
    event = MotionEvent(org_id=settings.LOCAL_ORG_ID, camera_id="cam-1", node_id="node-1", score=50)
    db.add(event)
    db.commit()

    real_push_table = sync_client._push_table
    call_count = {"n": 0}

    async def _flaky_push_table(client, db_, spec):
        call_count["n"] += 1
        if spec.model.__tablename__ == "cameras":
            raise httpx.ConnectError("boom")
        return await real_push_table(client, db_, spec)

    monkeypatch.setattr(sync_client, "_push_table", _flaky_push_table)
    calls: list = []
    _mock_http(monkeypatch, calls)

    await sync_client.push_pending_changes(db)  # must not raise

    # cameras raised, but every other table (including motion_events)
    # still got its chance to push.
    assert any(c[1]["table"] == "motion_events" for c in calls)
    assert call_count["n"] == len(sync_client._table_specs())


async def test_push_cycle_never_raises_on_total_transport_failure(monkeypatch, db):
    _grant_sync_entitlement(db)
    _make_camera(db)
    _mock_http(monkeypatch, [], exc=httpx.ConnectError("connection refused"))

    await sync_client.push_pending_changes(db)  # must not raise

    # Cursor must not have advanced — the push never actually succeeded.
    assert Setting.get(db, settings.LOCAL_ORG_ID, "sentinel_sync_cursor_cameras") is None
