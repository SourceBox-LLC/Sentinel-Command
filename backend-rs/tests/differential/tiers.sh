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
# `restart-rust` rebuilds only the Rust binary — that is what a mutation
# run wants, and it has to go through this script rather than an inlined
# popen, because a driver that sets its own environment drifts: one that
# forgot LOCAL_ADMIN_PASSWORD_HASH added a constant two-case background
# divergence to every mutation it scored.
#
# It restarts *both* processes even so. Some compared values are a
# function of process lifetime, not of the database — the viewer-second
# counter behind `GET /api/nodes/plan` is the one that caught this:
# it lives in memory, no reseed clears it, and the HLS harness fills it.
# Bouncing one tier alone zeroes that tier's counter and leaves the
# other's, which is eleven differing plan cases that have nothing to do
# with the code under test. Restart both, or neither. Python's restart
# is a couple of seconds; the rebuild, which is the expensive half,
# stays Rust-only.
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
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"

# The Python tier this script starts was deleted in 2baabe6. Every
# harness that needs :8001 stops here rather than starting Rust, waiting
# 20 seconds for a health check that cannot pass, and reporting a tail of
# uvicorn's ModuleNotFoundError as though the tier had crashed.
#
# csv_run.sh is the way to get a Python tier back: it serves one from a
# worktree of the commit before the cut. Anything here could be adapted
# the same way; what it cannot do is start a tier from source that is not
# in the tree.
# `stop` and `status` still work: both are about processes that may be
# running, not about starting one.
case "${1:-status}" in stop|status) NEEDS_PYTHON=0 ;; *) NEEDS_PYTHON=1 ;; esac
if [[ ! -f "$REPO/backend/app/main.py" && "$NEEDS_PYTHON" == 1 ]]; then
    echo "REFUSING: the Python web tier was deleted in 2baabe6, so there is" >&2
    echo "nothing for this script to start on :8001." >&2
    echo >&2
    echo "For a differential against the pre-cut Python:" >&2
    echo "    tests/differential/csv_run.sh        # the worked example" >&2
    echo "See tests/differential/README.md, 'After the cut'." >&2
    exit 2
fi

# The environment both tiers share lives in tier_env.sh, so that another
# launcher (dialect_run.sh) starts its tiers with exactly the same one.
# shellcheck source=tier_env.sh
source "$HERE/tier_env.sh"


# Start a daemon with NO path back to this script's stdout.
#
# `( cmd >log 2>&1 & )` is not enough, and the reason took a diagnostic
# to find: the daemon's own descriptors were clean, but the forked
# *subshell* outlived the script while still holding the caller's
# stdout pipe. Anything reading that pipe — `tiers.sh start | tail`, or
# subprocess.run(capture_output=True) — then waits forever on a pipe
# nobody will write to, long after the tiers are up and healthy. The
# outer redirection closes that path.
spawn() {
    local log="$1"; shift
    ( exec setsid "$@" </dev/null >"$log" 2>&1 & ) </dev/null >/dev/null 2>&1
}

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
        stop_one 8001
        mkdir -p "$LOGS"
        (cd "$RS" && cargo build 2>&1 | tail -3)
        # Python first: Rust proxies to it at startup.
        cd "$REPO/backend"
        DATABASE_URL="postgresql+psycopg://cc:cc@127.0.0.1:15434/cc" \
            spawn "$LOGS/8001.log" "$PYTHON" -m uvicorn \
            app.main:app --host 127.0.0.1 --port 8001
        cd "$RS"
        DATABASE_URL="postgresql://cc:cc@127.0.0.1:15434/cc" \
            PYTHON_UPSTREAM="http://127.0.0.1:8001" \
            spawn "$LOGS/8000.log" "$RS/target/debug/sentinel-command"
        wait_healthy 8001
        wait_healthy 8000
        ;;
    start|restart)
        [[ "${1}" == "restart" ]] && { stop_one 8000; stop_one 8001; }
        mkdir -p "$LOGS"

        # The fake licence service, if it is not already listening.
        if ! curl -fsS -m 2 -X POST "$SENTINEL_LICENSE_SERVICE_URL/v1/licenses/check-in" >/dev/null 2>&1; then
            spawn "$LOGS/fake-license.log" "$PYTHON" "$HERE/fake_license.py" --port 18090
            for _ in $(seq 25); do
                curl -fsS -m 1 -X POST "$SENTINEL_LICENSE_SERVICE_URL/v1/licenses/check-in" \
                    >/dev/null 2>&1 && break
                sleep 0.2
            done
        fi

        echo "building rust..."
        (cd "$RS" && cargo build 2>&1 | tail -3)

        # Python first: Rust proxies to it at startup and an unreachable
        # upstream makes every unported route a 502 that looks like a
        # port bug.
        cd "$REPO/backend"
        DATABASE_URL="postgresql+psycopg://cc:cc@127.0.0.1:15434/cc" \
            spawn "$LOGS/8001.log" "$REPO/backend/.venv/bin/python" -m uvicorn \
            app.main:app --host 127.0.0.1 --port 8001
        cd "$RS"
        DATABASE_URL="postgresql://cc:cc@127.0.0.1:15434/cc" \
            PYTHON_UPSTREAM="http://127.0.0.1:8001" \
            spawn "$LOGS/8000.log" "$RS/target/debug/sentinel-command"

        wait_healthy 8001
        wait_healthy 8000
        ;;
    restart-rust-clerk)
        # The Clerk-mode pair, rebuilding Rust only. Both processes are
        # bounced for the reason given above `restart-rust`.
        stop_one 8100
        stop_one 8101
        mkdir -p "$LOGS"
        (cd "$RS" && cargo build 2>&1 | tail -1)
        export AUTH_PROVIDER=clerk
        export CLERK_SECRET_KEY=sk_test_harness_placeholder
        export CLERK_PUBLISHABLE_KEY="$CLERK_PK_PLACEHOLDER"
        cd "$REPO/backend"
        DATABASE_URL="postgresql+psycopg://cc:cc@127.0.0.1:15434/cc" \
            spawn "$LOGS/8101.log" "$PYTHON" -m uvicorn app.main:app --host 127.0.0.1 --port 8101
        cd "$RS"
        DATABASE_URL="postgresql://cc:cc@127.0.0.1:15434/cc" \
            PYTHON_UPSTREAM="http://127.0.0.1:8101" PORT=8100 \
            spawn "$LOGS/8100.log" "$RS/target/debug/sentinel-command"
        wait_healthy 8101
        wait_healthy 8100
        ;;
    start-clerk|stop-clerk)
        # A second pair in Clerk mode, on 8100 (Rust) and 8101 (Python),
        # for behaviour that exists only there. main.py mounts the
        # webhook router only under Clerk, so a local-auth pair can
        # confirm the path is absent but never exercise it. Placeholder
        # keys: nothing these tiers are used for reaches Clerk, and the
        # Rust JWKS cache is fetched lazily. The two pairs share the
        # database, so run their harnesses one at a time.
        stop_one 8100
        stop_one 8101
        [[ "$1" == "stop-clerk" ]] && exit 0
        mkdir -p "$LOGS"
        (cd "$RS" && cargo build 2>&1 | tail -1)
        export AUTH_PROVIDER=clerk
        export CLERK_SECRET_KEY=sk_test_harness_placeholder
        export CLERK_PUBLISHABLE_KEY="$CLERK_PK_PLACEHOLDER"
        cd "$REPO/backend"
        DATABASE_URL="postgresql+psycopg://cc:cc@127.0.0.1:15434/cc" \
            spawn "$LOGS/8101.log" "$PYTHON" -m uvicorn app.main:app --host 127.0.0.1 --port 8101
        cd "$RS"
        DATABASE_URL="postgresql://cc:cc@127.0.0.1:15434/cc" \
            PYTHON_UPSTREAM="http://127.0.0.1:8101" PORT=8100 \
            spawn "$LOGS/8100.log" "$RS/target/debug/sentinel-command"
        wait_healthy 8101
        wait_healthy 8100
        ;;
    status)
        for port in 8000 8001; do
            pid="$(port_pid "$port" || true)"
            if [[ -n "${pid:-}" ]]; then printf ':%s pid %s\n' "$port" "$pid"; else printf ':%s not listening\n' "$port"; fi 
        done
        ;;
    *)
        echo "usage: $0 start|stop|restart|restart-rust|start-clerk|stop-clerk|status" >&2
        exit 64
        ;;
esac
