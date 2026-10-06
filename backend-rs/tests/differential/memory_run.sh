#!/usr/bin/env bash
#
# Measure what the rewrite was justified on.
#
# The case for doing this at all was memory, not latency: Python cost ~1 ms
# of a request whose database round trip is 25, and the plan said so in
# writing. `fly.toml` still sizes the segment cache against a MEASURED
# Python figure — 239 MB RSS — with a comment saying the number stays until
# the Rust tier has been measured too, "because the failure mode of getting
# it wrong is the OOM killer taking every org's streams at once".
#
# So: both tiers, same machine, same Postgres, same load, RSS sampled at
# each step. The Python comes from a worktree of the commit before the
# deletion, the way csv_run.sh gets it.
#
# Usage: tests/differential/memory_run.sh [--segments N]
#
# What it does NOT claim: this is a laptop, not a 1 GB Fly machine, and the
# load is synthetic. It is an apples-to-apples comparison of two processes
# doing identical work, which is the thing fly.toml's comment asks for
# before anyone moves that ceiling — not a production measurement.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
RS="$REPO/backend-rs"
LOGS="${TIER_LOGS:-$RS/target/tier-logs}"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
PRE_CUT="${PRE_CUT:-$RS/target/pre-cut-worktree}"
CUT="${CUT:-2baabe6}"
SEGMENTS="${SEGMENTS:-200}"
[[ "${1:-}" == "--segments" ]] && SEGMENTS="$2"

PY_PORT=8031
RS_PORT=8030

export APP_SECRET_KEY="${APP_SECRET_KEY:-differential-test-secret-not-a-real-key}"
export AUTH_PROVIDER=local
export LOCAL_ORG_ID=self-host
export LOCAL_ADMIN_USERNAME=admin
export LOCAL_ADMIN_EMAIL=admin@example.com
export LOCAL_ADMIN_PASSWORD_HASH='$argon2id$v=19$m=65536,t=3,p=4$Ro3CVUFhr5w3hNxP8Cfe9A$8L+JCXU1z+/zs9b32O92qCPEvf8AhWmpViV5KwwLRfc'
export STATIC_DIR="$PRE_CUT/backend/static"
export SCRIPTS_DIR="$PRE_CUT/backend/scripts"
# The real production ceilings, not the harness's shrunken ones: the point
# of this run is how much memory the cache's own limits allow.
export SEGMENT_CACHE_MAX_PER_CAMERA=60
export SEGMENT_CACHE_MAX_TOTAL_BYTES=402653184
FOREVER=315360000
export OFFLINE_SWEEP_INTERVAL_SECONDS=$FOREVER
export SENTINEL_REAPER_INTERVAL_SECONDS=$FOREVER
export MOTION_DIGEST_INTERVAL_SECONDS=$FOREVER
export DISK_CHECK_INTERVAL_SECONDS=$FOREVER
export RELEASE_CACHE_REFRESH_INTERVAL_SECONDS=$FOREVER
export EMAIL_WORKER_INTERVAL_SECONDS=$FOREVER

if [[ ! -d "$PRE_CUT" ]]; then
    echo "creating the pre-cut worktree at $PRE_CUT ($CUT~1)..."
    git -C "$REPO" worktree add -q --detach "$PRE_CUT" "$CUT~1"
fi
PYTHON="${PYTHON:-$PRE_CUT/backend/.venv/bin/python}"
if ! "$PYTHON" -c "import fastapi" >/dev/null 2>&1; then
    echo "syncing the pre-cut venv in the worktree..."
    ( cd "$PRE_CUT/backend" && uv sync --quiet )
fi

mkdir -p "$LOGS"
spawn() { local log="$1"; shift; ( exec setsid "$@" </dev/null >"$log" 2>&1 & ) </dev/null >/dev/null 2>&1; }
port_pid() { ss -lntpH "sport = :$1" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1; }
stop_one() {
    local pid; pid="$(port_pid "$1" || true)"
    [[ -n "${pid:-}" ]] && { kill "$pid" 2>/dev/null || true; sleep 0.3; kill -9 "$pid" 2>/dev/null || true; }
    return 0
}
cleanup() { stop_one "$PY_PORT"; stop_one "$RS_PORT"; }
trap cleanup EXIT
wait_healthy() {
    for _ in $(seq 150); do
        curl -fsS -m 2 "http://127.0.0.1:$1/api/health" >/dev/null 2>&1 && return 0
        sleep 0.2
    done
    echo "FAILED: :$1 never became healthy — see $LOGS/$1.log" >&2
    tail -20 "$LOGS/$1.log" >&2 || true
    return 1
}

# RSS of the whole process tree, because uvicorn's is one process but a
# fair comparison must not miss a child if either stack grows one.
rss_kb() {
    local pid; pid="$(port_pid "$1")"
    [[ -z "$pid" ]] && { echo 0; return; }
    local total=0
    for p in $pid $(pgrep -P "$pid" 2>/dev/null || true); do
        local kb; kb="$(awk '/^VmRSS:/{print $2}' "/proc/$p/status" 2>/dev/null || echo 0)"
        total=$(( total + ${kb:-0} ))
    done
    echo "$total"
}
report() {
    printf "  %-26s python %6.1f MB   rust %6.1f MB\n" "$1" \
        "$(echo "$(rss_kb $PY_PORT) / 1024" | bc -l)" \
        "$(echo "$(rss_kb $RS_PORT) / 1024" | bc -l)"
}

cleanup
( cd "$RS" && cargo build --release 2>&1 | tail -1 )

echo "seeding fixtures..."
timeout 120 docker exec -i "$PG_CONTAINER" psql -U cc -d cc -v ON_ERROR_STOP=1 -q \
    < "$HERE/seed_cameras.sql"

echo "starting both tiers..."
( cd "$PRE_CUT/backend" && DATABASE_URL="postgresql+psycopg://cc:cc@127.0.0.1:15434/cc" \
  spawn "$LOGS/$PY_PORT.log" "$PYTHON" -m uvicorn app.main:app --host 127.0.0.1 --port $PY_PORT )
# The RELEASE binary, which is what ships: `lto = true` and no debug
# assertions, and a debug build's allocator behaviour is not the one being
# measured.
( cd "$RS" && DATABASE_URL="postgresql://cc:cc@127.0.0.1:15434/cc" PORT=$RS_PORT \
  spawn "$LOGS/$RS_PORT.log" "$RS/target/release/sentinel-command" )
wait_healthy $PY_PORT
wait_healthy $RS_PORT
sleep 3

echo
echo "RSS, same machine, same database, same requests:"
report "idle, just booted"

# A 300 KB segment, which is what upload_diff.py uses as a realistic one.
SEG="$(mktemp)"; trap 'rm -f "$SEG"' EXIT
head -c 307200 /dev/urandom > "$SEG"

# Both cameras must EXIST and belong to node-aaaa1111, whose key this
# pushes with. The first version of this script used `cam-live-2`, which
# the fixture does not seed: half the pushes 404'd, both tiers were
# treated identically so the comparison stood, and the summary line
# claimed twice the cached bytes it actually held. A load generator that
# can silently load nothing is the same failure as a test that can
# silently skip.
echo "  pushing $SEGMENTS segments x 2 cameras to each tier..."
for cam in cam-live cam-boundary; do
    for i in $(seq 1 "$SEGMENTS"); do
        name="segment_$(printf '%05d' "$i").ts"
        for port in $PY_PORT $RS_PORT; do
            curl -sS -o /dev/null -m 10 -X POST \
                -H "X-Node-API-Key: test-node-key" \
                -H "Content-Type: video/mp2t" \
                --data-binary "@$SEG" \
                "http://127.0.0.1:$port/api/cameras/$cam/push-segment?filename=$name" || true
        done
    done
done
report "after $SEGMENTS segments x 2"

# Proof the pushes landed, read from each tier's own cache rather than
# assumed from the curl exit codes.
for port in $PY_PORT $RS_PORT; do
    cached="$(curl -fsS -m 5 "http://127.0.0.1:$port/api/health/detailed" \
        | grep -o '"segment_cameras":[0-9]*' | cut -d: -f2)"
    printf "    :%s reports %s camera(s) in its segment cache\n" "$port" "${cached:-?}"
    if [[ "${cached:-0}" -lt 2 ]]; then
        echo "REFUSING: :$port cached fewer than the 2 cameras pushed — the load" >&2
        echo "did not land, so the figures above are measuring an idle process." >&2
        exit 2
    fi
done

echo "  serving 400 segment reads from each..."
TOKEN="$(cd "$PRE_CUT/backend" && env APP_SECRET_KEY="$APP_SECRET_KEY" \
    AUTH_PROVIDER=local LOCAL_ORG_ID=self-host "$PYTHON" -c "
import sys; sys.path.insert(0, '.')
from app.core.local_auth import issue_token
print(issue_token())
")"
for i in $(seq 1 200); do
    name="segment_$(printf '%05d' $(( (i % 60) + (SEGMENTS - 60) ))).ts"
    for port in $PY_PORT $RS_PORT; do
        curl -sS -o /dev/null -m 10 -H "Authorization: Bearer $TOKEN" \
            "http://127.0.0.1:$port/api/cameras/cam-live/segment/$name" || true
    done
done
report "after 400 reads"

echo
echo "Cache ceilings in force: SEGMENT_CACHE_MAX_PER_CAMERA=60,"
echo "SEGMENT_CACHE_MAX_TOTAL_BYTES=384 MiB. Each camera therefore keeps its"
echo "newest 60 segments and no more, so with two cameras the tiers hold"
echo "2 x 60 x 300 KB ~ 36 MB whatever \$SEGMENTS is above 60 — raising it"
echo "tests the eviction path, not the ceiling."
echo
echo "The interesting number is not the total, it is the FIXED cost: both"
echo "tiers grow by roughly the cached bytes, so the gap at idle is the gap"
echo "that matters on a 1 GB machine, and it is what fly.toml's segment-cache"
echo "comment asks for before anyone moves that ceiling."
