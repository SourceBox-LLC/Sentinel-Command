"""Properties that must hold on BOTH SQLite and Postgres.

Hosted Command Center runs Postgres; self-hosted runs SQLite from this
same codebase. CI runs the whole suite against each, so these tests
assert once and are checked twice — they're the guard against a change
that quietly works on one engine and breaks the other.

Everything here pins something where the two engines genuinely differ
underneath and the application code papers over it. Each case is one
that already bit, or came within an inch of biting.
"""

from __future__ import annotations

from datetime import datetime

import pytest
from sqlalchemy import Boolean, Column, Integer, String, func, select, text
from sqlalchemy.schema import CreateTable

from app.core.database import Base, engine
from app.core.migrations import _compile_column_ddl, sync_schema
from app.models.models import Camera


def test_boolean_server_default_ddl_is_valid_on_this_dialect():
    """Regression: `server_default="0"` rendered as a bare `DEFAULT 0` in
    the ADD COLUMN path while `create_all` rendered `DEFAULT '0'`.
    Postgres coerces the quoted form and rejects the bare integer
    ("column is of type boolean but default expression is of type
    integer"), so fresh table creation succeeded and a later ADD COLUMN
    failed — a trap that only springs on the *next* Boolean column
    anyone adds.

    Both paths must render the same, valid default on either engine.
    """
    col = Camera.__table__.c.disabled_by_plan
    add_ddl = _compile_column_ddl(col, engine.dialect)
    create_ddl = str(CreateTable(Camera.__table__).compile(dialect=engine.dialect))

    # A bare integer default on a Boolean is the specific broken form.
    assert "DEFAULT 0" not in add_ddl, f"bare integer default is invalid on Postgres: {add_ddl}"
    assert "DEFAULT false" in add_ddl
    assert "DEFAULT false" in [
        line.strip() for line in create_ddl.splitlines() if "disabled_by_plan" in line
    ][0]


def test_add_column_with_boolean_default_applies_to_an_existing_table():
    """The end-to-end version of the above: sync_schema must be able to
    add a Boolean column carrying a server_default to a table that
    already exists. Nothing else in the suite exercises the ADD COLUMN
    path, because fixtures always build schemas from scratch.
    """
    Base.metadata.create_all(bind=engine)
    colname = "portability_probe_flag"
    Camera.__table__.append_column(
        Column(colname, Boolean, nullable=False, server_default=text("false"))
    )
    try:
        changes = sync_schema(engine, Base.metadata)
        assert f"cameras.{colname}" in changes

        inspector_cols = {c.name for c in Camera.__table__.columns}
        assert colname in inspector_cols
        with engine.begin() as conn:
            conn.execute(text(f'ALTER TABLE cameras DROP COLUMN "{colname}"'))
    finally:
        Camera.__table__._columns.remove(Camera.__table__.c[colname])


def test_one_failing_add_column_does_not_block_the_others(monkeypatch):
    """sync_schema logs and continues past a bad column. That only
    isolates the failure if the failure is isolated: on SQLite each
    statement stands alone, but Postgres aborts the whole transaction,
    so a shared transaction meant one bad column silently took every
    later column on that table with it.
    """
    from app.core import migrations

    Base.metadata.create_all(bind=engine)
    names = ["probe_before", "probe_bad", "probe_after"]
    for n in names:
        Camera.__table__.append_column(Column(n, String(10), nullable=True))

    real = migrations._compile_column_ddl

    def sabotage(column, dialect):
        if column.name == "probe_bad":
            # A trailing DEFAULT with no value: a syntax error on both
            # engines. An unknown *type* name won't do — SQLite has
            # dynamic typing and cheerfully accepts arbitrary type names,
            # so it would only fail on Postgres and this test would pass
            # vacuously on the SQLite leg.
            return '"probe_bad" VARCHAR(10) DEFAULT'
        return real(column, dialect)

    monkeypatch.setattr(migrations, "_compile_column_ddl", sabotage)
    try:
        changes = migrations.sync_schema(engine, Base.metadata)
        assert "cameras.probe_before" in changes
        assert "cameras.probe_bad" not in changes
        # The one that matters: a column queued AFTER the failure.
        assert "cameras.probe_after" in changes, (
            "a failed ADD COLUMN swallowed the columns after it"
        )
        with engine.begin() as conn:
            for n in ("probe_before", "probe_after"):
                conn.execute(text(f'ALTER TABLE cameras DROP COLUMN "{n}"'))
    finally:
        for n in names:
            if n in Camera.__table__.c:
                Camera.__table__._columns.remove(Camera.__table__.c[n])


def test_func_date_stringifies_identically_on_both_engines():
    """`func.date()` returns TEXT on SQLite and a `datetime.date` on
    Postgres. The MCP-activity and audit endpoints do `str(d)` on the
    result to build their `by_day` buckets, which happens to produce the
    same ISO string either way — but nothing tested that, and neither
    endpoint's grouping is covered elsewhere. Pinned so a future change
    to that serialisation can't silently alter the API shape on one
    engine only.
    """
    from sqlalchemy import DateTime, MetaData, Table

    md = MetaData()
    probe = Table(
        "_date_probe", md,
        Column("id", Integer, primary_key=True),
        Column("ts", DateTime),
    )
    probe.create(engine, checkfirst=True)
    try:
        with engine.begin() as conn:
            conn.execute(probe.delete())
            conn.execute(probe.insert().values(id=1, ts=datetime(2026, 9, 5, 14, 30)))
            d = conn.execute(select(func.date(probe.c.ts))).scalar()
    finally:
        probe.drop(engine, checkfirst=True)

    assert str(d) == "2026-09-05", (
        f"func.date() stringified to {str(d)!r} on {engine.dialect.name}; "
        "the by_day API buckets depend on this being a bare ISO date"
    )
