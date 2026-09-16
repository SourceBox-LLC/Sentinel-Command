#!/usr/bin/env bash
#
# Start, stop and restart the two tiers the differential harnesses
# compare: Rust on :8000 and Python on :8001, over one Postgres and one
# Redis.
#
# This existed only as a pair of shell-history incantations until the
# environment drifted between them — the Rust tier was once restarted
# without SCRIPTS_DIR, and /install.sh 500'd against a Python that
# served it fine. Anything a tier needs to behave like production
# belongs here, in one place, for both.
#
# Usage:
#   tests/differential/tiers.sh start|stop|restart|restart-rust|status
#
# `restart-rust` rebuilds and restarts only the Rust tier, leaving
# Python up. That is what a mutation run wants — and it has to go
# through this script rather than an inlined popen, because a driver
# that sets its own environment drifts: one that forgot
# LOCAL_ADMIN_PASSWORD_HASH added a constant two-case background
# divergence to every mutation it scored.
#
# Redirect this script's output to a file rather than piping it: the
# spawned daemons briefly share the pipe, so `tiers.sh start | tail`
# blocks after the tiers are already up and looks like a failed start.
#
# `restart` rebuilds the Rust binary first. Processes are killed by the
# PID holding the port, never by `pkill -f` on the binary name: that
# pattern also matches the shell running it, and killing the session is
# a memorable way to learn the difference.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
RS="$REPO/backend-rs"
LOGS="${TIER_LOGS:-$RS/target/tier-logs}"

export APP_SECRET_KEY="${APP_SECRET_KEY:-differential-test-secret-not-a-real-key}"
export AUTH_PROVIDER=local
export LOCAL_ORG_ID=self-host
export LOCAL_ADMIN_USERNAME=admin
export LOCAL_ADMIN_EMAIL=admin@example.com
# Argon2 hash of "differential-password". Single-quoted on purpose: the
# PHC string starts with `$argon2id`, which a double-quoted assignment
# expands to the empty string — both tiers then report "not configured"
# and agree with each other about nothing.
export LOCAL_ADMIN_PASSWORD_HASH='$argon2id$v=19$m=65536,t=3,p=4$Ro3CVUFhr5w3hNxP8Cfe9A$8L+JCXU1z+/zs9b32O92qCPEvf8AhWmpViV5KwwLRfc'
export REDIS_URL="${REDIS_URL:-redis://127.0.0.1:16379/0}"

# The scripts directory is the Python's own, resolved the way install.py
# resolves it: `Path(__file__).parent.parent.parent / "scripts"`. Both
# tiers must read the same bytes or /install.sh diffs for a reason that
# has nothing to do with the port.
export SCRIPTS_DIR="$REPO/backend/scripts"
export STATIC_DIR="$REPO/backend/static"

port_pid() { ss -lntpH "sport = :$1" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1; }

stop_one() {
    local port="$1" pid
    pid="$(port_pid "$port" || true)"
    if [[ -n "${pid:-}" ]]; then
        kill "$pid" 2>/dev/null || true
        for _ in $(seq 50); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -9 "$pid" 2>/dev/null || true
        echo "stopped :$port (pid $pid)"
    else
        echo ":$port was not listening"
    fi
}

wait_healthy() {
    local port="$1"
    for _ in $(seq 100); do
        if curl -fsS -m 2 "http://127.0.0.1:$port/api/health" >/dev/null 2>&1; then
            echo ":$port healthy"
            return 0
        fi
        sleep 0.2
    done
    echo "FAILED: :$port never became healthy — see $LOGS" >&2
    tail -30 "$LOGS/$port.log" >&2 || true
    return 1
}

case "${1:-status}" in
    stop)
        stop_one 8000
        stop_one 8001
        ;;
    restart-rust)
        stop_one 8000
        mkdir -p "$LOGS"
        (cd "$RS" && cargo build 2>&1 | tail -3)
        (cd "$RS" && DATABASE_URL="postgresql://cc:cc@127.0.0.1:15434/cc" \
            PYTHON_UPSTREAM="http://127.0.0.1:8001" \
            setsid ./target/debug/sentinel-command </dev/null >"$LOGS/8000.log" 2>&1 &)
        wait_healthy 8000
        ;;
    start|restart)
        [[ "${1}" == "restart" ]] && { stop_one 8000; stop_one 8001; }
        mkdir -p "$LOGS"

        echo "building rust..."
        (cd "$RS" && cargo build 2>&1 | tail -3)

        # Python first: Rust proxies to it at startup and an unreachable
        # upstream makes every unported route a 502 that looks like a
        # port bug.
        # `setsid` with all three descriptors redirected, not a bare
        # `&`. A backgrounded child inherits this script's stdout, so a
        # caller piping it into `tail` waits on a pipe the daemons hold
        # open for as long as they run — the script appears to hang when
        # in fact it finished.
        (cd "$REPO/backend" && DATABASE_URL="postgresql+psycopg://cc:cc@127.0.0.1:15434/cc" \
            setsid .venv/bin/python -m uvicorn app.main:app --host 127.0.0.1 --port 8001 \
            </dev/null >"$LOGS/8001.log" 2>&1 &)
        (cd "$RS" && DATABASE_URL="postgresql://cc:cc@127.0.0.1:15434/cc" \
            PYTHON_UPSTREAM="http://127.0.0.1:8001" \
            setsid ./target/debug/sentinel-command </dev/null >"$LOGS/8000.log" 2>&1 &)

        wait_healthy 8001
        wait_healthy 8000
        ;;
    status)
        for port in 8000 8001; do
            pid="$(port_pid "$port" || true)"
            if [[ -n "${pid:-}" ]]; then printf ':%s pid %s\n' "$port" "$pid"; else printf ':%s not listening\n' "$port"; fi 
        done
        ;;
    *)
        echo "usage: $0 start|stop|restart|restart-rust|status" >&2
        exit 64
        ;;
esac
