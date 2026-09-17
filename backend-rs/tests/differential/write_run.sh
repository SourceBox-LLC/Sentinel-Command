#!/usr/bin/env bash
#
# Side-effect differential, with both tokens minted for you.
#
# Separate from http_run.sh because this one reseeds between every
# request and takes minutes, where the read differential is seconds.
#
# Usage: tests/differential/write_run.sh [-v]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"
REDIS_CONTAINER="${REDIS_CONTAINER:-cc-redis-test}"
APP_SECRET_KEY="${APP_SECRET_KEY:-differential-test-secret-not-a-real-key}"

for port in 8000 8001; do
    if ! curl -fsS -m 3 "http://127.0.0.1:$port/api/health" >/dev/null 2>&1; then
        echo "nothing healthy on :$port — start both tiers first" >&2
        exit 1
    fi
done

docker exec "$REDIS_CONTAINER" redis-cli FLUSHDB >/dev/null 2>&1 || true

ADMIN="$(cd "$REPO/backend" && env \
    APP_SECRET_KEY="$APP_SECRET_KEY" AUTH_PROVIDER=local LOCAL_ORG_ID=self-host \
    "$PYTHON" -c "
import sys; sys.path.insert(0, '.')
from app.core.local_auth import issue_token
print(issue_token())
")"

# The non-admin. issue_token() always mints org:admin, so without this
# every require_admin rejection on a mutating route goes untested — the
# half where being wrong changes data.
MEMBER="$(cd "$REPO/backend" && env APP_SECRET_KEY="$APP_SECRET_KEY" "$PYTHON" -c "
import os, time, jwt
now = int(time.time())
print(jwt.encode({
    'sub': 'sentinel-local-auth',
    'user_id': 'local-member',
    'org_id': 'self-host',
    'org_role': 'org:member',
    'iat': now,
    'exp': now + 3600,
}, os.environ['APP_SECRET_KEY'], algorithm='HS256'))
")"

# Every run's output is kept. A one-in-fifteen flake once came and went
# with its output discarded, and without the DIFFER lines there was
# nothing to diagnose it from.
LAST="$HERE/../../target/last-write-diff.out"
mkdir -p "$(dirname "$LAST")"
"$PYTHON" "$HERE/write_diff.py" "$ADMIN" "$MEMBER" "$@" 2>&1 | tee "$LAST"
status="${PIPESTATUS[0]}"
# A failing run is also kept under its own name, so the next run — say,
# re-running the one case that differed to see if it reproduces — does
# not overwrite the only record of a flake. That happened.
if [[ "$status" != 0 ]]; then
    cp "$LAST" "$(dirname "$LAST")/write-diff-failed-$(date +%Y%m%d-%H%M%S).out"
fi
exit "$status"
