"""Client for the separate Sentinel-Sync-Service — cloud data-sync tier
for self-hosted installs (AUTH_PROVIDER=local). See
app/core/license_client.py for the sibling module this mirrors (Sentinel
AI licensing) and Sentinel-Sync-Service/README.md (sibling repo) for the
service itself.

Design: one-way mirror. This Command Center's own local database is
always the source of truth; this module only ever pushes, never pulls —
the app must keep working with zero internet, so nothing here is ever on
a read/request hot path. A background loop (app/main.py's
_data_sync_loop) calls push_pending_changes() on an interval; failures
are logged and retried next cycle, exactly like the license check-in
loop's fail-open posture.

Per-table incremental sync uses a high-water-mark cursor (the table's
`updated_at`, or `created_at`/`timestamp` for append-only tables) stored
in the same Setting KV table the license client already uses. Retries
are safe because Sentinel-Sync-Service's push endpoint upserts by
(tenant, table, row_id) — at-least-once delivery, not exactly-once.

Deletion handling is deliberately asymmetric across tables:
  - Small "identity" tables (cameras, groups, nodes) get a full-state
    reconciliation pass alongside the incremental push: every push
    includes the complete current set of local row ids, and
    Sentinel-Sync-Service tombstones anything under this tenant+table
    that's no longer in that set. Cheap because these tables are small
    (tens to low hundreds of rows even on a large install).
  - High-volume log/event tables (motion events, notifications, sentinel
    runs, incident evidence) do NOT propagate local deletions at all.
    This is intentional, not a gap: local retention jobs
    (_log_cleanup_loop) prune these tables specifically because local
    disk is limited — the whole point of the cloud mirror is to let it
    keep a longer history than local, so a local retention delete must
    never delete the cloud copy.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from datetime import datetime

import httpx
from sqlalchemy.orm import Session

from app.core.config import settings
from app.core.license_client import is_sync_enabled

logger = logging.getLogger(__name__)

_PUSH_TIMEOUT_SECONDS = 30.0
_BATCH_SIZE = 500


@dataclass(frozen=True)
class SyncTableSpec:
    model: type
    cursor_attr: str  # attribute to filter/order pushes by
    reconcile_deletes: bool = False


def _table_specs() -> list[SyncTableSpec]:
    # Imported lazily (like license_client.py's Setting imports) to
    # avoid a hard import-time dependency between this module and the
    # models module for callers that only need is_sync_enabled.
    from app.models.models import (
        Camera,
        CameraGroup,
        CameraNode,
        Incident,
        IncidentEvidence,
        MotionEvent,
        Notification,
        SentinelConfig,
        SentinelRun,
    )

    return [
        SyncTableSpec(Camera, "updated_at", reconcile_deletes=True),
        SyncTableSpec(CameraGroup, "updated_at", reconcile_deletes=True),
        SyncTableSpec(CameraNode, "updated_at", reconcile_deletes=True),
        SyncTableSpec(Incident, "updated_at"),
        SyncTableSpec(IncidentEvidence, "timestamp"),
        SyncTableSpec(MotionEvent, "timestamp"),
        SyncTableSpec(SentinelConfig, "updated_at"),
        SyncTableSpec(SentinelRun, "updated_at"),
        SyncTableSpec(Notification, "created_at"),
    ]


def _cursor_setting_key(table_name: str) -> str:
    return f"sentinel_sync_cursor_{table_name}"


# Columns that must never leave this install, keyed by table name.
#
# This is the explicit half of a decision that used to be implicit.
# The payload originally reused each model's `to_dict()`, on the
# reasoning that anything its author hadn't chosen to expose over the
# API couldn't leak into the cloud either. That held for secrecy — but
# to_dict() is a *display* shape, and it silently made the mirror
# unrestorable: Camera lost 11 of 21 columns (the whole recording
# policy nests under "recording_policy"; video/audio codec vanish),
# SentinelRun never carried tool_trace at all. A backup you can't
# restore from isn't a backup, so the payload is now raw columns and
# the exclusions are stated here instead of being a side effect.
_COLUMN_DENYLIST: dict[str, set[str]] = {
    # The hash that authenticates a node to THIS Command Center.
    # Useless to a restore (a restored node re-registers and gets a
    # fresh key) and actively dangerous in a cloud mirror.
    "camera_nodes": {"api_key_hash"},
    # Snapshot/clip bytes — tens of MB per row. Deliberately not
    # synced (see this module's docstring); the metadata columns around
    # it still are, so a restore knows evidence existed and what it
    # was. Also note this column is deferred() on the model: the
    # denylist check below happens BEFORE the attribute is read, which
    # is what stops iterating columns from lazy-loading every blob.
    "incident_evidence": {"data"},
}


def _row_payload(row) -> dict:
    """Serialise a row as its raw column values, minus anything denied.

    Keys are column names, so the cloud copy round-trips back into the
    same schema. Datetimes become ISO strings (JSON has no date type);
    everything else in these tables is already a JSON scalar.
    """
    denied = _COLUMN_DENYLIST.get(row.__table__.name, frozenset())
    payload: dict = {}
    for column in row.__table__.columns:
        if column.name in denied:
            continue
        value = getattr(row, column.name)
        if isinstance(value, datetime):
            value = value.isoformat()
        elif isinstance(value, (bytes, bytearray, memoryview)):
            # Nothing should reach this — blob columns are denied above
            # — but a future model gaining one must not make the whole
            # sync cycle die on a JSON encode error. Record the size so
            # the omission is visible in the mirror rather than silent.
            value = {"__omitted__": "binary", "bytes": len(value)}
        payload[column.name] = value
    return payload


async def _push_table(client: httpx.AsyncClient, db: Session, spec: SyncTableSpec) -> None:
    from app.models.models import Setting

    table_name = spec.model.__tablename__
    cursor_key = _cursor_setting_key(table_name)
    cursor_raw = Setting.get(db, settings.LOCAL_ORG_ID, cursor_key, "")
    cursor = datetime.fromisoformat(cursor_raw) if cursor_raw else None
    cursor_col = getattr(spec.model, spec.cursor_attr)

    while True:
        query = db.query(spec.model)
        if cursor is not None:
            query = query.filter(cursor_col > cursor)
        rows = query.order_by(cursor_col.asc(), spec.model.id.asc()).limit(_BATCH_SIZE).all()
        if not rows:
            break

        payload: dict = {
            "table": table_name,
            "rows": [
                {
                    "id": str(row.id),
                    "updated_at": getattr(row, spec.cursor_attr).isoformat(),
                    "data": _row_payload(row),
                }
                for row in rows
            ],
        }
        if spec.reconcile_deletes:
            # Full current-id snapshot, not just this batch — cheap for
            # these small identity tables, and lets Sentinel-Sync-Service
            # tombstone anything no longer present locally.
            payload["known_ids"] = [str(r.id) for r in db.query(spec.model.id).all()]

        resp = await client.post(
            f"{settings.SENTINEL_SYNC_SERVICE_URL.rstrip('/')}/v1/sync/push",
            headers={"Authorization": f"Bearer {settings.SENTINEL_LICENSE_KEY}"},
            json=payload,
        )
        resp.raise_for_status()

        # Advances past the last row's exact timestamp with a strict `>`
        # filter next cycle — theoretically could skip a same-timestamp
        # row if a batch boundary lands mid-tie (Python datetimes are
        # microsecond-granular, so this needs multiple rows sharing a
        # timestamp AND a batch cut landing between them; acceptable
        # residual risk for a best-effort backup mirror, not a ledger).
        new_cursor = getattr(rows[-1], spec.cursor_attr)
        Setting.set(db, settings.LOCAL_ORG_ID, cursor_key, new_cursor.isoformat())

        if len(rows) < _BATCH_SIZE:
            break


async def push_pending_changes(db: Session) -> None:
    """Push every syncable table's changes since its last cursor.
    Never raises — same fail-open contract as
    check_in_with_license_service; a push failure just means this
    cycle's data waits for the next tick, cursors only advance on
    confirmed success.
    """
    if not is_sync_enabled(db):
        return

    try:
        async with httpx.AsyncClient(timeout=_PUSH_TIMEOUT_SECONDS) as client:
            for spec in _table_specs():
                try:
                    await _push_table(client, db, spec)
                except Exception:
                    # One table's failure (e.g. a transient 5xx) must not
                    # block the others — each table's cursor is
                    # independent, so partial progress this cycle is
                    # both safe and useful.
                    logger.warning(
                        "[SentinelSync] push failed for table=%s",
                        spec.model.__tablename__,
                        exc_info=True,
                    )
    except Exception:
        logger.exception("[SentinelSync] push cycle failed")
