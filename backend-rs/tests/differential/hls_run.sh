#!/usr/bin/env bash
#
# The live video path, scenario by scenario. See hls_diff.py for why it
# cannot be a request/response differential.
#
# Usage: tests/differential/hls_run.sh [-v]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"
APP_SECRET_KEY="${APP_SECRET_KEY:-differential-test-secret-not-a-real-key}"

for port in 8000 8001; do
    if ! curl -fsS -m 3 "http://127.0.0.1:$port/api/health" >/dev/null 2>&1; then
        echo "nothing healthy on :$port — start both tiers first" >&2
        exit 1
    fi
done

TOKEN="$(cd "$REPO/backend" && env \
    APP_SECRET_KEY="$APP_SECRET_KEY" AUTH_PROVIDER=local LOCAL_ORG_ID=self-host \
    "$PYTHON" -c "
import sys; sys.path.insert(0, '.')
from app.core.local_auth import issue_token
print(issue_token())
")"

LAST="$HERE/../../target/last-hls-diff.out"
mkdir -p "$(dirname "$LAST")"
"$PYTHON" "$HERE/hls_diff.py" "$TOKEN" "$@" 2>&1 | tee "$LAST"
exit "${PIPESTATUS[0]}"
