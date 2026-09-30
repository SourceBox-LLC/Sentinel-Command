#!/usr/bin/env python3
"""Refuse clearly when a checker's Python half is gone.

Seven of the static checkers here read `backend/app/**` as their source
of truth and compare it against the Rust. That was the point of them —
neither side is trusted, both are parsed — and it means they stop working
the moment the web tier is deleted, which happened in `2baabe6`.

Without this they die on a `FileNotFoundError` traceback, which reads
like a broken script rather than a fact about the tree. A checker that
looks broken gets fixed or deleted by whoever hits it next; a checker
that says *why* it cannot run, and how to run it, keeps its value as the
record of how equivalence was established.

So each of them calls `require(...)` first and exits 2 with a sentence.
Exit 2, not 1: 1 means "I ran and found a difference", and a harness
runner that treats "could not run" as "no differences found" is the
failure this whole directory exists to avoid.
"""
from __future__ import annotations

import pathlib
import sys

#: The commit that deleted `backend/app/` except `sentinel_agent/`.
DELETED_IN = "2baabe6"


def require(*paths: pathlib.Path) -> None:
    """Exit 2 unless every path exists."""
    missing = [p for p in paths if not p.exists()]
    if not missing:
        return

    print(
        "REFUSING: this checker reads the Python web tier, which was deleted "
        f"in {DELETED_IN}.",
        file=sys.stderr,
    )
    for path in missing:
        print(f"    missing: {path}", file=sys.stderr)
    print(
        f"\nIt still runs against the commit before that:\n"
        f"    git worktree add /tmp/pre-cut {DELETED_IN}~1\n"
        f"    cd /tmp/pre-cut/backend-rs/tests/differential && python3 "
        f"{pathlib.Path(sys.argv[0]).name}\n"
        "\nWhat holds these invariants now is the Rust test suite — see "
        "README.md, 'After the cut'.",
        file=sys.stderr,
    )
    raise SystemExit(2)
