#!/usr/bin/env bash
#
# The CSV differential — the one harness whose Python half comes out of
# git history.
#
# The three `?format=csv` exports were forwarded to Python until the web
# tier was deleted, which turned three documented deferrals into three
# 502s. They were then ported with no running reference, which is exactly
# the situation every other slice avoided. This gets the reference back:
# Python is served from a WORKTREE of the commit before the cut, on the
# same Postgres as the Rust tier that is under test.
#
# Usage: tests/differential/csv_run.sh [-v]
#
# Env: PRE_CUT (worktree path; created here if absent), PG_CONTAINER,
#      PYTHON (an interpreter that can import the pre-cut web tier; if it
#      cannot, a venv is synced inside the worktree from the pre-cut
#      pyproject and used instead).
#
# Two things about the worktree are deliberate:
#
#   * Nothing is BUILT in it. It supplies Python source and a fixture,
#     no more. `cargo build` there would put a target/ directory in a
#     tmpfs and the point of the worktree is to be cheap.
#   * The Rust binary is the MAIN checkout's. Diffing the worktree
#     against itself would compare the pre-cut Rust, which still
#     forwards, against the Python it forwards to — and pass while
#     proving nothing.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
RS="$REPO/backend-rs"
LOGS="${TIER_LOGS:-$RS/target/tier-logs}"
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
REDIS_CONTAINER="${REDIS_CONTAINER:-cc-redis-test}"
# Under target/, which is gitignored and on real disk: a worktree in
# a tmpfs is a worktree that disappears, and one beside the repo
# clutters a directory that is not ours.
PRE_CUT="${PRE_CUT:-$RS/target/pre-cut-worktree}"

# The commit that deleted the web tier. Its parent is the last one whose
# Python can answer these routes.
CUT="${CUT:-2baabe6}"

# Same environment as tiers.sh, because a tier that differs from the
# other in its configuration reports differences that are configuration.
export APP_SECRET_KEY="${APP_SECRET_KEY:-differential-test-secret-not-a-real-key}"
export AUTH_PROVIDER=local
export LOCAL_ORG_ID=self-host
export LOCAL_ADMIN_USERNAME=admin
export LOCAL_ADMIN_EMAIL=admin@example.com
export LOCAL_ADMIN_PASSWORD_HASH='$argon2id$v=19$m=65536,t=3,p=4$Ro3CVUFhr5w3hNxP8Cfe9A$8L+JCXU1z+/zs9b32O92qCPEvf8AhWmpViV5KwwLRfc'
export REDIS_URL="${REDIS_URL:-redis://127.0.0.1:16379/0}"
export STATIC_DIR="$PRE_CUT/backend/static"
export SCRIPTS_DIR="$PRE_CUT/backend/scripts"
FOREVER=315360000
export OFFLINE_SWEEP_INTERVAL_SECONDS=$FOREVER
export SENTINEL_REAPER_INTERVAL_SECONDS=$FOREVER
export MOTION_DIGEST_INTERVAL_SECONDS=$FOREVER
export DISK_CHECK_INTERVAL_SECONDS=$FOREVER
export RELEASE_CACHE_REFRESH_INTERVAL_SECONDS=$FOREVER
export EMAIL_WORKER_INTERVAL_SECONDS=$FOREVER
export VIEWER_USAGE_FLUSH_INTERVAL_SECONDS=$FOREVER

# Ports separate from the 8000/8001 pair, so a running differential
# session is not torn down by this one.
PY_PORT=8011
RS_PORT=8010

if [[ ! -d "$PRE_CUT" ]]; then
    echo "creating the pre-cut worktree at $PRE_CUT ($CUT~1)..."
    git -C "$REPO" worktree add -q --detach "$PRE_CUT" "$CUT~1"
fi
if [[ ! -f "$PRE_CUT/backend/app/main.py" ]]; then
    echo "FAILED: $PRE_CUT has no app/main.py — wrong commit checked out?" >&2
    exit 2
fi
# The interpreter has to be able to IMPORT the pre-cut web tier, not just
# exist. `backend/.venv` used to satisfy that by accident — it still held
# the pre-trim 151 packages — and the first `uv sync` after the
# dependency set was trimmed to the agent's 99 removed fastapi from it,
# which turned this harness into a ModuleNotFoundError traceback. The
# check now asks the question it means.
#
# The fallback is a venv inside the worktree, built from the PRE-CUT
# `pyproject.toml` and `uv.lock`, which is the only place those 151
# packages are still described. It costs a couple of minutes once and
# then nothing, and it cannot be invalidated by work in the main
# checkout.
if ! "$PYTHON" -c "import fastapi" >/dev/null 2>&1; then
    WORKTREE_PY="$PRE_CUT/backend/.venv/bin/python"
    if ! "$WORKTREE_PY" -c "import fastapi" >/dev/null 2>&1; then
        echo "$PYTHON cannot import fastapi — syncing the pre-cut venv in the worktree..."
        if ! ( cd "$PRE_CUT/backend" && uv sync --quiet ); then
            echo "FAILED: could not create a venv with the pre-cut dependencies." >&2
            echo "The pre-cut web tier needs fastapi/fastmcp/psycopg, which the" >&2
            echo "trimmed agent-only pyproject.toml no longer installs. Point" >&2
            echo "PYTHON at an interpreter that has them." >&2
            exit 2
        fi
    fi
    PYTHON="$WORKTREE_PY"
    echo "using the worktree's own venv: $PYTHON"
fi

mkdir -p "$LOGS"

spawn() {
    local log="$1"; shift
    ( exec setsid "$@" </dev/null >"$log" 2>&1 & ) </dev/null >/dev/null 2>&1
}
port_pid() { ss -lntpH "sport = :$1" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1; }
stop_one() {
    local pid; pid="$(port_pid "$1" || true)"
    [[ -n "${pid:-}" ]] && { kill "$pid" 2>/dev/null || true; sleep 0.3; kill -9 "$pid" 2>/dev/null || true; }
    return 0
}
# Killed by the PID holding the port, never `pkill -f` on a pattern that
# also matches this shell. That mistake ends the session, memorably.
cleanup() { stop_one "$PY_PORT"; stop_one "$RS_PORT"; }
trap cleanup EXIT

wait_healthy() {
    for _ in $(seq 100); do
        curl -fsS -m 2 "http://127.0.0.1:$1/api/health" >/dev/null 2>&1 && return 0
        sleep 0.2
    done
    echo "FAILED: :$1 never became healthy — see $LOGS/$1.log" >&2
    tail -25 "$LOGS/$1.log" >&2 || true
    return 1
}

cleanup
(cd "$RS" && cargo build 2>&1 | tail -2)

echo "starting the pre-cut Python on :$PY_PORT..."
( cd "$PRE_CUT/backend" && \
  DATABASE_URL="postgresql+psycopg://cc:cc@127.0.0.1:15434/cc" PORT=$PY_PORT \
  spawn "$LOGS/$PY_PORT.log" "$PYTHON" -m uvicorn app.main:app \
      --host 127.0.0.1 --port $PY_PORT )
echo "starting the Rust tier under test on :$RS_PORT..."
( cd "$RS" && \
  DATABASE_URL="postgresql://cc:cc@127.0.0.1:15434/cc" PORT=$RS_PORT \
  STATIC_DIR="$PRE_CUT/backend/static" \
  spawn "$LOGS/$RS_PORT.log" "$RS/target/debug/sentinel-command" )

wait_healthy $PY_PORT
wait_healthy $RS_PORT

# Rate-limit counters are shared in Redis, and both tiers decorate these
# routes at 120/minute. 35 cases × 2 tiers is well inside that, but a
# second run in the same minute is not.
docker exec "$REDIS_CONTAINER" redis-cli FLUSHDB >/dev/null 2>&1 \
    && echo "flushed rate-limit counters" \
    || echo "WARNING: no redis — back-to-back runs may report 429s" >&2

echo "seeding csv_fixture.sql..."
timeout 60 docker exec -i "$PG_CONTAINER" psql -U cc -d cc -v ON_ERROR_STOP=1 -q \
    < "$HERE/csv_fixture.sql"

# A tie in any of the three sort keys makes the export order arbitrary,
# and a diff of two arbitrary orders is a flake. The fixture spaces them
# by a microsecond; this proves it still does.
TIES=$(docker exec "$PG_CONTAINER" psql -U cc -d cc -tAc "
SELECT coalesce(sum(n-1),0) FROM (
  SELECT count(*) n FROM audit_log GROUP BY org_id, timestamp HAVING count(*)>1
  UNION ALL SELECT count(*) FROM stream_access_logs GROUP BY org_id, accessed_at HAVING count(*)>1
  UNION ALL SELECT count(*) FROM mcp_activity_logs GROUP BY org_id, timestamp HAVING count(*)>1
) t" | tr -d '[:space:]')
if [[ "${TIES:-0}" != "0" ]]; then
    echo "FIXTURE DEFECT: $TIES tied sort key(s) — the export order is not" >&2
    echo "deterministic and any difference below would be a flake." >&2
    exit 2
fi

echo "minting tokens with the pre-cut Python's own issue_token()..."
TOKEN="$(cd "$PRE_CUT/backend" && env APP_SECRET_KEY="$APP_SECRET_KEY" \
    AUTH_PROVIDER=local LOCAL_ORG_ID=self-host "$PYTHON" -c "
import sys; sys.path.insert(0, '.')
from app.core.local_auth import issue_token
print(issue_token())
")"
MEMBER_TOKEN="$(env APP_SECRET_KEY="$APP_SECRET_KEY" "$PYTHON" -c "
import os, time, jwt
now = int(time.time())
print(jwt.encode({
    'sub': 'sentinel-local-auth', 'user_id': 'local-member',
    'org_id': 'self-host', 'org_role': 'org:member',
    'iat': now, 'exp': now + 3600,
}, os.environ['APP_SECRET_KEY'], algorithm='HS256'))
")"

# The diff script talks to 8000/8001 by default; point it at this pair.
echo
LAST="$RS/target/last-csv-diff.out"
mkdir -p "$(dirname "$LAST")"
CSV_RUST="http://127.0.0.1:$RS_PORT" CSV_PYTHON="http://127.0.0.1:$PY_PORT" \
    "$PYTHON" "$HERE/csv_diff.py" "$TOKEN" "$MEMBER_TOKEN" "$@" 2>&1 | tee "$LAST"
exit "${PIPESTATUS[0]}"
