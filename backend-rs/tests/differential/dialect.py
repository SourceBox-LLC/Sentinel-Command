#!/usr/bin/env python3
"""The bridge between the two databases, for the dialect differential.

The PostgreSQL build of Command Center was verified against the Python
tier case by case. The SQLite build is the same Rust compiled against a
different driver, so the question it raises is narrower — *does the same
code behave the same on the other database?* — and the reference for it
is the PostgreSQL build, which is still here, rather than the Python,
which is not.

`dialect_run.sh` puts the PostgreSQL build on :8000 and the SQLite build
on :8001 and runs the existing read and write case lists at both. Those
harnesses were written against ONE database, so two things have to be
supplied for the second:

  * the fixture. `seed_cameras.sql` and every per-case `setup` are
    PostgreSQL (TRUNCATE, `now() - interval`, `setval`). Rather than
    keep a second copy in a second dialect — two fixtures drift, and a
    drifted fixture reads exactly like a port bug — the fixture is
    applied to PostgreSQL as always and then COPIED, row for row, into
    the SQLite file. `copy_from_postgres`.
  * the snapshot. `write_diff.py` reads table contents with
    `json_agg(t)`; `snapshot` reads the SQLite file and renders each
    value the way PostgreSQL's JSON would, so the two compare.

Both directions go through the declared column types in the SQLite
schema, because that is where the information is: SQLite stores a
timestamp as text and a boolean as an integer, and only the declaration
says which is which.
"""
from __future__ import annotations

import json
import re
import sqlite3
import subprocess

PG_CONTAINER = "cc-schema-test"

# How sqlx writes a chrono::NaiveDateTime to SQLite: `%F %T%.f`, where
# `%.f` is nothing for a whole second and otherwise the shortest of
# three, six or nine digits that is exact. Rows the fixture supplies are
# written the same way, so a seeded timestamp and one the tier writes
# compare as text the way they would if the tier had written both.
_PG_TS = re.compile(r"^(\d{4}-\d{2}-\d{2})T(\d{2}:\d{2}:\d{2})(?:\.(\d{1,6}))?$")
_LITE_TS = re.compile(r"^(\d{4}-\d{2}-\d{2}) (\d{2}:\d{2}:\d{2})(?:\.(\d{1,9}))?$")


def _psql(sql: str) -> str:
    out = subprocess.run(
        ["docker", "exec", "-i", PG_CONTAINER, "psql", "-U", "cc", "-d", "cc",
         "-v", "ON_ERROR_STOP=1", "-tAq", "-c", sql],
        capture_output=True, text=True, timeout=120,
    )
    if out.returncode != 0:
        raise RuntimeError(f"psql failed: {out.stderr.strip()[:300]}")
    return out.stdout


def _connect(path: str) -> sqlite3.Connection:
    con = sqlite3.connect(path, timeout=30)
    con.execute("PRAGMA busy_timeout=30000")
    return con


def _tables(con: sqlite3.Connection) -> list[str]:
    return [r[0] for r in con.execute(
        "SELECT name FROM sqlite_master WHERE type='table' "
        "AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '_sqlx_%' ORDER BY name")]


def _columns(con: sqlite3.Connection, table: str) -> list[tuple[str, str]]:
    return [(r[1], (r[2] or "").upper()) for r in con.execute(f"PRAGMA table_info({table})")]


def _to_sqlite(value, kind: str):
    if value is None:
        return None
    if kind in ("DATETIME", "TIMESTAMP"):
        m = _PG_TS.match(value) if isinstance(value, str) else None
        if not m:
            raise ValueError(f"not a timestamp PostgreSQL would render: {value!r}")
        date, clock, frac = m.groups()
        micros = int((frac or "").ljust(6, "0") or 0)
        if micros == 0:
            return f"{date} {clock}"
        if micros % 1000 == 0:
            return f"{date} {clock}.{micros // 1000:03d}"
        return f"{date} {clock}.{micros:06d}"
    if kind == "BOOLEAN":
        return 1 if value else 0
    if kind == "BLOB":
        # json_agg renders bytea as "\\x<hex>".
        if not (isinstance(value, str) and value.startswith("\\x")):
            raise ValueError(f"not a bytea PostgreSQL would render: {value!r:.40}")
        return bytes.fromhex(value[2:])
    if isinstance(value, (dict, list)):
        return json.dumps(value)
    return value


def _to_pg_json(value, kind: str):
    if value is None:
        return None
    if kind in ("DATETIME", "TIMESTAMP"):
        m = _LITE_TS.match(value) if isinstance(value, str) else None
        if not m:
            # Left as it is, so a timestamp the tier wrote in some other
            # shape shows up in the comparison instead of being repaired
            # on the way to it.
            return value
        date, clock, frac = m.groups()
        frac = (frac or "")[:6].rstrip("0")
        return f"{date}T{clock}" + (f".{frac}" if frac else "")
    if kind == "BOOLEAN":
        return bool(value) if value in (0, 1) else value
    if kind == "BLOB":
        return "\\x" + bytes(value).hex()
    return value


def align_sequences() -> None:
    """Make PostgreSQL hand out `max(id) + 1`, which is what SQLite does.

    The fixture inserts explicit ids, and a PostgreSQL sequence does not
    notice: after seeding, `settings` holds ids up to 608 and its
    sequence is still at 12, so the next insert is id 13 there and 609
    in SQLite. Both are correct and they are different rows to a
    comparison keyed on id. An INTEGER PRIMARY KEY in SQLite has no
    sequence to move, so the alignment is done on the side that has one.
    """
    names = _psql(
        "SELECT table_name FROM information_schema.columns "
        "WHERE table_schema = 'public' AND column_name = 'id' "
        "AND column_default LIKE 'nextval(%' ORDER BY table_name").split()
    if not names:
        raise RuntimeError("found no table with an id sequence — the catalog query has rotted")
    _psql("; ".join(
        f"SELECT setval(pg_get_serial_sequence('{t}', 'id'), "
        f"COALESCE((SELECT max(id) FROM {t}), 1), (SELECT max(id) FROM {t}) IS NOT NULL)"
        for t in names))


def copy_from_postgres(sqlite_path: str) -> int:
    """Make the SQLite file hold exactly what PostgreSQL holds. Returns rows."""
    con = _connect(sqlite_path)
    try:
        tables = _tables(con)
        parts = ", ".join(
            f"'{t}', COALESCE((SELECT json_agg(x) FROM (SELECT * FROM {t}) x), '[]'::json)"
            for t in tables)
        data = json.loads(_psql(f"SELECT json_build_object({parts})"))
        total = 0
        # One transaction, with foreign keys off for its duration: the
        # tables are loaded in name order, not dependency order.
        con.execute("PRAGMA foreign_keys=OFF")
        con.execute("BEGIN IMMEDIATE")
        for table in tables:
            con.execute(f"DELETE FROM {table}")
        for table in tables:
            columns = _columns(con, table)
            rows = data[table]
            if not rows:
                continue
            names = [c for c, _ in columns]
            missing = set(rows[0]) - set(names)
            if missing:
                raise RuntimeError(f"{table}: PostgreSQL has column(s) SQLite lacks: {sorted(missing)}")
            sql = (f"INSERT INTO {table} ({', '.join(chr(34) + n + chr(34) for n in names)}) "
                   f"VALUES ({', '.join('?' for _ in names)})")
            con.executemany(sql, [[_to_sqlite(row.get(n), k) for n, k in columns] for row in rows])
            total += len(rows)
        con.execute("COMMIT")
        return total
    finally:
        con.close()


def snapshot(sqlite_path: str, tables: list[str], filters: dict[str, str] | None = None) -> dict:
    """Table contents in the shape `json_agg(t) … ORDER BY 1` gives."""
    filters = filters or {}
    con = _connect(sqlite_path)
    try:
        out = {}
        for table in tables:
            columns = _columns(con, table)
            where = filters.get(table, "")
            # The filters are written for PostgreSQL, whose LIKE escapes
            # with a backslash by default. SQLite's has no default.
            if " LIKE " in where and "ESCAPE" not in where:
                where += " ESCAPE '\\'"
            rows = con.execute(f"SELECT * FROM {table}{where} ORDER BY 1").fetchall()
            out[table] = [
                {name: _to_pg_json(value, kind) for (name, kind), value in zip(columns, row)}
                for row in rows
            ]
        return out
    finally:
        con.close()


def query(sqlite_path: str, sql: str):
    con = _connect(sqlite_path)
    try:
        return con.execute(sql).fetchall()
    finally:
        con.close()


if __name__ == "__main__":
    import sys
    print(f"copied {copy_from_postgres(sys.argv[1])} row(s) into {sys.argv[1]}")
