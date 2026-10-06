#!/usr/bin/env bash
set -euo pipefail

# ─────────────────────────────────────────────────────
# Sentinel MCP Client Setup
# Automatically configure AI tools to connect to your
# Sentinel cameras via the Model Context Protocol.
# ─────────────────────────────────────────────────────

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
RED='\033[0;31m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

API_KEY="${1:-}"
SERVER_URL="${2:-}"

# Normalize: ensure trailing slash on the server URL.  FastAPI mounts
# the MCP app at "/mcp" with internal path="/", so a POST to "/mcp"
# (no slash) emits a 307 redirect to "/mcp/".  Strict-HTTPS clients
# like mcp-remote can't follow that redirect cleanly when the proxy
# emits the redirect with http:// scheme (Fly's edge upgrades it
# transparently, the client doesn't), so we hand them the
# redirect-free URL up front.
if [[ -n "$SERVER_URL" && "${SERVER_URL: -1}" != "/" ]]; then
    SERVER_URL="${SERVER_URL}/"
fi

if [[ -z "$API_KEY" || -z "$SERVER_URL" ]]; then
    echo -e "${RED}${BOLD}Error:${NC} Missing arguments"
    echo ""
    echo "Usage: bash mcp-setup.sh <api_key> <server_url>"
    echo ""
    echo "Get your command from the Sentinel MCP dashboard:"
    echo "  https://app.sentinel-command.com/mcp"
    exit 1
fi

# ── Header ────────────────────────────────────────────

echo ""
echo -e "  ${GREEN}${BOLD}Sentinel MCP Setup${NC}"
echo -e "  ${DIM}Configure AI tools to connect to your cameras${NC}"
echo ""

# ── Check for Python (needed for JSON manipulation) ───

PYTHON=""
if command -v python3 &>/dev/null; then
    PYTHON="python3"
elif command -v python &>/dev/null; then
    PYTHON="python"
else
    echo -e "${RED}Error: Python is required for JSON config editing.${NC}"
    echo "Install Python 3 and try again."
    exit 1
fi

# ── Detect MCP Clients ────────────────────────────────

# Per-client outcome tracking.  Used to compute the final summary and exit
# code — without this the script previously printed "Setup Complete" in
# green even when every selected client was skipped or failed, which lied
# to the user about what actually happened.
#
#   CONFIGURED_CLIENTS : write succeeded, AI client now wired up
#   SKIPPED_CLIENTS    : refused because the client process was running
#   FAILED_CLIENTS     : read/parse/write error, "name|reason" entries
CONFIGURED_CLIENTS=()
SKIPPED_CLIENTS=()
FAILED_CLIENTS=()

# Echoes the PID of a running matching process for the named client, or
# nothing if none. Refusing to write when the client is up avoids its own
# file-watcher stomping our write with stale in-memory state -- that's the
# bug the PS version of this script hit in the field.
# Tests can bypass by setting SOURCEBOX_SENTRY_MCP_ALLOW_RUNNING=1.
client_running_pid() {
    local name="$1"
    if [[ "${SOURCEBOX_SENTRY_MCP_ALLOW_RUNNING:-0}" == "1" ]]; then
        return 0
    fi
    local procs=""
    case "$name" in
        "Claude Code"|"Claude Desktop") procs="Claude claude" ;;
        "Cursor") procs="Cursor cursor" ;;
        "Windsurf") procs="Windsurf windsurf" ;;
        *) return 0 ;;
    esac
    for p in $procs; do
        local pid
        pid=$(pgrep -x "$p" 2>/dev/null | head -1 || true)
        if [[ -n "$pid" ]]; then
            echo "$pid"
            return 0
        fi
    done
}

# Client name, config path, detected (0=yes 1=no)
CLIENT_NAMES=()
CLIENT_CONFIGS=()
CLIENT_DETECTED=()

detect_client() {
    local name="$1"
    local config_path="$2"
    local detected=1

    # Check if config file or parent directory exists
    if [[ -f "$config_path" ]]; then
        detected=0
    elif [[ -d "$(dirname "$config_path")" ]]; then
        detected=0
    fi

    CLIENT_NAMES+=("$name")
    CLIENT_CONFIGS+=("$config_path")
    CLIENT_DETECTED+=("$detected")
}

# Claude Code
detect_client "Claude Code" "$HOME/.claude.json"

# Claude Desktop
if [[ "$(uname)" == "Darwin" ]]; then
    detect_client "Claude Desktop" "$HOME/Library/Application Support/Claude/claude_desktop_config.json"
else
    detect_client "Claude Desktop" "$HOME/.config/Claude/claude_desktop_config.json"
fi

# Cursor
detect_client "Cursor" "$HOME/.cursor/mcp.json"

# Windsurf
detect_client "Windsurf" "$HOME/.codeium/windsurf/mcp_config.json"

# ── Display Detected Clients ─────────────────────────

echo -e "  ${BOLD}MCP Clients:${NC}"
echo ""

DETECTED_COUNT=0
for i in "${!CLIENT_NAMES[@]}"; do
    if [[ "${CLIENT_DETECTED[$i]}" == "0" ]]; then
        echo -e "    ${GREEN}[$((i+1))]${NC} ${GREEN}●${NC} ${BOLD}${CLIENT_NAMES[$i]}${NC}"
        echo -e "        ${DIM}${CLIENT_CONFIGS[$i]}${NC}"
        ((DETECTED_COUNT++)) || true
    else
        echo -e "    ${DIM}[$((i+1))] ○ ${CLIENT_NAMES[$i]}${NC}"
        echo -e "        ${DIM}${CLIENT_CONFIGS[$i]} (not found)${NC}"
    fi
done

echo ""

if [[ "$DETECTED_COUNT" -eq 0 ]]; then
    echo -e "  ${YELLOW}No MCP clients detected.${NC}"
    echo -e "  ${DIM}You can still configure a client by entering its number.${NC}"
    echo ""
fi

# ── Warning: quit target apps ────────────────────────

echo -e "  ${YELLOW}${BOLD}Important:${NC}"
echo -e "  ${DIM}Quit Claude Code / Claude Desktop / Cursor / Windsurf before continuing.${NC}"
echo -e "  ${DIM}Running clients may overwrite config changes while the setup is writing.${NC}"
echo ""

# ── Prompt for Selection ──────────────────────────────

echo -e "  ${BOLD}Which clients would you like to configure?${NC}"
echo -e "  ${DIM}Enter numbers separated by commas (e.g. 1,3), 'all' for all detected, or 'q' to quit${NC}"
echo ""
# Read from the terminal, not stdin — stdin is the piped script when run via
# `curl ... | bash -s --`, so a plain `read` would immediately hit EOF.
if [[ -t 0 ]]; then
    read -rp "  > " SELECTION
else
    read -rp "  > " SELECTION </dev/tty
fi

if [[ "$SELECTION" == "q" || "$SELECTION" == "Q" ]]; then
    echo -e "\n  ${DIM}Setup cancelled.${NC}\n"
    exit 0
fi

# Parse selection
SELECTED=()
if [[ "$SELECTION" == "all" || "$SELECTION" == "ALL" ]]; then
    for i in "${!CLIENT_NAMES[@]}"; do
        if [[ "${CLIENT_DETECTED[$i]}" == "0" ]]; then
            SELECTED+=("$i")
        fi
    done
    if [[ ${#SELECTED[@]} -eq 0 ]]; then
        echo -e "\n  ${YELLOW}No detected clients to configure.${NC}\n"
        exit 0
    fi
else
    IFS=',' read -ra NUMS <<< "$SELECTION"
    for num in "${NUMS[@]}"; do
        num=$(echo "$num" | tr -d ' ')
        idx=$((num - 1))
        if [[ "$idx" -ge 0 && "$idx" -lt "${#CLIENT_NAMES[@]}" ]]; then
            SELECTED+=("$idx")
        else
            echo -e "  ${YELLOW}Skipping invalid selection: $num${NC}"
        fi
    done
fi

if [[ ${#SELECTED[@]} -eq 0 ]]; then
    echo -e "\n  ${YELLOW}No valid selections. Exiting.${NC}\n"
    exit 0
fi

echo ""

# ── Configure Selected Clients ────────────────────────

configure_client() {
    local name="$1"
    local config_path="$2"

    echo -e "  ${BLUE}Configuring ${BOLD}$name${NC}${BLUE}...${NC}"

    # Refuse to touch the config if the target client is currently running --
    # its own file-watcher will clobber our write with stale in-memory state.
    local running_pid
    running_pid=$(client_running_pid "$name")
    if [[ -n "$running_pid" ]]; then
        echo -e "    ${YELLOW}$name is currently running (pid $running_pid).${NC}"
        echo -e "    ${YELLOW}Skipping -- quit $name completely and re-run this script.${NC}"
        echo -e "    ${DIM}Your config was NOT modified.${NC}"
        echo ""
        SKIPPED_CLIENTS+=("$name")
        return
    fi

    # Create parent directory if needed
    local dir
    dir="$(dirname "$config_path")"
    if [[ ! -d "$dir" ]]; then
        mkdir -p "$dir"
        echo -e "    ${DIM}Created directory: $dir${NC}"
    fi

    # Claude Desktop's mcp-remote adapter needs Node.js.  Warn early
    # rather than letting the user discover it via a cryptic
    # "npx not found" error inside Claude Desktop after restart.
    if [[ "$name" == "Claude Desktop" ]]; then
        if ! command -v node &>/dev/null; then
            echo -e "    ${YELLOW}Note: Node.js was not found on PATH.${NC}"
            echo -e "    ${DIM}Claude Desktop uses mcp-remote (an npx package) to talk to${NC}"
            echo -e "    ${DIM}Sentinel's HTTP MCP server.  Install Node.js from${NC}"
            echo -e "    ${DIM}https://nodejs.org/ then restart Claude Desktop.${NC}"
        fi
    fi

    # Use Python to safely merge JSON. Exit codes:
    #   0 = wrote config successfully
    #   2 = existing file unparseable — we refused to overwrite
    #   other = I/O or unexpected failure
    #
    # Pass the client name as a 4th arg so the embedded Python can pick
    # the right config shape per client.
    $PYTHON - "$config_path" "$SERVER_URL" "$API_KEY" "$name" << 'PYEOF'
import json
import shutil
import sys
import os

config_path = sys.argv[1]
server_url = sys.argv[2]
api_key = sys.argv[3]
client_name = sys.argv[4]

# Read existing config. If the file exists and is non-empty but unparseable,
# ABORT instead of starting fresh — losing an existing .claude.json full of
# session state is catastrophic.
config = {}
if os.path.isfile(config_path):
    try:
        with open(config_path, "r", encoding="utf-8") as f:
            content = f.read().strip()
        if content:
            config = json.loads(content)
    except json.JSONDecodeError as e:
        print(f"    Could not parse {config_path} as JSON.", file=sys.stderr)
        print(f"    Error: {e}", file=sys.stderr)
        print("    Skipping this client to avoid overwriting your existing data.", file=sys.stderr)
        print("    Fix the file manually (or delete it) and re-run.", file=sys.stderr)
        sys.exit(2)
    except OSError as e:
        print(f"    Failed to read {config_path}: {e}", file=sys.stderr)
        sys.exit(3)

# Always back up the pre-existing config before we write anything.
if os.path.isfile(config_path):
    backup = config_path + ".bak"
    try:
        shutil.copy(config_path, backup)
        print(f"    Backed up existing config to {backup}")
    except OSError as e:
        print(f"    Warning: could not create backup at {backup}: {e}", file=sys.stderr)

# Ensure mcpServers exists (preserving any other entries already there).
if not isinstance(config.get("mcpServers"), dict):
    config["mcpServers"] = {}

# Build the right config shape for this specific client.  Each MCP
# client speaks a slightly different config schema and they are NOT
# interchangeable -- writing the wrong shape silently produces an
# entry the client refuses to load.
#
# Original bug: the script wrote {"type":"http", "url", "headers"} to
# every client.  That shape is correct for Claude Code (which speaks
# streamable HTTP MCP natively) but Claude Desktop's MCP loader
# rejects it with "not valid MCP server configurations and were
# skipped: sentinel".  Claude Desktop only loads stdio servers;
# remote HTTP MCP servers need to be wrapped with the `mcp-remote`
# adapter (npx package) which fronts the HTTP server as a local
# stdio process.  Cursor accepts {"url", "headers"} (no "type" field).
# Windsurf uses "serverUrl" instead of "url".
if client_name == "Claude Code":
    sentinel_config = {
        "type": "http",
        "url": server_url,
        "headers": {"Authorization": f"Bearer {api_key}"},
    }
elif client_name == "Claude Desktop":
    sentinel_config = {
        "command": "npx",
        "args": [
            "-y", "mcp-remote", server_url,
            "--header", f"Authorization:Bearer {api_key}",
        ],
    }
elif client_name == "Cursor":
    sentinel_config = {
        "url": server_url,
        "headers": {"Authorization": f"Bearer {api_key}"},
    }
elif client_name == "Windsurf":
    sentinel_config = {
        "serverUrl": server_url,
        "headers": {"Authorization": f"Bearer {api_key}"},
    }
else:
    sentinel_config = {
        "type": "http",
        "url": server_url,
        "headers": {"Authorization": f"Bearer {api_key}"},
    }

config["mcpServers"]["sentinel"] = sentinel_config

# Write back atomically — write to a tempfile in the same dir, then rename.
# Avoids the "half-written config on crash" failure mode.
import tempfile
dir_ = os.path.dirname(config_path) or "."
fd, tmp = tempfile.mkstemp(prefix=".mcp-setup-", suffix=".json", dir=dir_)
try:
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(config, f, indent=2)
        f.write("\n")
    os.replace(tmp, config_path)
except Exception as e:
    try:
        os.unlink(tmp)
    except OSError:
        pass
    print(f"    Failed to write {config_path}: {e}", file=sys.stderr)
    sys.exit(4)

print("    OK")
PYEOF
    rc=$?

    if [[ $rc -eq 0 ]]; then
        echo -e "    ${GREEN}Done${NC} ${DIM}→ $config_path${NC}"
        CONFIGURED_CLIENTS+=("$name")
    elif [[ $rc -eq 2 ]]; then
        # Parse-failure abort message is already on stderr above — just note the skip.
        echo -e "    ${YELLOW}Skipped $name (existing config not valid JSON).${NC}"
        FAILED_CLIENTS+=("$name|existing config is not valid JSON")
    else
        echo -e "    ${RED}Failed to configure $name (exit $rc)${NC}"
        FAILED_CLIENTS+=("$name|I/O error during read or write (exit $rc)")
    fi
    echo ""
}

for idx in "${SELECTED[@]}"; do
    configure_client "${CLIENT_NAMES[$idx]}" "${CLIENT_CONFIGS[$idx]}"
done

# Wait for the user before exiting so a terminal launched just to run
# this script doesn't slam shut on them mid-summary.  Skip when:
#   - SOURCEBOX_SENTRY_MCP_NO_PAUSE=1 (CI / scripted callers)
#   - We're not attached to a tty (curl|bash | piped output)
wait_for_exit_key() {
    if [[ "${SOURCEBOX_SENTRY_MCP_NO_PAUSE:-0}" == "1" ]]; then return; fi
    # If neither stdin nor /dev/tty is usable, we're being piped or run
    # under a context with no controlling terminal — don't block forever.
    if [[ ! -t 0 ]] && [[ ! -e /dev/tty ]]; then return; fi
    echo ""
    echo -e "  ${DIM}Press Enter to close...${NC}"
    if [[ -t 0 ]]; then
        read -r _ || true
    else
        read -r _ </dev/tty || true
    fi
}

# ── Summary ───────────────────────────────────────────
#
# Three possible end-states; pick the right banner + exit code for each.
# Previously the script always said "Setup Complete" in green even when
# every selected client got skipped (because the client was running) --
# the user thought their AI tools were wired up when nothing changed.

CONFIGURED_COUNT=${#CONFIGURED_CLIENTS[@]}
SKIPPED_COUNT=${#SKIPPED_CLIENTS[@]}
FAILED_COUNT=${#FAILED_CLIENTS[@]}
TOTAL_COUNT=$((CONFIGURED_COUNT + SKIPPED_COUNT + FAILED_COUNT))

# Per-bucket details first so the user sees what happened before the banner.
if [[ $CONFIGURED_COUNT -gt 0 ]]; then
    echo -e "  ${GREEN}${BOLD}Configured:${NC}"
    for c in "${CONFIGURED_CLIENTS[@]}"; do
        echo -e "    ${GREEN}* $c${NC}"
    done
    echo ""
fi

if [[ $SKIPPED_COUNT -gt 0 ]]; then
    echo -e "  ${YELLOW}${BOLD}Skipped (still running):${NC}"
    for s in "${SKIPPED_CLIENTS[@]}"; do
        echo -e "    ${YELLOW}* $s${NC}"
    done
    echo ""
    echo -e "  ${DIM}Quit the above and re-run this script to configure them.${NC}"
    echo ""
fi

if [[ $FAILED_COUNT -gt 0 ]]; then
    echo -e "  ${RED}${BOLD}Failed:${NC}"
    for f in "${FAILED_CLIENTS[@]}"; do
        # Entries are stored as "name|reason" so we can show both.
        name="${f%%|*}"
        reason="${f#*|}"
        echo -e "    ${RED}* $name -- $reason${NC}"
    done
    echo ""
fi

# Final banner + exit code.
DASHBOARD_URL="${SERVER_URL%/mcp}/mcp"

if [[ $CONFIGURED_COUNT -eq 0 ]]; then
    # Nothing was configured.  Could be 100% skipped, 100% failed, or a
    # mix -- either way the script did not do its job and the exit code
    # has to reflect that for any non-interactive caller.
    echo -e "  ${RED}${BOLD}Setup did NOT complete${NC}"
    echo ""
    echo -e "  ${DIM}No clients were configured.${NC}"
    if [[ $SKIPPED_COUNT -gt 0 ]]; then
        echo -e "  ${DIM}Quit the running clients listed above and re-run.${NC}"
    fi
    echo ""
    echo -e "  ${DIM}Manage your MCP keys at:${NC}"
    echo -e "  ${CYAN}${DASHBOARD_URL}${NC}"
    echo ""
    wait_for_exit_key
    exit 1
elif [[ $SKIPPED_COUNT -gt 0 || $FAILED_COUNT -gt 0 ]]; then
    # At least one configured, but at least one didn't.  Don't claim
    # green-checkmark success; tell the user clearly that some clients
    # need a re-run.
    echo -e "  ${YELLOW}${BOLD}Setup completed with warnings${NC}"
    echo ""
    echo -e "  ${DIM}${CONFIGURED_COUNT} of ${TOTAL_COUNT} clients configured.${NC}"
    echo -e "  ${DIM}Restart the configured clients so they pick up the new MCP server.${NC}"
    echo ""
    echo -e "  ${DIM}Manage your MCP keys at:${NC}"
    echo -e "  ${CYAN}${DASHBOARD_URL}${NC}"
    echo ""
    # Exit 0 -- the script did configure something, the user just needs
    # a follow-up run for the rest.  Use the warning banner to make that
    # visible without breaking shell pipelines that key off exit codes.
    wait_for_exit_key
    exit 0
else
    # Clean success.
    echo -e "  ${GREEN}${BOLD}Setup Complete${NC}"
    echo ""
    echo -e "  ${DIM}Your AI tools can now access your Sentinel cameras.${NC}"
    echo -e "  ${DIM}Restart the clients you configured so they pick up the new MCP server.${NC}"
    echo -e "  ${DIM}Try asking: \"List my cameras\" or \"Show me what the front door sees\"${NC}"
    echo ""
    echo -e "  ${DIM}Manage your MCP keys at:${NC}"
    echo -e "  ${CYAN}${DASHBOARD_URL}${NC}"
    echo ""
    wait_for_exit_key
    exit 0
fi
