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
"$PYTHON" "$HERE/diff_probes.py" \
    "$WORK/corpus.jsonl" "$WORK/py.jsonl" "$WORK/rs.jsonl" \
    "python vs rust" "$@"

# diff_probes prints the counts; re-derive the verdict for the exit code.
"$PYTHON" - "$WORK" <<'PY'
import json, sys
w = sys.argv[1]
py = [json.loads(l) for l in open(f"{w}/py.jsonl")]
rs = [json.loads(l) for l in open(f"{w}/rs.jsonl")]
bad = sum(1 for a, b in zip(py, rs) if a != b)

# A corpus that resolves nothing would compare equal and prove nothing;
# this is the guard against the test quietly going vacuous.
resolved = sum(1 for r in rs if "error" not in r)
with_perms = sum(1 for r in rs if "error" not in r and r["org_permissions"])
admins = sum(1 for r in rs if "error" not in r and r["is_admin"])
print(f"coverage: {resolved} users resolved, {with_perms} with permissions, "
      f"{admins} admin, {len(rs)-resolved} rejected")
if resolved < 100 or with_perms < 100 or admins < 50:
    print("CORPUS IS TOO THIN — this run proves nothing")
    sys.exit(2)
sys.exit(1 if bad else 0)
PY
