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

# The sweeps are pushed out of the way for BOTH tiers.
#
# They are not a no-op on this fixture: it carries a stranded
# `sentinel_run`, and the reaper's five-minute tick landed inside one
# tier's window and not the other's, which the write differential
# correctly reported as a side-effect difference in `sentinel_runs`
# with no code behind it. The two tiers start seconds apart, so their
# ticks never align.
#
# What the loops DO is compared by loops_run.sh, which calls the bodies
# directly. What the HTTP differentials compare is routes, and a sweep
# firing mid-case is noise in that. Python reads all four from the
# environment; `sentinel_dispatch`'s own comment calls the reaper
# cadence "tunable down for ops or up for quieter environments", and
# this is the quietest environment there is.
export OFFLINE_SWEEP_INTERVAL_SECONDS="${OFFLINE_SWEEP_INTERVAL_SECONDS:-86400}"
export SENTINEL_REAPER_INTERVAL_SECONDS="${SENTINEL_REAPER_INTERVAL_SECONDS:-86400}"
export MOTION_DIGEST_INTERVAL_SECONDS="${MOTION_DIGEST_INTERVAL_SECONDS:-86400}"
export DISK_CHECK_INTERVAL_SECONDS="${DISK_CHECK_INTERVAL_SECONDS:-86400}"

# The scripts directory is the Python's own, resolved the way install.py
# resolves it: `Path(__file__).parent.parent.parent / "scripts"`. Both
# tiers must read the same bytes or /install.sh diffs for a reason that
# has nothing to do with the port.
export SCRIPTS_DIR="$REPO/backend/scripts"

# Python's background loops, pushed out of reach. Every one sleeps before
# its first run, so a ten-year interval means it never fires during a
# session. They write to the same tables the write differential
# snapshots, on their own timer, and only the Python tier runs them:
#
#   offline sweep (30s)   flips `online` nodes with a stale last_seen to
#                         offline and writes transition notifications —
#                         and the write fixture freezes every last_seen
#                         to January;
#   sentinel reaper (5m)  marks pending runs older than 6h, and running
#                         runs older than 20m, as errored — every seeded
#                         run qualifies.
#
# A loop firing between one tier's reseed and its snapshot is a one-case
# diff that is gone on the rerun. The reaper did exactly that: it was
# caught red-handed rewriting run ...0001 to "Abandoned — agent never
# claimed this run within 6 hours" during an unrelated integration case,
# and it is the likeliest author of an earlier sentinel_runs flake that
# never reproduced. The loops themselves are ported, and verified, with
# the background-loop slice — not by racing them here.
# A licence key, so the three states beyond "unlicensed" are reachable
# at all: without it every Sentinel route answers 402 license_required
# and the licensed paths cannot be compared. The reconcile loop's
# interval is a module constant and cannot be pushed out like the
# others, so it is pointed at fake_license.py, which answers exactly
# what the seeded licence state says — a tick mid-run then rewrites the
# same values instead of moving the gate underneath a case.
# The agent's bearer for the MCP tool surface. Set so the SHARED
# multi-tenant path is reachable at all — unset, every attempt to use
# it falls through to the key lookup and the agent allowlist is never
# exercised. Distinct from SENTINEL_AGENT_KEY, which is the run queue's.
export SENTINEL_AGENT_MCP_KEY="${SENTINEL_AGENT_MCP_KEY:-harness-agent-mcp-key}"
export SENTINEL_LICENSE_KEY="${SENTINEL_LICENSE_KEY:-harness-licence-key}"
export SENTINEL_LICENSE_SERVICE_URL="${SENTINEL_LICENSE_SERVICE_URL:-http://127.0.0.1:18090}"

# The HLS caches, shrunk so their eviction paths are reachable from a
# test at all. The real ceilings are 60 segments per camera and 384 MB
# across all of them; filling either honestly would mean pushing
# hundreds of megabytes through both tiers for one case. The policies
# are what the differential is for — which segment goes, and when — and
# those are the same at five as at sixty.
export SEGMENT_CACHE_MAX_PER_CAMERA="${SEGMENT_CACHE_MAX_PER_CAMERA:-5}"
# Three megabytes, not the real 384: small enough that four pushes fill
# it, large enough that a single realistic segment (upload_diff pushes
# 300 KB of real bytes) is not evicted the instant it lands.
export SEGMENT_CACHE_MAX_TOTAL_BYTES="${SEGMENT_CACHE_MAX_TOTAL_BYTES:-3000000}"
# Likewise the sweep cadence: every third playlist push rather than
# every twentieth.
export CLEANUP_INTERVAL="${CLEANUP_INTERVAL:-3}"


# Email on. Off is the production default and was the harness default
# too, which meant `email_enabled_for_kind` refused every kind and not
# one outbox row was ever written — so the templates, the recipient
# lookup and the unsubscribe tokens went entirely uncompared while the
# write differential reported green. The worker interval below is
# already pinned to never, so nothing is sent; the rows are simply
# enqueued, and `email_outbox` is a watched table.
export EMAIL_ENABLED="${EMAIL_ENABLED:-true}"
# Both tiers already default to this. Set explicitly because it is the
# base of every unsubscribe link, and two tiers disagreeing about it
# would look like a template difference.
export FRONTEND_URL="${FRONTEND_URL:-http://localhost:5173}"
# Where the Rust tier loads the shared .j2 templates from. Python finds
# them relative to its own package; there is one copy, and this points
# at it.
# One copy of each template, pointed at by both tiers. It moved to
# backend-rs/templates/emails when the Python tree was deleted — and the
# Rust tier now compiles them in, so this override exists for the Python
# half and for anyone diffing a template edit without a rebuild.
export EMAIL_TEMPLATES_DIR="${EMAIL_TEMPLATES_DIR:-$RS/templates/emails}"

# The same Svix secret for both tiers, so write_diff can sign one
# webhook delivery with the svix library and send it to each.
export RESEND_WEBHOOK_SECRET="${RESEND_WEBHOOK_SECRET:-whsec_aGFybmVzcy13ZWJob29rLXNlY3JldC0xMjM0NTY=}"
# Clerk's webhook secret. Only the Clerk-mode pair mounts that route at
# all — main.py registers the webhooks router under Clerk only — but it
# is exported for both so the two pairs differ by AUTH_PROVIDER and
# nothing else.
export CLERK_WEBHOOK_SECRET="${CLERK_WEBHOOK_SECRET:-whsec_Y2xlcmstaGFybmVzcy1zZWNyZXQtNjU0MzIxMDA=}"

# Placeholder Clerk keys for the Clerk-mode pair: base64 of a made-up
# Frontend API host. Nothing that pair is used for reaches Clerk.
CLERK_PK_PLACEHOLDER=pk_test_aGFybmVzcy5jbGVyay5hY2NvdW50cy5kZXYk

FOREVER=315360000
export OFFLINE_SWEEP_INTERVAL_SECONDS=$FOREVER
# The Rust tier's two HLS loops are pushed out of the way, like the
# other background loops. The viewer-usage flush is the one that
# matters: it writes org_monthly_usage, which the GDPR export reads, so
# a tick landing between the two passes of a write case would report a
# difference that is a timer rather than a port. Python's copy of this
# loop is a literal 60 seconds and cannot be stretched, and this
# comment used to call that exposure "one-sided and narrow" because the
# pending counters are only non-empty just after an HLS run.
#
# The window is narrow; the damage was not. The row Python wrote in it
# was never deleted by anything, so it sat in the database for the rest
# of the session and the next full-reset case counted it -- python 1,
# rust 0, buried in a JSON blob of per-table delete counts, reading
# exactly like a bug in the erasure path. seed_cameras.sql now clears
# `org_monthly_usage` per case, which is what actually bounds it.
# Rust's flush is covered by tests/hls_db.rs, against a real database.
export VIEWER_USAGE_FLUSH_INTERVAL_SECONDS=$FOREVER
# The eviction loop is NOT stretched: it touches only memory, and
# Python's copy of it runs every sixty seconds whatever this does.
# Stretching Rust's made the two tiers disagree about a camera that an
# earlier run had left in the cache — Python had swept it and Rust had
# not, which looked exactly like a port difference on the first push of
# a scenario.
export SEGMENT_CACHE_EVICT_INTERVAL_SECONDS="${SEGMENT_CACHE_EVICT_INTERVAL_SECONDS:-60}"
export SENTINEL_REAPER_INTERVAL_SECONDS=$FOREVER
export MOTION_DIGEST_INTERVAL_SECONDS=$FOREVER
export DISK_CHECK_INTERVAL_SECONDS=$FOREVER
export RELEASE_CACHE_REFRESH_INTERVAL_SECONDS=$FOREVER
# Every 5s it "sends" any pending email_outbox row — which the fixture
# has had only since the Resend webhook cases, so it raced the harness
# only from then on, alternating which tier's snapshot it landed in.
export EMAIL_WORKER_INTERVAL_SECONDS=$FOREVER
#
# Loops with a hardcoded interval, left running because none touches a
# watched table: the viewer-usage flush (60s, writes org_monthly_usage
# only after Python has served HLS segments), segment-cache eviction
# (60s, in memory), log cleanup (24h, sleeps first) and, under Clerk,
# the plan reconcile (hourly, sleeps first). The licence reconcile is
# handled by fake_license.py. Watch one of their tables and this list
# has to be revisited.
export STATIC_DIR="$REPO/backend/static"


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
