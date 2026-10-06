#!/usr/bin/env bash
#
# The MCP protocol differential. Needs both tiers up; reseeds between
# every request, because `effective_status` ages the fixture faster than
# two sequential calls take.
#
# Usage: tests/differential/mcp_run.sh [-v]
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"
for port in 8000 8001; do
    if ! curl -fsS -m 3 "http://127.0.0.1:$port/api/health" >/dev/null 2>&1; then
        echo "nothing healthy on :$port — start both tiers first" >&2
        exit 1
    fi
done
LAST="$HERE/../../target/last-mcp-diff.out"
mkdir -p "$(dirname "$LAST")"
"$PYTHON" "$HERE/mcp_diff.py" "$@" 2>&1 | tee "$LAST"
