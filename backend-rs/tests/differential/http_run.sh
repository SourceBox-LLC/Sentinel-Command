#!/usr/bin/env bash
#
# HTTP differential for the ported routes: Rust (:8000) vs Python (:8001),
# both pointed at one Postgres.
#
# Seeds the fixture and diffs in one step, on purpose. The camera
# fixtures are relative to now() because `effective_status` flips a
# camera offline after 90 seconds, and a stale fixture silently reduces
# this to a test of the offline path only — which is exactly what
# happened the first time these were separate commands.
#
# Both stacks run with AUTH_PROVIDER=local so they share one HS256
# secret and therefore accept the same token. That is also what makes
# the token itself a test: it is minted by Python's own issue_token().
#
# Usage: tests/differential/http_run.sh
#
# Env: PG_CONTAINER (default cc-schema-test), APP_SECRET_KEY, PYTHON

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
REDIS_CONTAINER="${REDIS_CONTAINER:-cc-redis-test}"
APP_SECRET_KEY="${APP_SECRET_KEY:-differential-test-secret-not-a-real-key}"

for port in 8000 8001; do
    if ! curl -fsS -m 3 "http://127.0.0.1:$port/api/health" >/dev/null 2>&1; then
        echo "nothing healthy on :$port — start both tiers first" >&2
        echo "  python: AUTH_PROVIDER=local APP_SECRET_KEY=... uvicorn app.main:app --port 8001" >&2
        echo "  rust:   AUTH_PROVIDER=local APP_SECRET_KEY=... ./target/debug/sentinel-command" >&2
        exit 1
    fi
done

# Both tiers now rate-limit the routes Python decorates, so counters
# have to start empty or a second run in the same minute reports 429s as
# if they were port bugs. They are shared in Redis precisely so this is
# one command rather than a process restart.
if docker exec "$REDIS_CONTAINER" redis-cli FLUSHDB >/dev/null 2>&1; then
    echo "flushed rate-limit counters"
else
    echo "WARNING: no redis at $REDIS_CONTAINER — counters carry over between runs," >&2
    echo "         so back-to-back runs may report 429s. Wait 60s or start redis." >&2
fi

echo "seeding fixtures..."
docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q < "$HERE/seed_cameras.sql"

# Several ported routes page with ORDER BY <timestamp> DESC and no
# tiebreaker, so two rows sharing a sort key let Postgres return a
# different page each run. That is a fixture defect that reads exactly
# like a port bug — it cost a debugging detour once already.
TIES=$(docker exec "$PG_CONTAINER" psql -U cc -d cc -tAc "
SELECT coalesce(sum(n-1),0) FROM (
  SELECT count(*) n FROM stream_access_logs GROUP BY org_id, accessed_at HAVING count(*)>1
  UNION ALL SELECT count(*) FROM motion_events GROUP BY org_id, timestamp HAVING count(*)>1
  UNION ALL SELECT count(*) FROM mcp_activity_logs GROUP BY org_id, timestamp HAVING count(*)>1
  UNION ALL SELECT count(*) FROM audit_log GROUP BY org_id, timestamp HAVING count(*)>1
) t" | tr -d '[:space:]')
if [[ "${TIES:-0}" != "0" ]]; then
    echo "FIXTURE DEFECT: $TIES tied sort key(s) — paged results are non-deterministic" >&2
    echo "and any diff they produce is a flake, not a finding. Fix seed_cameras.sql." >&2
    exit 2
fi

echo "minting a token with Python's own issue_token()..."
TOKEN="$(cd "$REPO/backend" && env \
    APP_SECRET_KEY="$APP_SECRET_KEY" AUTH_PROVIDER=local LOCAL_ORG_ID=self-host \
    "$PYTHON" -c "
import sys; sys.path.insert(0, '.')
from app.core.local_auth import issue_token
print(issue_token())
")"

echo
exec "$PYTHON" "$HERE/http_diff.py" "$TOKEN" "$@"
