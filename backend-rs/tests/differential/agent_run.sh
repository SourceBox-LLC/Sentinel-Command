#!/usr/bin/env bash
#
# The agent differential: the Python Sentinel agent and the Rust one,
# each driven through the same scenarios against one Command Center and
# one scripted model, on all three provider wires.
#
# Usage:
#   tests/differential/agent_run.sh [-v] [--wire ollama|openai|anthropic]
#   tests/differential/agent_run.sh --restart-cc      (used by agent_diff.py)
#
# The Python half is the ORIGINAL agent source, untouched, run on
# `mcp==1.28.1` — the version it was written and verified against. That
# pin is not incidental. On the lockfile's `mcp` 2.2.0 the Python agent
# does not work at all: `streamable_http_client()` lost its `headers`
# keyword and `Tool.inputSchema` was renamed, so every run fails at the
# MCP connect (PYTHON_BUGS #18). A reference that cannot run is not a
# reference, and patching it forward attribute by attribute would be
# comparing the port against my own fixes. Env: REF_PYTHON, REF_AGENT_DIR.
#
# Command Center is RESTARTED before every play. Its per-org MCP rate
# limit lives in memory and the budget scenarios spend it, so without the
# restart the limit lands on whichever agent happens to run next and reads
# as a difference between them.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
RS="$REPO/backend-rs"
LOGS="${TIER_LOGS:-$RS/target/tier-logs}"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
REF_PYTHON="${REF_PYTHON:-$RS/target/ref-agent-venv/bin/python}"
# The agent source to use as the reference. Defaults to the tree's own
# backend/, falling back to the commit before it was deleted.
REF_AGENT_DIR="${REF_AGENT_DIR:-$REPO/backend}"

CC_PORT=8052; RS_PORT=8050; PY_PORT=8051; LLM_PORT=18096
QUEUE_KEY=harness-agent-queue-key
MCP_KEY=harness-agent-mcp-key
export MAX_AGENT_ITERATIONS="${MAX_AGENT_ITERATIONS:-6}"

mkdir -p "$LOGS"
spawn() { local log="$1"; shift; ( exec setsid "$@" </dev/null >"$log" 2>&1 & ) </dev/null >/dev/null 2>&1; }
port_pid() { ss -lntpH "sport = :$1" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1; }
stop_one() {
    local pid; pid="$(port_pid "$1" || true)"
    if [[ -n "${pid:-}" ]]; then
        kill "$pid" 2>/dev/null || true
        for _ in $(seq 40); do kill -0 "$pid" 2>/dev/null || break; sleep 0.05; done
        kill -9 "$pid" 2>/dev/null || true
    fi
    return 0
}
wait_up() {
    for _ in $(seq 150); do
        curl -fsS -m 2 "http://127.0.0.1:$1$2" >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    echo "FAILED: :$1 never answered $2 — see $LOGS/agent-diff-$1.log" >&2
    tail -15 "$LOGS/agent-diff-$1.log" >&2 || true
    return 1
}

start_cc() {
    stop_one $CC_PORT
    # `spawn` is a shell function, so the environment is exported in a
    # subshell rather than passed through `env`, which can only exec a file.
    ( cd "$RS"
      export DATABASE_URL="postgresql://cc:cc@127.0.0.1:15434/cc" PORT=$CC_PORT \
        AUTH_PROVIDER=local APP_SECRET_KEY=agent-differential-secret LOCAL_ORG_ID=self-host \
        SENTINEL_AGENT_KEY=$QUEUE_KEY SENTINEL_AGENT_MCP_KEY=$MCP_KEY \
        SENTINEL_LICENSE_KEY=harness-licence-key \
        SENTINEL_LICENSE_SERVICE_URL=http://127.0.0.1:18090 \
        OFFLINE_SWEEP_INTERVAL_SECONDS=315360000 SENTINEL_REAPER_INTERVAL_SECONDS=315360000 \
        MOTION_DIGEST_INTERVAL_SECONDS=315360000 EMAIL_WORKER_INTERVAL_SECONDS=315360000
      spawn "$LOGS/agent-diff-$CC_PORT.log" "$RS/target/debug/sentinel-command" )
    wait_up $CC_PORT /api/health
}

if [[ "${1:-}" == "--restart-cc" ]]; then
    start_cc
    exit 0
fi

VERBOSE=""; WIRES="ollama openai anthropic"; ONLY=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        -v) VERBOSE="-v" ;;
        --wire) WIRES="$2"; shift ;;
        --only) ONLY=(--only "$2"); shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

if [[ ! -f "$REF_AGENT_DIR/app/sentinel_agent/agent.py" ]]; then
    echo "REFUSING: no Python agent at $REF_AGENT_DIR/app/sentinel_agent." >&2
    echo "It was deleted when the agent was ported. To run this differential:" >&2
    echo "    git worktree add target/pre-agent-cut <commit before the deletion>" >&2
    echo "    REF_AGENT_DIR=target/pre-agent-cut/backend $0" >&2
    exit 2
fi
if ! "$REF_PYTHON" -c "import mcp, litellm, importlib.metadata as m; assert m.version('mcp').startswith('1.')" 2>/dev/null; then
    echo "REFUSING: $REF_PYTHON is not an environment with litellm and mcp 1.x." >&2
    echo "Build one:" >&2
    echo "    uv venv --python 3.12 target/ref-agent-venv" >&2
    echo "    VIRTUAL_ENV=target/ref-agent-venv uv pip install 'mcp==1.28.1' <the agent's dependencies>" >&2
    exit 2
fi

cleanup() { for p in $CC_PORT $RS_PORT $PY_PORT $LLM_PORT; do stop_one $p; done; }
trap cleanup EXIT
cleanup

( cd "$RS" && cargo build 2>&1 | tail -1 )
spawn "$LOGS/agent-diff-$LLM_PORT.log" python3 "$HERE/fake_llm.py" --port $LLM_PORT
start_cc

status=0
for wire in $WIRES; do
    case "$wire" in
        ollama)    MODEL="ollama_chat/fake-model"; BASE="http://127.0.0.1:$LLM_PORT" ;;
        # LiteLLM and rig both append /chat/completions to an OpenAI base.
        openai)    MODEL="openai/fake-model";      BASE="http://127.0.0.1:$LLM_PORT/v1" ;;
        anthropic) MODEL="anthropic/fake-model";   BASE="http://127.0.0.1:$LLM_PORT" ;;
        *) echo "unknown wire: $wire" >&2; exit 2 ;;
    esac
    stop_one $RS_PORT; stop_one $PY_PORT
    AGENT_ENV=(
        OPENSENTRY_API_BASE="http://127.0.0.1:$CC_PORT"
        SENTINEL_AGENT_KEY=$QUEUE_KEY OPENSENTRY_MCP_AGENT_KEY=$MCP_KEY
        LLM_MODEL="$MODEL" LLM_API_KEY=fake-key LLM_API_BASE="$BASE"
        AGENT_HOST=127.0.0.1 MAX_AGENT_ITERATIONS="$MAX_AGENT_ITERATIONS"
    )
    ( cd "$RS"; export "${AGENT_ENV[@]}" PORT=$RS_PORT
      spawn "$LOGS/agent-diff-$RS_PORT.log" "$RS/target/debug/sentinel-agent" )
    ( cd "$REF_AGENT_DIR"; export "${AGENT_ENV[@]}" PORT=$PY_PORT
      spawn "$LOGS/agent-diff-$PY_PORT.log" "$REF_PYTHON" -m app.sentinel_agent )
    wait_up $RS_PORT /health
    wait_up $PY_PORT /health

    CC_RESTART_SCRIPT="$HERE/agent_run.sh" \
        python3 "$HERE/agent_diff.py" --wire "$wire" $VERBOSE "${ONLY[@]}" || status=$?
done
exit "$status"
