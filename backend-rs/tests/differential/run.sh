#!/usr/bin/env bash
#
# Claim-extraction differential test: Rust vs the live Python.
#
# Generates a corpus of Clerk claim sets, resolves each one through both
# implementations, and compares the resulting AuthUser as JSON.
#
# The Python side does not re-implement anything — it imports
# `decode_v2_permissions` and `AuthUser` from the running service and
# exec's the claim-extraction block sliced out of `auth.py` by source
# text. A re-implementation would only ever encode what we already think
# auth.py says, which is the failure this is meant to catch.
#
# Usage:  tests/differential/run.sh [-v]
#
# Exits non-zero if the two disagree about any case.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BACKEND_RS="$(cd "$HERE/../.." && pwd)"
REPO="$(cd "$BACKEND_RS/.." && pwd)"
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if [[ ! -x "$PYTHON" ]]; then
    echo "no python at $PYTHON — set PYTHON=/path/to/venv/bin/python" >&2
    exit 1
fi

echo "generating corpus..."
"$PYTHON" "$HERE/gen_corpus.py" > "$WORK/corpus.jsonl"
echo "  $(wc -l < "$WORK/corpus.jsonl") claim sets"

echo "resolving through python (production code)..."
"$PYTHON" "$HERE/py_claims_probe.py" < "$WORK/corpus.jsonl" > "$WORK/py.jsonl"

echo "resolving through rust..."
( cd "$BACKEND_RS" && cargo run --quiet --example claims_probe ) \
    < "$WORK/corpus.jsonl" > "$WORK/rs.jsonl"

echo
# diff_probes.py owns the verdict: it fails on any divergence not listed
# in expected_divergences.jsonl, on any listed one that has stopped
# diverging, and on a corpus too thin to prove anything.
exec "$PYTHON" "$HERE/diff_probes.py" \
    "$WORK/corpus.jsonl" "$WORK/py.jsonl" "$WORK/rs.jsonl" \
    "python vs rust" "$@"
