#!/usr/bin/env bash
#
# The CameraNode socket. See ws_diff.py for why none of it can be a
# request/response differential.
#
# Usage: tests/differential/ws_run.sh [-v]
#
# The connect throttle lives in memory and only a tier restart clears
# it, so running this more than a few times a minute will start seeing
# throttled refusals on the functional cases. The throttle's own cases
# use a node id of their own to keep that budget separate.

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

# The raw key behind the fixture's only real api_key_hash.
NODE_KEY="${NODE_KEY:-test-node-key}"

LAST="$HERE/../../target/last-ws-diff.out"
mkdir -p "$(dirname "$LAST")"
"$PYTHON" "$HERE/ws_diff.py" "$NODE_KEY" "$@" 2>&1 | tee "$LAST"
exit "${PIPESTATUS[0]}"
