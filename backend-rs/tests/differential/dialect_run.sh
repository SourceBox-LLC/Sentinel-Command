#!/usr/bin/env bash
#
# The dialect differential: the PostgreSQL build against the SQLite
# build, on the read and write case lists the port was verified with.
#
#   :8000  target/tier-bin/sentinel-command-pg      → the test Postgres
#   :8001  target/tier-bin/sentinel-command-sqlite  → target/dialect.db
#
# The PostgreSQL build is the reference here. It was held to the Python
# tier case by case before that tier was deleted; this holds the SQLite
# build to it, which is the question a second driver raises — the same
# code, on the other database.
#
# In the output, "rust" is the PostgreSQL build and "python" is the
# SQLite build: the case lists and their reporting are reused unchanged,
# and they name their two tiers by what they used to be.
#
# Usage:
#   tests/differential/dialect_run.sh [read|write|all] [-v]
#   DIFF_ONLY=incidents tests/differential/dialect_run.sh write
#   tests/differential/dialect_run.sh start|stop     (leave the pair up)

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
RS="$REPO/backend-rs"
LOGS="${TIER_LOGS:-$RS/target/tier-logs}"
BIN="$RS/target/tier-bin"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
REDIS_CONTAINER="${REDIS_CONTAINER:-cc-redis-test}"
# The case lists sign unsubscribe tokens and webhooks with PyJWT and
# svix — test-side libraries, which the pre-cut worktree's venv has.
HARNESS_PY="${HARNESS_PY:-$RS/target/pre-cut-worktree/backend/.venv/bin/python}"
if ! "$HARNESS_PY" -c "import jwt, svix" 2>/dev/null; then
    echo "REFUSING: $HARNESS_PY cannot import jwt and svix, which the write cases sign with." >&2
    echo "Run tests/differential/csv_run.sh once (it builds that venv), or set HARNESS_PY." >&2
    exit 2
fi
export DIALECT_SQLITE_DB="${DIALECT_SQLITE_DB:-$RS/target/dialect.db}"

# shellcheck source=tier_env.sh
source "$HERE/tier_env.sh"
# tier_env.sh points these at the Python tree, which is gone.
export SCRIPTS_DIR="$REPO/scripts"
export STATIC_DIR="$RS/target/dialect-static"
mkdir -p "$LOGS" "$BIN" "$STATIC_DIR"
[[ -f "$STATIC_DIR/index.html" ]] || echo '<!doctype html><title>SPA</title>' > "$STATIC_DIR/index.html"

spawn() {
    local log="$1"; shift
    ( exec setsid "$@" </dev/null >"$log" 2>&1 & ) </dev/null >/dev/null 2>&1
}
port_pid() { ss -lntpH "sport = :$1" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1; }
stop_one() {
    local pid; pid="$(port_pid "$1" || true)"
    [[ -n "${pid:-}" ]] || return 0
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
    kill -9 "$pid" 2>/dev/null || true
}
wait_healthy() {
    for _ in $(seq 100); do
        curl -fsS -m 2 "http://127.0.0.1:$1/api/health" >/dev/null 2>&1 && return 0
        sleep 0.2
    done
    echo "FAILED: :$1 never became healthy" >&2; tail -20 "$LOGS/dialect-$1.log" >&2 || true
    return 1
}

start_pair() {
    stop_one 8000; stop_one 8001
    echo "building both drivers..."
    # One target directory; the two builds differ in a feature, so cargo
    # keeps both sets of artefacts and only the final binary's path is
    # shared — hence the copy after each.
    (cd "$RS" && cargo build 2>&1 | tail -1 && cp target/debug/sentinel-command "$BIN/sentinel-command-pg")
    (cd "$RS" && cargo build --features sqlite 2>&1 | tail -1 && cp target/debug/sentinel-command "$BIN/sentinel-command-sqlite")

    if ! curl -fsS -m 2 -X POST "$SENTINEL_LICENSE_SERVICE_URL/v1/licenses/check-in" >/dev/null 2>&1; then
        spawn "$LOGS/fake-license.log" python3 "$HERE/fake_license.py" --port 18090
        for _ in $(seq 25); do
            curl -fsS -m 1 -X POST "$SENTINEL_LICENSE_SERVICE_URL/v1/licenses/check-in" >/dev/null 2>&1 && break
            sleep 0.2
        done
    fi

    # A fresh file every time: the schema comes from the migration, the
    # rows from PostgreSQL, and nothing from the last run.
    rm -f "$DIALECT_SQLITE_DB" "$DIALECT_SQLITE_DB-wal" "$DIALECT_SQLITE_DB-shm"
    ( cd "$RS"; PORT=8000 DATABASE_URL="postgresql://cc:cc@127.0.0.1:15434/cc" \
        spawn "$LOGS/dialect-8000.log" "$BIN/sentinel-command-pg" )
    ( cd "$RS"; PORT=8001 DATABASE_URL="sqlite:///$DIALECT_SQLITE_DB" \
        spawn "$LOGS/dialect-8001.log" "$BIN/sentinel-command-sqlite" )
    wait_healthy 8000; wait_healthy 8001
}

# HS256 with the standard library: the tokens used to come from the
# Python tier's own `issue_token()`, and there is no Python tier.
mint() {
    python3 - "$1" "$2" <<'PY'
import base64, hashlib, hmac, json, os, sys, time
def b64(raw): return base64.urlsafe_b64encode(raw).rstrip(b"=")
now = int(time.time())
claims = {"sub": "sentinel-local-auth", "user_id": sys.argv[1], "org_id": "self-host",
          "org_role": sys.argv[2], "iat": now, "exp": now + 3600}
head = b64(json.dumps({"alg": "HS256", "typ": "JWT"}, separators=(",", ":")).encode())
body = b64(json.dumps(claims, separators=(",", ":")).encode())
sig = b64(hmac.new(os.environ["APP_SECRET_KEY"].encode(), head + b"." + body, hashlib.sha256).digest())
print((head + b"." + body + b"." + sig).decode())
PY
}

what="${1:-all}"; shift || true
case "$what" in
    stop) stop_one 8000; stop_one 8001; exit 0 ;;
    start) start_pair; exit 0 ;;
    read|write|mcp|ws|hls|sse|all) ;;
    *) echo "usage: $0 [read|write|mcp|ws|hls|sse|all|start|stop] [-v]" >&2; exit 2 ;;
esac

start_pair
export RUST_URL=http://127.0.0.1:8000 PYTHON_URL=http://127.0.0.1:8001
ADMIN="$(mint local-admin org:admin)"
MEMBER="$(mint local-member org:member)"
status=0

if [[ "$what" == read || "$what" == all ]]; then
    docker exec "$REDIS_CONTAINER" redis-cli FLUSHDB >/dev/null 2>&1 || true
    timeout 120 docker exec -i "$PG_CONTAINER" psql -U cc -d cc -v ON_ERROR_STOP=1 -q < "$HERE/seed_cameras.sql"
    python3 "$HERE/dialect.py" "$DIALECT_SQLITE_DB"
    echo "== read cases: postgres build (as 'rust') vs sqlite build (as 'python')"
    "$HARNESS_PY" -u "$HERE/http_diff.py" "$ADMIN" "$MEMBER" "$@" 2>&1 | tee "$RS/target/last-dialect-read.out" || status=$?
fi
if [[ "$what" == write || "$what" == all ]]; then
    docker exec "$REDIS_CONTAINER" redis-cli FLUSHDB >/dev/null 2>&1 || true
    echo "== write cases: postgres build (as 'rust') vs sqlite build (as 'python')"
    "$HARNESS_PY" -u "$HERE/write_diff.py" "$ADMIN" "$MEMBER" "$@" 2>&1 | tee "$RS/target/last-dialect-write.out" || status=$?
fi
# The rest, each a harness the port was verified with. A restart between
# them, because several compare values that live in the process rather
# than the database — the viewer-second counter, the WebSocket connect
# throttle — and one harness's leftovers read as the next one's diffs.
extra() {
    local name="$1"; shift
    [[ "$what" == "$name" || "$what" == all ]] || return 0
    [[ "$what" == all ]] && start_pair
    docker exec "$REDIS_CONTAINER" redis-cli FLUSHDB >/dev/null 2>&1 || true
    # Seeded here even though most of these reseed for themselves: the
    # SSE harness does not, and a freshly created SQLite file is EMPTY —
    # its integration key was simply absent, and six "differences" were
    # one tier answering 401 to a key the other one had.
    timeout 120 docker exec -i "$PG_CONTAINER" psql -U cc -d cc -v ON_ERROR_STOP=1 -q < "$HERE/seed_cameras.sql"
    python3 "$HERE/dialect.py" "$DIALECT_SQLITE_DB" >/dev/null
    echo "== $name: postgres build (as 'rust') vs sqlite build (as 'python')"
    "$HARNESS_PY" -u "$HERE/${name}_diff.py" "$@" 2>&1 | tee "$RS/target/last-dialect-$name.out" || status=$?
}
extra mcp "$@"
extra ws "${NODE_KEY:-test-node-key}" "$@"
extra hls "$ADMIN" "$@"
extra sse "$ADMIN" "$@"
exit "$status"
