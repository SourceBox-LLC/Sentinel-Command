"""The two notification preference maps must say the same thing in both stacks.

Each map is a wall of kind -> (setting_key, default) with a paragraph of
reasoning between most entries, and the reasoning is what makes them
hard to copy: the eye follows the comments and skips a line. Porting
them, `welcome` was dropped from the email map — the last entry, after
three screens of commentary, and the one an existing comment in the
tree already warned was "easy to miss".

Getting an entry wrong is close to invisible. A missing kind reads as
"this org does not want that email", which is also what a legitimately
disabled toggle looks like; a wrong default only shows for an org that
has never opened the settings page. Neither produces an error, and the
differential cannot see it either, because a notification that is never
emitted writes no row to compare.

Usage: notify_parity.py     (exits non-zero on a mismatch)
"""
import ast
import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
BACKEND_RS = HERE.parent.parent
BACKEND = BACKEND_RS.parent / "backend"

PY_SOURCE = BACKEND / "app/api/notifications.py"

import deleted_python  # noqa: E402

deleted_python.require(PY_SOURCE)
RS_SOURCE = BACKEND_RS / "src/notifications.rs"

# (python dict name, rust const name)
MAPS = [
    ("_NOTIFICATION_KIND_TO_SETTING", "INBOX_KIND_TO_SETTING"),
    ("_EMAIL_KIND_TO_SETTING", "EMAIL_KIND_TO_SETTING"),
]


def python_map(name):
    """kind -> (setting_key, default), read from the annotated assignment."""
    tree = ast.parse(PY_SOURCE.read_text())
    for node in ast.walk(tree):
        target = None
        if isinstance(node, ast.AnnAssign):
            target = node.target
        elif isinstance(node, ast.Assign) and len(node.targets) == 1:
            target = node.targets[0]
        if not isinstance(target, ast.Name) or target.id != name:
            continue
        out = {}
        for key, value in zip(node.value.keys, node.value.values):
            setting, default = ast.literal_eval(value)
            out[ast.literal_eval(key)] = (setting, default)
        return out
    raise SystemExit(f"{name} not found in {PY_SOURCE}")


def rust_map(name):
    """Same shape, read from the `[(&str, &str, bool); N]` const."""
    src = RS_SOURCE.read_text()
    m = re.search(
        rf"const {name}: \[\(&str, &str, bool\); (\d+)\] = \[(.*?)\n\];",
        src,
        re.S,
    )
    if not m:
        raise SystemExit(f"{name} not found in {RS_SOURCE}")
    declared = int(m.group(1))
    out = {}
    for kind, setting, default in re.findall(
        r'\(\s*"([^"]+)",\s*"([^"]+)",\s*(true|false)\s*\)', m.group(2)
    ):
        out[kind] = (setting, default == "true")
    if len(out) != declared:
        raise SystemExit(
            f"{name}: declares {declared} entries, parsed {len(out)} distinct kinds"
        )
    return out


def main():
    problems = []
    for py_name, rs_name in MAPS:
        py = python_map(py_name)
        rs = rust_map(rs_name)
        for kind in sorted(set(py) - set(rs)):
            problems.append(
                f"{rs_name}: missing {kind!r} — Python maps it to {py[kind]}"
            )
        for kind in sorted(set(rs) - set(py)):
            problems.append(
                f"{rs_name}: has {kind!r} ({rs[kind]}), which Python does not"
            )
        for kind in sorted(set(py) & set(rs)):
            if py[kind] != rs[kind]:
                problems.append(
                    f"{rs_name}: {kind!r} is {rs[kind]} but Python says {py[kind]}"
                )
        print(f"{rs_name}: {len(rs)} kinds, {len(set(v[0] for v in rs.values()))} settings")

    if problems:
        print()
        for problem in problems:
            print(f"  {problem}")
        print(f"\n{len(problems)} mismatch(es)")
        return 1
    print("\nboth maps agree")
    return 0


if __name__ == "__main__":
    sys.exit(main())
