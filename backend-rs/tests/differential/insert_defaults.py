"""Every INSERT must name the columns SQLAlchemy would fill for itself.

A `Column(..., default=X)` is a *Python-side* default: SQLAlchemy
applies it at flush time whenever the attribute is None. So a handler
that passes the value explicitly as None still stores X — and a port
that faithfully writes the NULL the handler appears to ask for stores
something else.

That is not a hypothetical. `POST /api/integration/keys` passes
`scope_mode=None` with a comment saying integration keys have no
per-tool scoping, and the row lands with `scope_mode='all'`. Filing an
incident never mentions `report` at all, and the row lands with `''`
rather than NULL. Both were written as NULL by the port, and both are
invisible to a response comparison: nothing reads `scope_mode` on an
integration key, and the incident read path returns `self.report or ""`,
which renders NULL and `''` identically.

Only a side-effect comparison can see them, and only if a case happens
to exercise that INSERT. This checks the shape instead: every column
with a Python-side default has to appear in the column list of every
Rust INSERT into that table, so the value is a decision someone made
rather than one the database chose.

Naming the column and binding NULL is still allowed — that is a
decision. What this refuses is silence.

Usage: insert_defaults.py     (exits non-zero on an omission)
"""
import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
BACKEND_RS = HERE.parent.parent
MODELS = BACKEND_RS.parent / "backend/app/models/models.py"


def python_side_defaults() -> dict[str, list[str]]:
    """table -> columns SQLAlchemy fills in when the value is None.

    `server_default` is excluded: that one is the database's own, so
    an INSERT that omits the column gets the same value from either
    stack. `onupdate` is excluded for the same reason it is not a
    default — it fires on UPDATE, not INSERT.
    """
    out: dict[str, list[str]] = {}
    table = None
    for line in MODELS.read_text().splitlines():
        named = re.search(r'__tablename__\s*=\s*"([^"]+)"', line)
        if named:
            table = named.group(1)
            continue
        column = re.match(r"\s*(\w+)\s*=\s*Column\((.*)", line)
        if not (column and table):
            continue
        args = column.group(2)
        if "onupdate" in args:
            continue
        if re.search(r"(?<!server_)default=", args):
            out.setdefault(table, []).append(column.group(1))
    return out


def rust_inserts() -> list[tuple[str, int, str, set[str]]]:
    """(file, line, table, named columns) for every INSERT in src/."""
    found = []
    for path in sorted((BACKEND_RS / "src").rglob("*.rs")):
        text = path.read_text()
        for m in re.finditer(r"INSERT INTO (\w+)\s*\n?\s*\(([^)]*)\)", text):
            columns = {c.strip() for c in m.group(2).replace("\n", " ").split(",")}
            line = text[: m.start()].count("\n") + 1
            found.append((path.relative_to(BACKEND_RS), line, m.group(1), columns))
    return found


def main() -> int:
    defaults = python_side_defaults()
    inserts = rust_inserts()
    problems = []
    for path, line, table, columns in inserts:
        missing = [c for c in defaults.get(table, []) if c not in columns]
        if missing:
            problems.append(
                f"  {path}:{line} INSERT INTO {table} — does not name "
                f"{', '.join(missing)}, which SQLAlchemy would fill"
            )

    tables = {table for _, _, table, _ in inserts}
    print(f"{len(inserts)} INSERT(s) across {len(tables)} table(s); "
          f"{sum(len(v) for v in defaults.values())} defaulted column(s) in the models")
    if problems:
        print()
        print("\n".join(problems))
        print(f"\n{len(problems)} omission(s)")
        return 1
    print("every INSERT names each defaulted column")
    return 0


if __name__ == "__main__":
    sys.exit(main())
