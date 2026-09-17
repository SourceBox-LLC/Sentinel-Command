#!/usr/bin/env bash
# Mint the admin and member tokens the same way http_run.sh does, then
# run ratelimit_order.py. Exists so mutate.py can drive it.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"
export APP_SECRET_KEY="${APP_SECRET_KEY:-differential-test-secret-not-a-real-key}"
TOKENS="$(cd "$REPO/backend" && AUTH_PROVIDER=local LOCAL_ORG_ID=self-host "$PYTHON" -c "
import sys, time, os, jwt; sys.path.insert(0, '.')
from app.core.local_auth import issue_token
now = int(time.time())
member = jwt.encode({'sub': 'sentinel-local-auth', 'user_id': 'local-member',
    'org_id': 'self-host', 'org_role': 'org:member', 'iat': now, 'exp': now + 3600},
    os.environ['APP_SECRET_KEY'], algorithm='HS256')
print(issue_token(), member)" 2>/dev/null)"
# shellcheck disable=SC2086
exec "$PYTHON" "$HERE/ratelimit_order.py" $TOKENS "$@"
