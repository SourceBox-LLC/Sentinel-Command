#!/usr/bin/env python3
"""Restore a self-hosted Command Center's data from its cloud mirror.

The other half of app/core/sync_client.py. That pushes rows up to
Sentinel-Sync-Service every 30 minutes; this pulls them back down —
which is what makes the mirror a backup rather than a write-only
archive.

Usage (from backend/):
    uv run python scripts/restore_from_cloud.py --list
    uv run python scripts/restore_from_cloud.py --dry-run
    uv run python scripts/restore_from_cloud.py
    uv run python scripts/restore_from_cloud.py --table cameras --overwrite

Reads SENTINEL_LICENSE_KEY and SENTINEL_SYNC_SERVICE_URL from the same
config the sync loop uses, so a restore needs no arguments a working
install doesn't already have.

Non-destructive by default: rows whose primary key already exists
locally are skipped, not replaced. Someone running this is usually
mid-incident, and a recovery tool's worst failure mode is making
things worse than it found them. `--overwrite` opts into replacement.
"""

from __future__ import annotations

import argparse
import sys
from datetime import datetime
from pathlib import Path

import httpx
from sqlalchemy import DateTime
from sqlalchemy import inspect as sa_inspect
from sqlalchemy.exc import SQLAlchemyError

# Run as `uv run python scripts/restore_from_cloud.py` from backend/ —
# scripts/ isn't on sys.path by default, so `app` isn't importable
# without this. Same pattern as Sentinel-License-Service's CLIs.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from app.core.config import settings  # noqa: E402
from app.core.database import Base, SessionLocal, engine  # noqa: E402
from app.core.migrations import sync_schema  # noqa: E402
from app.core.sync_client import _table_specs  # noqa: E402

# Imported purely for the side effect of registering every table on
# Base.metadata, so the schema bootstrap below knows what to create.
from app.models import models as _register_models  # noqa: E402,F401

_TIMEOUT_SECONDS = 60.0
_PAGE_SIZE = 500

# camera_nodes.api_key_hash is NOT NULL, but deliberately never
# mirrored (it authenticates a node to THIS Command Center — see
# sync_client._COLUMN_DENYLIST). A restored row therefore has no value
# to put there, and the column can't be left empty.
#
# This sentinel is deliberately not valid hex, so it can never collide
# with a real SHA-256 hash: a restored node simply fails authentication
# until it re-registers, which is the correct outcome. Re-registration
# mints a fresh key anyway, so nothing is lost but the illusion that
# the old credential survived.
_UNRESTORABLE_CREDENTIAL = "restored-node-must-re-register"


def _models_by_table() -> dict:
    """Table name -> model, taken from the sync spec so the two halves
    can't drift: anything mirrored is restorable and vice versa."""
    return {spec.model.__table__.name: spec.model for spec in _table_specs()}


def _client() -> httpx.Client:
    if not settings.SENTINEL_LICENSE_KEY:
        sys.exit(
            "SENTINEL_LICENSE_KEY is not set — the cloud mirror is licence-gated,\n"
            "so there's nothing to restore from without the key this install syncs with."
        )
    return httpx.Client(
        base_url=settings.SENTINEL_SYNC_SERVICE_URL.rstrip("/"),
        headers={"Authorization": f"Bearer {settings.SENTINEL_LICENSE_KEY}"},
        timeout=_TIMEOUT_SECONDS,
    )


def _fail_on_error(resp: httpx.Response) -> None:
    if resp.status_code == 403:
        sys.exit(
            "Sync service rejected the licence key (403).\n"
            "Either the licence has no data-sync entitlement, or it's been "
            "revoked/expired."
        )
    if resp.status_code == 401:
        sys.exit("Sync service rejected the request as unauthenticated (401).")
    resp.raise_for_status()


def _fetch_tables(client: httpx.Client) -> list[dict]:
    resp = client.get("/v1/sync/tables")
    _fail_on_error(resp)
    return resp.json()["tables"]


def _iter_rows(client: httpx.Client, table: str):
    """Walk every page for one table. The service paginates on row_id,
    so this terminates even while a sync push is running concurrently."""
    cursor = None
    while True:
        params = {"table": table, "limit": _PAGE_SIZE}
        if cursor:
            params["cursor"] = cursor
        resp = client.get("/v1/sync/rows", params=params)
        _fail_on_error(resp)
        body = resp.json()
        yield from body["rows"]
        cursor = body.get("next_cursor")
        if not cursor:
            return


def _coerce(model, payload: dict) -> dict:
    """Map a mirrored payload onto real column values.

    Keys are column names (sync_client mirrors raw columns), so this is
    mostly a filter plus datetime parsing. Unknown keys are dropped
    rather than raising: a mirror written by a newer or older Command
    Center than the one restoring must not hard-fail the whole restore.
    """
    columns = {c.name: c for c in model.__table__.columns}
    values: dict = {}
    for key, value in payload.items():
        column = columns.get(key)
        if column is None:
            continue
        # JSON has no date type, so sync_client sent these as ISO
        # strings; turn them back into datetimes for a DateTime column.
        if isinstance(value, str) and isinstance(column.type, DateTime):
            try:
                value = datetime.fromisoformat(value)
            except ValueError:
                # Unparseable timestamp shouldn't sink the whole row —
                # the column is better empty than the record lost.
                value = None
        values[key] = value

    if model.__table__.name == "camera_nodes" and not values.get("api_key_hash"):
        values["api_key_hash"] = _UNRESTORABLE_CREDENTIAL
    return values


def restore_table(db, model, rows, *, overwrite: bool, dry_run: bool) -> tuple[int, int, int]:
    """Returns (written, skipped, failed).

    A row that can't be written — a mirror record missing a NOT NULL
    column, say — is counted and stepped over rather than aborting the
    run. Without that, a single bad record rolls the whole transaction
    back and a restore of 10,000 rows yields nothing, which is the
    worst possible outcome for a tool someone reaches for after losing
    a disk. Each write gets its own SAVEPOINT so a failure doesn't
    poison the surrounding transaction.
    """
    written = skipped = failed = 0
    pk = list(model.__table__.primary_key.columns)[0].name

    # On a fresh machine the table may not exist yet. A real run creates
    # the schema first (see main()), but a dry run promises to touch
    # nothing — so instead of creating tables just to look in them,
    # treat "no table" as "nothing exists locally", which is exactly
    # what it means.
    table_exists = sa_inspect(engine).has_table(model.__table__.name)

    for row in rows:
        values = _coerce(model, row["data"])
        key = values.get(pk)
        if key is None:
            skipped += 1
            continue

        existing = (
            db.query(model).filter(getattr(model, pk) == key).first()
            if table_exists
            else None
        )
        if existing is not None and not overwrite:
            skipped += 1
            continue

        if dry_run:
            written += 1
            continue

        try:
            with db.begin_nested():
                if existing is not None:
                    for k, v in values.items():
                        setattr(existing, k, v)
                else:
                    db.add(model(**values))
            written += 1
        except SQLAlchemyError as exc:
            # Savepoint rolled back; the outer transaction is still
            # usable, so the remaining rows get their chance.
            failed += 1
            print(
                f"    row {key!r} could not be restored: "
                f"{type(exc).__name__} — {str(exc).splitlines()[0]}",
                file=sys.stderr,
            )

    if not dry_run:
        db.commit()
    return written, skipped, failed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--list", action="store_true",
        help="Show what the cloud mirror holds and exit. Also the quickest way "
             "to confirm syncing is actually working.",
    )
    parser.add_argument(
        "--dry-run", action="store_true",
        help="Report what would be written without touching the database.",
    )
    parser.add_argument("--table", default=None, help="Restore only this table.")
    parser.add_argument(
        "--overwrite", action="store_true",
        help="Replace rows that already exist locally. Off by default so a "
             "restore can't damage a database that still has data in it.",
    )
    args = parser.parse_args()

    models = _models_by_table()

    with _client() as client:
        summaries = _fetch_tables(client)

        if not summaries:
            print("Cloud mirror is empty — nothing to restore.")
            print("(If this install should be syncing, check that the licence "
                  "has the data-sync entitlement and that a sync tick has run.)")
            return 0

        if args.list:
            print(f"{'table':<24} {'rows':>8} {'deleted':>8}")
            for s in summaries:
                print(f"{s['table']:<24} {s['rows']:>8} {s['deleted']:>8}")
            return 0

        wanted = [s for s in summaries if args.table in (None, s["table"])]
        if args.table and not wanted:
            print(f"No mirrored data for table {args.table!r}.", file=sys.stderr)
            return 1

        # A restore is most likely run against a machine that has never
        # started the app — that's the whole scenario. Bootstrapping the
        # schema here (the same create_all + sync_schema main.py runs at
        # boot) means "install, restore, start" works, instead of dying
        # on a raw "no such table" traceback and forcing the operator to
        # discover they must boot the app first.
        if not args.dry_run:
            Base.metadata.create_all(bind=engine)
            sync_schema(engine, Base.metadata)

        db = SessionLocal()
        try:
            total_written = total_skipped = total_failed = 0
            for summary in wanted:
                table = summary["table"]
                model = models.get(table)
                if model is None:
                    # Mirrored by a Command Center that syncs a table
                    # this build doesn't know about. Skip loudly rather
                    # than guessing at a schema.
                    print(f"  {table:<22} skipped — not a table this build knows")
                    continue

                written, skipped, table_failed = restore_table(
                    db, model, _iter_rows(client, table),
                    overwrite=args.overwrite, dry_run=args.dry_run,
                )
                total_written += written
                total_skipped += skipped
                total_failed += table_failed
                verb = "would restore" if args.dry_run else "restored"
                line = f"  {table:<22} {verb} {written:>6}, skipped {skipped:>6}"
                if table_failed:
                    line += f", FAILED {table_failed:>4}"
                print(line)

            print()
            if args.dry_run:
                print(f"Dry run: would write {total_written} row(s), skip {total_skipped}.")
                print("Re-run without --dry-run to apply.")
            else:
                print(f"Restored {total_written} row(s), skipped {total_skipped}.")
                if total_skipped and not args.overwrite:
                    print("Skipped rows already existed locally. Use --overwrite to replace them.")
                if total_failed:
                    # Loud, and reflected in the exit code: a partial
                    # restore that looks like a clean one is how someone
                    # discovers months later that data they believed was
                    # recovered never actually came back.
                    print()
                    print(f"WARNING: {total_failed} row(s) could not be restored (listed above).")
                    print("Everything else was restored — this was not an all-or-nothing failure.")
                print()
                print("Note: camera nodes restore WITHOUT their API keys — those are")
                print("never mirrored. Each node must re-register to get a fresh key.")
                if total_failed:
                    return 2
        finally:
            db.close()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
