"""Lightweight schema sync for SQLite + one-shot migration helpers.

Two kinds of function live here:

1. ``sync_schema`` — runs on every boot from ``app/main.py``.  Walks
   ``Base.metadata`` and ``ALTER TABLE ADD COLUMN`` for any model
   field missing from the live SQLite table.  Idempotent.  Our
   stand-in for Alembic; the most common schema change on this
   project is a single nullable column being added to a model.

2. ``drop_orphan_tables`` + ``sanitize_existing_codecs`` — one-shot
   helpers for specific past data problems.  USED to run on every
   boot but were pulled out of the startup path on 2026-05-05 once
   prod had churned through them — now both are no-ops on every
   tracked database.  Kept here as documented patterns + as
   manual-recovery tools for someone restoring an old DB snapshot.
   Run them by hand if you need to:

       from app.core.database import engine
       from app.core.migrations import drop_orphan_tables, sanitize_existing_codecs
       drop_orphan_tables(engine)
       sanitize_existing_codecs(engine)

Caveats for ``sync_schema``:
- SQLite can't add a NOT NULL column without a DEFAULT. If you add such a
  column and have existing rows, the ALTER will fail loudly — write a real
  migration in that case. Make new columns nullable or give them a default.
- SQLite ADD COLUMN doesn't create indexes or unique constraints. If you need
  those, add them manually or drop/recreate the table.
- This only handles columns added to existing tables. Renames, type changes,
  and drops still need a real migration.
"""

from __future__ import annotations

import logging
from collections.abc import Iterable

from sqlalchemy import inspect, text
from sqlalchemy.engine import Engine
from sqlalchemy.schema import Column

logger = logging.getLogger(__name__)


def _compile_column_ddl(column: Column, dialect) -> str:
    """Build an `ADD COLUMN` DDL fragment for a single column."""
    col_type = column.type.compile(dialect=dialect)
    parts = [f'"{column.name}"', col_type]

    # NULL-ability: default to nullable unless the column explicitly says NOT NULL
    # AND ships with something that can populate existing rows.
    has_server_default = column.server_default is not None
    if not column.nullable and not has_server_default:
        # SQLite rejects ADD COLUMN ... NOT NULL without a DEFAULT. Downgrade to
        # NULL here so existing rows survive; the model's Python default will
        # populate new rows. Operators who really need NOT NULL on existing data
        # should write a hand-rolled migration.
        logger.warning(
            "migrations: column %s.%s is NOT NULL with no server_default; "
            "adding as NULLABLE to keep existing rows valid",
            column.table.name,
            column.name,
        )
    elif not column.nullable:
        parts.append("NOT NULL")

    if has_server_default:
        # column.server_default.arg may be a string, a TextClause, or a callable
        default = column.server_default.arg
        if hasattr(default, "text"):
            default_sql = default.text
        else:
            default_sql = str(default)
        parts.append(f"DEFAULT {default_sql}")

    return " ".join(parts)


def _table_columns(engine: Engine, table_name: str) -> set[str]:
    insp = inspect(engine)
    return {c["name"] for c in insp.get_columns(table_name)}


def _existing_tables(engine: Engine) -> set[str]:
    return set(inspect(engine).get_table_names())


def sync_schema(engine: Engine, metadata) -> list[str]:
    """Walk every table in `metadata` and add any columns missing from the DB.

    Returns a list of human-readable change descriptions, mainly for logs/tests.
    """
    changes: list[str] = []
    existing = _existing_tables(engine)
    dialect = engine.dialect

    for table in metadata.sorted_tables:
        if table.name not in existing:
            # create_all() will have taken care of this one.
            continue

        db_cols = _table_columns(engine, table.name)
        missing: Iterable[Column] = [c for c in table.columns if c.name not in db_cols]
        if not missing:
            continue

        with engine.begin() as conn:
            for column in missing:
                ddl_fragment = _compile_column_ddl(column, dialect)
                stmt = f'ALTER TABLE "{table.name}" ADD COLUMN {ddl_fragment}'
                try:
                    conn.execute(text(stmt))
                    changes.append(f"{table.name}.{column.name}")
                    logger.info("migrations: added column %s.%s", table.name, column.name)
                except Exception as exc:  # noqa: BLE001
                    # Log and keep going — one broken column shouldn't block app start.
                    logger.error(
                        "migrations: failed to add %s.%s (%s): %s",
                        table.name,
                        column.name,
                        stmt,
                        exc,
                    )

    if changes:
        logger.info("migrations: applied %d column additions: %s", len(changes), ", ".join(changes))
    else:
        logger.debug("migrations: schema already in sync")

    return changes


def sync_indexes(engine: Engine, metadata) -> list[str]:
    """Create any model-declared indexes missing from the live DB.

    ``create_all(checkfirst=True)`` skips tables that already exist —
    ENTIRELY, indexes included — and ``sync_schema`` above only does
    ADD COLUMN.  So every ``Index(...)``/``index=True`` added to a
    model AFTER its table first shipped silently never materialized in
    prod (several composite-index commits carried comments claiming
    "picked up on next boot"; they were not).  This walks the declared
    indexes and issues ``CREATE INDEX IF NOT EXISTS`` for each —
    idempotent, WAL-friendly, and a no-op once in sync.

    Returns the list of index names created.
    """
    created: list[str] = []
    existing_tables = _existing_tables(engine)
    inspector = inspect(engine)

    for table in metadata.sorted_tables:
        if table.name not in existing_tables:
            continue  # create_all built this table fresh, indexes included
        try:
            db_indexes = {ix["name"] for ix in inspector.get_indexes(table.name)}
        except Exception:  # noqa: BLE001
            logger.exception("migrations: index inspect failed for %s", table.name)
            continue
        for index in table.indexes:
            if not index.name or index.name in db_indexes:
                continue
            cols = ", ".join(f'"{c.name}"' for c in index.columns)
            unique = "UNIQUE " if index.unique else ""
            stmt = (
                f'CREATE {unique}INDEX IF NOT EXISTS "{index.name}" '
                f'ON "{table.name}" ({cols})'
            )
            try:
                with engine.begin() as conn:
                    conn.execute(text(stmt))
                created.append(index.name)
                logger.info("migrations: created index %s on %s", index.name, table.name)
            except Exception:  # noqa: BLE001
                # One bad index must not block app start.
                logger.exception("migrations: failed to create index %s", index.name)

    if created:
        logger.info("migrations: created %d missing index(es): %s",
                    len(created), ", ".join(created))
    else:
        logger.debug("migrations: indexes already in sync")
    return created


# ─────────────────────────────────────────────────────────────────
# Orphan-table sweep (one-shot helper, NOT in the boot path)
# ─────────────────────────────────────────────────────────────────
#
# ``sync_schema`` only adds missing columns; it never drops things.  When a
# model is removed entirely, the underlying SQLite table sticks around as
# zero-row dead weight.  This helper enumerates known orphan tables and
# drops them.  Idempotent — ``DROP TABLE IF EXISTS`` noops if absent.
#
# Pulled out of the boot path on 2026-05-05 because the only entry
# (``webhook_endpoints``, retired by commit d4dd2db in Apr 2026) had been
# dropped from every prod machine for weeks and the function was paying
# a metadata round-trip per boot to do nothing.
#
# To drop a newly-orphaned table:
# 1. Append a ``(name, why)`` entry to ``_ORPHAN_TABLES`` below.
# 2. Run ``drop_orphan_tables(engine)`` once against prod (manually,
#    or by re-adding the call to ``main.py`` for one deploy then
#    removing it again).
# 3. After the deploy, blank ``_ORPHAN_TABLES`` back out and remove
#    the boot-path call so future boots don't pay the inspect cost.

_ORPHAN_TABLES: tuple[tuple[str, str], ...] = ()


def drop_orphan_tables(engine: Engine) -> list[str]:
    """Drop SQLite tables for models that have been removed from the codebase.

    NOT in the boot path — invoke manually after appending a new entry to
    ``_ORPHAN_TABLES`` (see module docstring).  Runs ``DROP TABLE IF EXISTS``
    for each entry; returns the list of tables that were actually present
    (and therefore dropped).
    """
    dropped: list[str] = []
    existing = _existing_tables(engine)

    for table_name, _why in _ORPHAN_TABLES:
        if table_name not in existing:
            continue
        with engine.begin() as conn:
            try:
                conn.execute(text(f'DROP TABLE IF EXISTS "{table_name}"'))
                dropped.append(table_name)
                logger.warning(
                    "migrations: dropped orphan table %s (zero rows, model retired)",
                    table_name,
                )
            except Exception as exc:  # noqa: BLE001
                logger.error(
                    "migrations: failed to drop orphan table %s: %s",
                    table_name,
                    exc,
                )

    if not dropped:
        logger.debug("migrations: no orphan tables present")
    return dropped


def sanitize_existing_codecs(engine: Engine) -> int:
    """One-shot sweep: rewrite any H.264 codec string below level 2.0.

    NOT in the boot path — pulled out 2026-05-05 once prod had finished
    rewriting affected rows (post-fix boots match zero rows).  Kept
    here for snapshot-restore scenarios; invoke manually if needed.

    Matches the same threshold as ``app.core.codec.sanitize_video_codec``.
    Existing rows with ``avc1.*e00*``, ``avc1.*e010``, …, ``avc1.*e013``
    (level 1.0 through 1.3) get upgraded in-place to ``*e01e`` (level 3.0)
    so the next HLS playlist fetch has a valid codec declaration.

    Idempotent: subsequent runs match zero rows and are cheap.
    """
    # Level hex < 14 is the broken range. We match by the
    # LOWER(substr(video_codec, -2)) < '14' predicate in SQL, but
    # string ordering on hex isn't safe ('2' < '14' is lexicographically
    # true, numerically false). So we use SQLite's printf + hex parsing
    # in an expression we know works: enumerate the handful of values
    # that actually are <0x14 and would be produced by normalize_h264_level
    # rounding garbage: 0a, 0b, 0c, 0d, 10, 11, 12, 13. 14 and above stay.
    bad_suffixes = ("0a", "0b", "0c", "0d", "10", "11", "12", "13")
    total_updated = 0
    for table, col in (("cameras", "video_codec"), ("camera_nodes", "video_codec")):
        for suffix in bad_suffixes:
            # Only H.264 (`avc1.` prefix) — leave hvc1/vp9/etc alone.
            # Case-insensitive on the suffix since ffprobe output varies.
            like_pattern = f"avc1.____{suffix}"
            with engine.begin() as conn:
                try:
                    result = conn.execute(
                        text(
                            f'UPDATE "{table}" '
                            f'SET {col} = substr({col}, 1, length({col}) - 2) || \'1e\' '
                            f'WHERE lower({col}) LIKE :pat'
                        ),
                        {"pat": like_pattern},
                    )
                    n = result.rowcount or 0
                    total_updated += n
                    if n:
                        logger.warning(
                            "migrations: upgraded %d %s.%s rows matching avc1.*%s → *1e",
                            n,
                            table,
                            col,
                            suffix,
                        )
                except Exception as exc:  # noqa: BLE001
                    logger.error(
                        "migrations: codec sanitize sweep failed on %s.%s / %s: %s",
                        table, col, suffix, exc,
                    )
    if total_updated:
        logger.warning(
            "migrations: codec sanitize sweep rewrote %d rows total", total_updated,
        )
    else:
        logger.debug("migrations: no codec rows needed sanitization")
    return total_updated
