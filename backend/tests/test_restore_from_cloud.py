"""Tests for scripts/restore_from_cloud.py — the read half of the
cloud mirror.

The HTTP paging is exercised against the real service in
Sentinel-Sync-Service's own suite; what matters here is the part that
touches the operator's database, where the failure modes are
destructive: clobbering rows that were still good, duplicating on a
re-run, or writing a row that violates a NOT NULL the mirror never
carried.
"""

from __future__ import annotations

import importlib.util
from datetime import datetime
from pathlib import Path

import pytest

from app.models.models import Camera, CameraNode

# Loaded by path: scripts/ isn't a package, and the module is a CLI
# rather than part of the app tree.
_SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "restore_from_cloud.py"
_spec = importlib.util.spec_from_file_location("restore_from_cloud", _SCRIPT)
restore = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(restore)


# ── _coerce: mirror payload -> column values ─────────────────────────


def test_coerce_parses_iso_strings_back_into_datetimes():
    # JSON has no date type, so sync_client sends ISO strings; they have
    # to become datetimes again or SQLAlchemy writes garbage into a
    # DateTime column.
    values = restore._coerce(Camera, {"camera_id": "c1", "created_at": "2026-01-02T03:04:05"})
    assert values["created_at"] == datetime(2026, 1, 2, 3, 4, 5)


def test_coerce_survives_an_unparseable_timestamp():
    # One bad timestamp shouldn't cost the whole row.
    values = restore._coerce(Camera, {"camera_id": "c1", "created_at": "not-a-date"})
    assert values["created_at"] is None
    assert values["camera_id"] == "c1"


def test_coerce_drops_keys_that_are_not_columns():
    # A mirror written by a different Command Center build may carry
    # fields this one doesn't have; that must not raise.
    values = restore._coerce(Camera, {"camera_id": "c1", "invented_field": "x"})
    assert "invented_field" not in values
    assert values["camera_id"] == "c1"


def test_coerce_supplies_a_placeholder_for_the_unmirrored_credential():
    """camera_nodes.api_key_hash is NOT NULL but deliberately never
    mirrored, so a restore has nothing to put there and the insert
    would fail. The placeholder must not be valid hex, so it can never
    collide with a real SHA-256 hash — a restored node fails auth until
    it re-registers, which is the correct outcome."""
    values = restore._coerce(CameraNode, {"node_id": "n1", "name": "Pi"})
    placeholder = values["api_key_hash"]
    assert placeholder == restore._UNRESTORABLE_CREDENTIAL
    with pytest.raises(ValueError):
        int(placeholder, 16)


# ── restore_table: what actually touches the database ────────────────


def _mirror_row(row_id: int, **data):
    return {"id": str(row_id), "data": {"id": row_id, **data}}


def test_restores_rows_into_an_empty_table(db):
    rows = [_mirror_row(1, camera_id="cam-1", org_id="o", name="Front")]
    written, skipped, failed = restore.restore_table(db, Camera, rows, overwrite=False, dry_run=False)

    assert (written, skipped, failed) == (1, 0, 0)
    assert db.query(Camera).filter_by(id=1).one().name == "Front"


def test_default_run_never_clobbers_existing_rows(db):
    """The property that matters most: someone runs this mid-incident,
    and a recovery tool must not be able to make things worse than it
    found them."""
    db.add(Camera(id=1, camera_id="cam-1", org_id="o", name="Local edit"))
    db.commit()

    rows = [_mirror_row(1, camera_id="cam-1", org_id="o", name="Mirrored")]
    written, skipped, failed = restore.restore_table(db, Camera, rows, overwrite=False, dry_run=False)

    assert (written, skipped, failed) == (0, 1, 0)
    assert db.query(Camera).filter_by(id=1).one().name == "Local edit"


def test_overwrite_replaces_without_duplicating(db):
    db.add(Camera(id=1, camera_id="cam-1", org_id="o", name="Local edit"))
    db.commit()

    rows = [_mirror_row(1, camera_id="cam-1", org_id="o", name="Mirrored")]
    written, skipped, failed = restore.restore_table(db, Camera, rows, overwrite=True, dry_run=False)

    assert (written, skipped, failed) == (1, 0, 0)
    assert db.query(Camera).filter_by(id=1).one().name == "Mirrored"
    assert db.query(Camera).count() == 1, "overwrite must update in place, not insert a second row"


def test_rerunning_a_restore_is_idempotent(db):
    rows = [_mirror_row(1, camera_id="cam-1", org_id="o", name="Front")]
    restore.restore_table(db, Camera, rows, overwrite=False, dry_run=False)
    written, skipped, failed = restore.restore_table(db, Camera, rows, overwrite=False, dry_run=False)

    assert (written, skipped, failed) == (0, 1, 0)
    assert db.query(Camera).count() == 1


def test_dry_run_reports_without_writing(db):
    rows = [_mirror_row(1, camera_id="cam-1", org_id="o", name="Front")]
    written, skipped, failed = restore.restore_table(db, Camera, rows, overwrite=False, dry_run=True)

    assert (written, skipped, failed) == (1, 0, 0)
    assert db.query(Camera).count() == 0, "dry run must not write"


def test_rows_without_a_primary_key_are_skipped_not_crashed_on(db):
    rows = [
        {"id": "1", "data": {"camera_id": "no-pk"}},  # no id -> can't be placed
        _mirror_row(2, camera_id="cam-2", org_id="o", name="Good"),
    ]
    written, skipped, failed = restore.restore_table(db, Camera, rows, overwrite=False, dry_run=False)

    assert (written, skipped, failed) == (1, 1, 0)
    assert db.query(Camera).count() == 1


def test_one_unwritable_row_does_not_abort_the_whole_restore(db):
    """Found by this test failing against the first implementation: a
    row the database rejects used to raise straight out of the loop,
    rolling back the transaction and losing every row — so a restore of
    10,000 records could yield nothing because one was malformed. That
    is the worst possible behaviour for a tool someone reaches for
    after losing a disk.

    Now each write gets its own SAVEPOINT: the bad row is reported and
    stepped over, and rows after it still land.
    """
    rows = [
        _mirror_row(1, camera_id="cam-1", org_id="o", name="Before"),
        # name is NOT NULL — this row cannot be inserted.
        _mirror_row(2, camera_id="cam-2", org_id="o"),
        _mirror_row(3, camera_id="cam-3", org_id="o", name="After"),
    ]
    written, skipped, failed = restore.restore_table(db, Camera, rows, overwrite=False, dry_run=False)

    assert (written, skipped, failed) == (2, 0, 1)
    names = {c.name for c in db.query(Camera).all()}
    assert names == {"Before", "After"}, "the row after the failure must still be restored"
