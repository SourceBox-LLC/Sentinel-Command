"""Every INSERT and UPDATE must name the columns SQLAlchemy fills for itself.

Two families of Python-side column argument, and the port has already
been bitten by both:

* `Column(..., default=X)` is applied on INSERT whenever the attribute
  is None — including when the handler passes None *explicitly*.
  `POST /api/integration/keys` does exactly that for `scope_mode`, with
  a comment saying integration keys have no per-tool scoping, and the
  row lands with `'all'`. Filing an incident never mentions `report`,
  and the row lands with `''`.
* `Column(..., onupdate=X)` is applied on UPDATE, every time. The
  WebSocket heartbeat writes `camera_nodes` and `cameras` without
  mentioning `updated_at`, and SQLAlchemy stamps it anyway — so a raw
  UPDATE that leaves the column alone drifts from the Python on a
  field the data-sync tier uses as its high-water mark.

None of the three is visible in a response. The first two took a
side-effect snapshot to find and the third took one written for the
WebSocket, which is to say: each was found by a harness that did not
exist when the bug was written.

**The model is parsed with `ast`, not with a regex.** The first version
of this checker matched `Column(` line by line and so saw only the
single-line definitions — every multi-line one, which is most of the
interesting ones including all six `updated_at` columns, was invisible.
It reported a clean run while checking about a third of what it claimed.

Naming a column and binding NULL is still allowed: that is a decision.
What this refuses is silence.

Usage: column_defaults.py     (exits non-zero on an omission)
"""
import ast
import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
BACKEND_RS = HERE.parent.parent
MODELS = BACKEND_RS.parent / "backend/app/models/models.py"


def model_columns() -> tuple[dict[str, list[str]], dict[str, list[str]]]:
    """(table -> columns with `default=`, table -> columns with `onupdate=`).

    `server_default` is excluded: the database applies that one, so an
    INSERT that omits the column gets the same value from either stack.
    """
    tree = ast.parse(MODELS.read_text())
    defaults: dict[str, list[str]] = {}
    onupdates: dict[str, list[str]] = {}
    for node in tree.body:
        if not isinstance(node, ast.ClassDef):
            continue
        table = None
        found_default, found_onupdate = [], []
        for stmt in node.body:
            if not (isinstance(stmt, ast.Assign) and len(stmt.targets) == 1):
                continue
            target = stmt.targets[0]
            if not isinstance(target, ast.Name):
                continue
            if target.id == "__tablename__" and isinstance(stmt.value, ast.Constant):
                table = stmt.value.value
                continue
            if not (isinstance(stmt.value, ast.Call)
                    and getattr(stmt.value.func, "id", "") == "Column"):
                continue
            keywords = {kw.arg for kw in stmt.value.keywords}
            if "default" in keywords:
                found_default.append(target.id)
            if "onupdate" in keywords:
                found_onupdate.append(target.id)
        if table:
            if found_default:
                defaults[table] = found_default
            if found_onupdate:
                onupdates[table] = found_onupdate
    return defaults, onupdates


def rust_statements(kind: str):
    """(file, line, table, named columns) for every INSERT or UPDATE."""
    found = []
    for path in sorted((BACKEND_RS / "src").rglob("*.rs")):
        text = path.read_text()
        if kind == "INSERT":
            pattern = re.compile(r"INSERT INTO (\w+)\s*\n?\s*\(([^)]*)\)")
            for m in pattern.finditer(text):
                columns = {c.strip() for c in m.group(2).replace("\n", " ").split(",")}
                found.append((path.relative_to(BACKEND_RS),
                              text[: m.start()].count("\n") + 1, m.group(1), columns))
        else:
            # `UPDATE <table> SET a = $1, b = $2 WHERE …` — the column
            # list is every assignment target up to WHERE or RETURNING.
            pattern = re.compile(
                r"UPDATE (\w+)\s+SET\s+(.*?)(?:\s+WHERE\s|\s+RETURNING\s|\")",
                re.S | re.I,
            )
            for m in pattern.finditer(text):
                columns = set(re.findall(r"(\w+)\s*=", m.group(2)))
                found.append((path.relative_to(BACKEND_RS),
                              text[: m.start()].count("\n") + 1, m.group(1), columns))
    return found


def main() -> int:
    defaults, onupdates = model_columns()
    problems = []

    inserts = rust_statements("INSERT")
    for path, line, table, columns in inserts:
        missing = [c for c in defaults.get(table, []) if c not in columns]
        if missing:
            problems.append(
                f"  {path}:{line} INSERT INTO {table} — does not name "
                f"{', '.join(missing)}, which SQLAlchemy fills on insert"
            )

    updates = rust_statements("UPDATE")
    for path, line, table, columns in updates:
        missing = [c for c in onupdates.get(table, []) if c not in columns]
        if missing:
            problems.append(
                f"  {path}:{line} UPDATE {table} — does not set "
                f"{', '.join(missing)}, which SQLAlchemy stamps on every update"
            )

    print(f"{len(inserts)} INSERT(s) and {len(updates)} UPDATE(s) in src/; "
          f"{sum(len(v) for v in defaults.values())} defaulted and "
          f"{sum(len(v) for v in onupdates.values())} onupdate column(s) in the models")
    if problems:
        print()
        print("\n".join(sorted(problems)))
        print(f"\n{len(problems)} omission(s)")
        return 1
    print("every statement names what SQLAlchemy would fill")
    return 0


if __name__ == "__main__":
    sys.exit(main())
