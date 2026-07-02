#!/bin/bash
set -euo pipefail

# Sentinel CameraNode Installer (Sentinel by SourceBox)
#
# Default behavior: download binary, register the node, and tell the
# operator how to start the foreground TUI.  The foreground TUI is the
# recommended primary experience on every platform — same model as
# the Windows MSI's Start menu shortcut.  See README "Running as a
# Windows Service" / "Running unattended on Linux" for the explicit
# unattended-operation paths, both of which are opt-in.
#
# Usage:
#   Interactive (downloads binary, prompts for credentials):
#     curl -fsSL https://opensentry-command.fly.dev/install.sh | bash
#
#   One-shot (downloads + registers; foreground-launchable after):
#     curl -fsSL .../install.sh | bash -s -- \
#       --url   https://opensentry-command.fly.dev \
#       --node-id <node_id> \
#       --key    <api_key>
#
#   Unattended / 24/7 headless (above + register systemd service):
#     curl -fsSL .../install.sh | bash -s -- \
#       --url   <url> --node-id <id> --key <key> \
#       --install-service
#
# When --node-id and --key are passed, the script overwrites any stale
# node.db from a previous install (e.g. cargo-run testing) so the new
# credentials actually take effect.
#
# --install-service is OPT-IN: without it, the script never registers
# or starts a systemd service.  This keeps Linux consistent with the
# Windows model where foreground TUI is primary and the service is a
# deliberate second step.

# ── Colors ──────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
DIM='\033[2m'
BOLD='\033[1m'
NC='\033[0m'

REPO="SourceBox-LLC/Sentinel-CameraNode"
INSTALL_DIR="${SOURCEBOX_SENTRY_INSTALL_DIR:-$HOME/.sourcebox-sentry}"

# ── Parse credential args ──────────────────────────────────────────
# When the dashboard's "Add Node" modal generates the install command
# it now appends --url/--node-id/--key so the operator gets a single
# one-liner that does everything.  When invoked without these args
# (e.g. someone hand-typing the curl URL with no creds) the script
# falls back to its old interactive setup-wizard flow.
ARG_URL=""
ARG_NODE_ID=""
ARG_KEY=""
ARG_INSTALL_SERVICE=false

# The space-separated forms (`--flag value`) consume two tokens, but a bare
# `shift 2` trips "shift count out of range" when the flag is the trailing
# token with no value — and under `set -e` that aborts with a cryptic error
# instead of letting the all-or-nothing validation below print a friendly
# one. `shift $(( $# > 1 ? 2 : 1 ))` shifts 2 normally, 1 in that edge case.
while [ $# -gt 0 ]; do
    case "$1" in
        --url)
            ARG_URL="${2:-}"
            shift "$(( $# > 1 ? 2 : 1 ))" ;;
        --url=*)
            ARG_URL="${1#*=}"
            shift ;;
        --node-id)
            ARG_NODE_ID="${2:-}"
            shift "$(( $# > 1 ? 2 : 1 ))" ;;
        --node-id=*)
            ARG_NODE_ID="${1#*=}"
            shift ;;
        --key)
            ARG_KEY="${2:-}"
            shift "$(( $# > 1 ? 2 : 1 ))" ;;
        --key=*)
            ARG_KEY="${1#*=}"
            shift ;;
        --install-service)
            # Opt-in flag for 24/7 unattended operation.  Mirrors the
            # Windows MSI's manual-start service registration: the
            # foreground TUI is still the default, this just adds a
            # systemd unit that survives logout and reboots.
            ARG_INSTALL_SERVICE=true
            shift ;;
        *)
            # Unknown arg — ignore so future install.sh versions can
            # accept new flags from older dashboard one-liners without
            # crashing the install.
            shift ;;
    esac
done

# All-or-nothing: if any of the three are set, all three must be.
# Better to fail loud here than to half-configure and leave the
# operator wondering why setup didn't run.
HAVE_QUICK_ARGS=false
if [ -n "$ARG_URL" ] || [ -n "$ARG_NODE_ID" ] || [ -n "$ARG_KEY" ]; then
    if [ -z "$ARG_URL" ] || [ -z "$ARG_NODE_ID" ] || [ -z "$ARG_KEY" ]; then
        echo -e "${RED}Error: --url, --node-id, and --key must all be provided together.${NC}"
        echo -e "  Got: --url=${ARG_URL:-<missing>} --node-id=${ARG_NODE_ID:-<missing>} --key=${ARG_KEY:-<missing>}"
        exit 1
    fi
    HAVE_QUICK_ARGS=true
fi

# ── Banner ──────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}${BOLD}  Sentinel CameraNode Installer${NC}"
echo -e "${DIM}  ================================${NC}"
echo ""

# ── Detect platform ────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux*)  PLATFORM="linux" ;;
    Darwin*) PLATFORM="macos" ;;
    *)
        echo -e "${RED}Error: Unsupported operating system: $OS${NC}"
        echo ""
        echo "For Windows, download the MSI installer from the latest release:"
        echo "  https://github.com/SourceBox-LLC/Sentinel-CameraNode/releases/latest"
        echo ""
        echo "(Run the MSI, then 'sourcebox-sentry-cameranode setup' from an admin PowerShell.)"
        exit 1
        ;;
esac

case "$ARCH" in
    x86_64|amd64)  ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    armv7*)        ARCH="armv7" ;;
    *)
        echo -e "${RED}Error: Unsupported architecture: $ARCH${NC}"
        exit 1
        ;;
esac

echo -e "  Platform:  ${CYAN}${PLATFORM}-${ARCH}${NC}"
echo -e "  Install:   ${CYAN}${INSTALL_DIR}${NC}"
echo ""

# ── Check dependencies ─────────────────────────────────────────────
check_cmd() {
    if ! command -v "$1" &>/dev/null; then
        return 1
    fi
    return 0
}

if ! check_cmd curl; then
    echo -e "${RED}Error: curl is required but not installed.${NC}"
    exit 1
fi

# ── Shared prompt helper ───────────────────────────────────────────
# The installer is usually run via `curl | bash`, so stdin is the pipe
# from curl, not a terminal.  Reading from /dev/tty is how we still get
# an interactive yes/no from the operator — skip silently if the
# controlling terminal isn't available (CI, container build, etc.).
#
# Default answer is "yes" for every prompt the installer asks.
# Operators running this one-liner almost always want the thing we're
# about to install; making them type `y` for each step is friction
# without safety, and they can still decline.
prompt_yes() {
    local prompt_msg="$1"
    if [ ! -t 1 ] || [ ! -r /dev/tty ]; then
        # No tty — assume yes so unattended installs (cloud-init,
        # Ansible, curl | bash in Dockerfile builds) still work.
        return 0
    fi
    local reply=""
    printf "  %b " "${prompt_msg} [Y/n]:"
    read -r reply </dev/tty || reply="n"
    case "${reply:-y}" in
        y|Y|yes|YES) return 0 ;;
        *) return 1 ;;
    esac
}

# Install one or more apt packages after asking the operator.  Returns 0
# on success (or if declined), 1 on actual apt failure so callers can
# decide whether to continue or bail.  Silently no-ops on non-apt
# systems — we'd rather print a manual-install hint than try to detect
# dnf/pacman/apk and get it wrong.
apt_install_pkgs() {
    local prompt_msg="$1"
    shift
    local pkgs="$*"
    if ! check_cmd apt-get; then
        echo -e "  ${DIM}Install manually: ${CYAN}sudo apt install ${pkgs}${NC}"
        return 1
    fi
    if ! prompt_yes "${prompt_msg}"; then
        echo -e "  ${DIM}Skipped — install manually: ${CYAN}sudo apt install ${pkgs}${NC}"
        return 1
    fi
    echo -e "  ${DIM}Running: sudo apt-get install -y ${pkgs}${NC}"
    # Run `apt-get update` opportunistically — a stale cache is the #1
    # cause of "E: Unable to locate package" errors on fresh Pi images.
    # Quiet flag keeps the output from drowning out the installer banner.
    if sudo apt-get update -qq && sudo apt-get install -y $pkgs; then
        return 0
    else
        echo -e "  ${RED}apt install failed for: ${pkgs}${NC}"
        return 1
    fi
}

# ── Try downloading pre-built binary ───────────────────────────────
LATEST_URL="https://api.github.com/repos/${REPO}/releases/latest"

echo -e "${DIM}Checking for pre-built release...${NC}"

DOWNLOAD_URL=""
RELEASE_TAG=""

if RELEASE_JSON=$(curl -fsSL "$LATEST_URL" 2>/dev/null); then
    # NOTE: these pipelines end in `grep` which returns 1 if it finds nothing.
    # Under `set -o pipefail`, that exit code propagates out of the $(...)
    # and, because we're assigning to a variable, `set -e` would silently
    # abort the entire script. The `|| true` keeps us alive so we can fall
    # through to the source-build path when there's no matching binary
    # (e.g. linux-aarch64 users hitting a release that only ships x86_64).
    RELEASE_TAG=$(echo "$RELEASE_JSON" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"//;s/".*//' || true)

    # Look for matching binary in release assets
    ASSET_PATTERN="${PLATFORM}.*${ARCH}"
    DOWNLOAD_URL=$(echo "$RELEASE_JSON" | grep -o '"browser_download_url": "[^"]*'"${ASSET_PATTERN}"'[^"]*"' | head -1 | sed 's/"browser_download_url": "//;s/"$//' || true)

    if [ -z "$DOWNLOAD_URL" ] && [ -n "$RELEASE_TAG" ]; then
        echo -e "  Release ${CYAN}${RELEASE_TAG}${NC} found, but no ${CYAN}${PLATFORM}-${ARCH}${NC} binary in its assets."
    fi
fi

if [ -n "$DOWNLOAD_URL" ]; then
    echo -e "${GREEN}Found release ${RELEASE_TAG}${NC}"
    echo -e "${DIM}Downloading...${NC}"

    mkdir -p "$INSTALL_DIR"
    TMPFILE=$(mktemp)

    if curl -fsSL "$DOWNLOAD_URL" -o "$TMPFILE"; then
        # Best-effort integrity check against the release's SHA256SUMS.
        # The installers are unsigned, so a checksum is the minimum
        # supply-chain guard for a curl|bash install onto the box that
        # watches your home. Skips gracefully (does NOT abort) if the
        # release predates SHA256SUMS or no sha256 tool is present, but
        # a present-and-MISMATCHING checksum hard-fails the install.
        ASSET_NAME="${DOWNLOAD_URL##*/}"
        SUMS_URL="${DOWNLOAD_URL%/*}/SHA256SUMS"
        if SUMS=$(curl -fsSL "$SUMS_URL" 2>/dev/null) && [ -n "$SUMS" ]; then
            EXPECTED=$(echo "$SUMS" | awk -v a="$ASSET_NAME" '$2 == a {print $1}' | head -1)
            if [ -n "$EXPECTED" ]; then
                if check_cmd sha256sum; then
                    ACTUAL=$(sha256sum "$TMPFILE" | awk '{print $1}')
                elif check_cmd shasum; then
                    ACTUAL=$(shasum -a 256 "$TMPFILE" | awk '{print $1}')
                else
                    ACTUAL=""
                fi
                if [ -z "$ACTUAL" ]; then
                    echo -e "${YELLOW}No sha256 tool found — skipping checksum verification.${NC}"
                elif [ "$ACTUAL" = "$EXPECTED" ]; then
                    echo -e "${GREEN}Checksum verified.${NC}"
                else
                    echo -e "${RED}Checksum MISMATCH for ${ASSET_NAME} — refusing to install.${NC}"
                    echo -e "${RED}  expected: ${EXPECTED}${NC}"
                    echo -e "${RED}  actual:   ${ACTUAL}${NC}"
                    rm -f "$TMPFILE"
                    exit 1
                fi
            else
                echo -e "${YELLOW}No checksum entry for ${ASSET_NAME} — skipping verification.${NC}"
            fi
        else
            echo -e "${DIM}No SHA256SUMS for this release — skipping checksum verification.${NC}"
        fi

        # Detect archive type and extract
        case "$DOWNLOAD_URL" in
            *.tar.gz|*.tgz)
                tar -xzf "$TMPFILE" -C "$INSTALL_DIR"
                ;;
            *.zip)
                if check_cmd unzip; then
                    unzip -qo "$TMPFILE" -d "$INSTALL_DIR"
                else
                    echo -e "${RED}Error: unzip is required to extract this release.${NC}"
                    rm -f "$TMPFILE"
                    exit 1
                fi
                ;;
            *)
                # Assume raw binary
                cp "$TMPFILE" "$INSTALL_DIR/sourcebox-sentry-cameranode"
                ;;
        esac

        rm -f "$TMPFILE"
        chmod +x "$INSTALL_DIR/sourcebox-sentry-cameranode" 2>/dev/null || true

        echo -e "${GREEN}Downloaded successfully.${NC}"
    else
        echo -e "${YELLOW}Download failed. Falling back to source build...${NC}"
        DOWNLOAD_URL=""
    fi
fi

# ── Fall back to building from source ──────────────────────────────
#
# Pi users (aarch64 / armv7) always hit this path until we publish
# ARM binaries in the release matrix — so this needs to be a real
# "install everything for you" flow, not a "here's what to type" wall
# of text.  We auto-install the whole build toolchain via apt and
# bootstrap rustup non-interactively; the operator just presses Enter.
if [ -z "$DOWNLOAD_URL" ]; then
    echo -e "${YELLOW}No pre-built binary available. Building from source...${NC}"
    echo ""

    # ── Build toolchain (gcc / make / pkg-config / libbz2) ─────────
    # On Debian/Ubuntu/Raspberry Pi OS, `build-essential` pulls in
    # gcc/g++/make.  pkg-config and libbz2-dev are needed by transitive
    # crates (ring, bzip2-sys) — missing libbz2-dev is the most common
    # source-build failure on a fresh Pi image, so we install it up
    # front rather than waiting for cargo to fail mid-compile.
    if [ "$PLATFORM" = "linux" ] && check_cmd apt-get; then
        NEED_APT=""
        check_cmd gcc         || NEED_APT="$NEED_APT build-essential"
        check_cmd make        || NEED_APT="$NEED_APT build-essential"
        check_cmd git         || NEED_APT="$NEED_APT git"
        check_cmd pkg-config  || NEED_APT="$NEED_APT pkg-config"
        # libbz2-dev is a header-only check — no CLI to probe for — so
        # we install it unconditionally when already calling apt.  It's
        # ~90 KB and pulled in transitively by the `zip` crate's
        # `bzip2-sys`; skipping it is the most common source-build
        # failure on a fresh Pi image, 15 minutes deep into cargo.
        # (We don't need libssl-dev — CameraNode uses rustls, not OpenSSL.)
        NEED_APT="$NEED_APT libbz2-dev"
        # De-dup (build-essential may appear twice).
        NEED_APT=$(echo "$NEED_APT" | tr ' ' '\n' | awk 'NF && !seen[$0]++' | tr '\n' ' ')
        if [ -n "$(echo "$NEED_APT" | tr -d ' ')" ]; then
            echo -e "  ${DIM}Build toolchain needed: ${CYAN}${NEED_APT}${NC}"
            if ! apt_install_pkgs "Install build toolchain via apt?" $NEED_APT; then
                echo -e "${RED}Cannot build from source without these packages.${NC}"
                exit 1
            fi
        fi
    elif ! check_cmd git; then
        echo -e "${RED}git is required but not installed.${NC}"
        if [ "$PLATFORM" = "macos" ]; then
            echo -e "Install: ${CYAN}xcode-select --install${NC}"
        fi
        exit 1
    fi

    # ── Rust toolchain via rustup ──────────────────────────────────
    # If cargo is already on PATH we use whatever version the user has;
    # otherwise we install stable via the official rustup one-liner
    # (non-interactive with -y).  After install we source cargo's env
    # script so `cargo` resolves in *this* shell invocation — without
    # that the very next `cargo build` call would fail with "command
    # not found" even though rustup landed successfully.
    if ! check_cmd cargo; then
        echo ""
        echo -e "  ${BOLD}Rust toolchain not found.${NC}"
        echo -e "  ${DIM}CameraNode needs rustc + cargo to build from source.${NC}"
        if prompt_yes "Install Rust via rustup (the official installer)?"; then
            echo -e "  ${DIM}Running rustup-init -y (stable toolchain, default profile)...${NC}"
            # --default-toolchain stable: pin to stable to avoid nightly
            # surprises. --profile minimal: skip docs/clippy/rust-src we
            # don't need for a release build — saves ~300 MB on a Pi.
            if curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
                 sh -s -- -y --default-toolchain stable --profile minimal; then
                # shellcheck disable=SC1091
                . "$HOME/.cargo/env"
                echo -e "  Rust:      ${GREEN}$(rustc --version 2>&1 | awk '{print $2}') (installed)${NC}"
            else
                echo -e "${RED}rustup install failed. Re-run after installing manually.${NC}"
                exit 1
            fi
        else
            echo -e "${RED}Cannot build from source without Rust.${NC}"
            echo -e "Install: ${CYAN}curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh${NC}"
            exit 1
        fi
    fi

    mkdir -p "$INSTALL_DIR"
    CLONE_DIR="$INSTALL_DIR/source"

    if [ -d "$CLONE_DIR" ]; then
        echo -e "${DIM}Updating existing source...${NC}"
        git -C "$CLONE_DIR" pull --quiet
    else
        echo -e "${DIM}Cloning repository...${NC}"
        git clone --quiet "https://github.com/${REPO}.git" "$CLONE_DIR"
    fi

    # Build time on a Pi 4 is ~10-15 min — the `quiet` flag hides
    # cargo's per-crate progress but we print a heads-up so operators
    # don't think the terminal hung.
    echo -e "${DIM}Building (~10-15 min on Raspberry Pi 4)...${NC}"
    (cd "$CLONE_DIR" && cargo build --release --quiet)

    cp "$CLONE_DIR/target/release/sourcebox-sentry-cameranode" "$INSTALL_DIR/sourcebox-sentry-cameranode"
    chmod +x "$INSTALL_DIR/sourcebox-sentry-cameranode"

    echo -e "${GREEN}Build complete.${NC}"
fi

# ── Check for ffmpeg + v4l-utils ──────────────────────────────────
# ffmpeg is the encoder; v4l-utils gives operators `v4l2-ctl` for
# diagnosing "camera not detected" issues — tiny package, huge help
# in support threads, so we install it alongside rather than asking.
echo ""
if check_cmd ffmpeg; then
    FFMPEG_VERSION=$(ffmpeg -version 2>&1 | head -1 | awk '{print $3}')
    echo -e "  ffmpeg:    ${GREEN}${FFMPEG_VERSION} (installed)${NC}"
    # ffmpeg is present but v4l-utils might not be.  Quiet install if
    # we're on a Linux box with apt — no prompt, it's tiny and useful.
    if [ "$PLATFORM" = "linux" ] && ! check_cmd v4l2-ctl && check_cmd apt-get; then
        echo -e "  ${DIM}Installing v4l-utils for camera diagnostics...${NC}"
        sudo apt-get install -y v4l-utils >/dev/null 2>&1 || true
    fi
else
    echo -e "  ffmpeg:    ${YELLOW}not found${NC}"
    echo ""
    echo -e "${YELLOW}CameraNode requires ffmpeg for video processing.${NC}"

    if [ "$PLATFORM" = "linux" ]; then
        # apt is the common case (Debian/Ubuntu/Raspberry Pi OS).  Ship
        # v4l-utils in the same apt invocation so we only prompt once.
        # Other distros (dnf/pacman/apk) get a manual-install hint —
        # auto-detecting every package manager is more footgun than win.
        if apt_install_pkgs "Install ffmpeg + v4l-utils via apt?" ffmpeg v4l-utils; then
            echo -e "  ffmpeg:    ${GREEN}installed${NC}"
        fi
    elif [ "$PLATFORM" = "macos" ]; then
        if check_cmd brew; then
            if prompt_yes "Install ffmpeg now with Homebrew?"; then
                echo -e "  ${DIM}Running: brew install ffmpeg${NC}"
                if brew install ffmpeg; then
                    echo -e "  ffmpeg:    ${GREEN}installed${NC}"
                else
                    echo -e "  ${RED}brew install failed — install manually and re-run setup.${NC}"
                fi
            fi
        else
            echo -e "  Install:  ${CYAN}brew install ffmpeg${NC}  ${DIM}(install Homebrew first from https://brew.sh)${NC}"
        fi
    fi
fi

# ── Raspberry Pi–specific checks ──────────────────────────────────
# On a Pi the operator hits two common gotchas:
#   1. Their user isn't in the `video` group, so opening /dev/video0
#      fails with EACCES and the setup wizard silently shows zero
#      cameras detected.
#   2. `/dev/video10` (the V4L2 M2M hardware H.264 encoder) isn't
#      present, so FFmpeg's hw-encoder probe falls back to libx264.
#      On a Pi 4 that means two 720p30 streams can pin the CPU and
#      thermal-throttle into a pipeline wedge.  The supervisor now
#      auto-recovers from the wedge, but avoiding it entirely is still
#      the win.
# Both checks run only on Linux + ARM — no point nagging an x86 NUC.
if [ "$PLATFORM" = "linux" ] && { [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "armv7" ]; }; then
    IS_PI=false
    if [ -r /proc/device-tree/model ] && grep -qi "raspberry pi" /proc/device-tree/model 2>/dev/null; then
        IS_PI=true
    fi

    # `video` group membership — required to open USB webcams without
    # systemd's SupplementaryGroups rewrite.
    CURRENT_USER="${SUDO_USER:-$USER}"
    if ! id -nG "$CURRENT_USER" 2>/dev/null | tr ' ' '\n' | grep -qx video; then
        echo ""
        echo -e "  ${BOLD}User ${CURRENT_USER} is not in the 'video' group.${NC}"
        echo -e "  ${DIM}USB cameras live at /dev/video* and need this group for access.${NC}"
        if prompt_yes "Add ${CURRENT_USER} to the video group?"; then
            if sudo usermod -a -G video "$CURRENT_USER"; then
                echo -e "  ${GREEN}Added to video group.${NC}  ${DIM}Log out + back in for it to take effect${NC}"
                echo -e "  ${DIM}(or just reboot — the systemd service works without this).${NC}"
            else
                echo -e "  ${RED}usermod failed — add manually: ${CYAN}sudo usermod -a -G video ${CURRENT_USER}${NC}"
            fi
        fi
    fi

    # Hardware encoder device.  Only warn on Pi specifically — other
    # ARM SBCs (Jetson, Rock Pi, etc.) have different encoder paths
    # and our probe picks them up elsewhere.
    if [ "$IS_PI" = true ] && [ ! -e /dev/video10 ]; then
        echo ""
        echo -e "  ${YELLOW}Pi detected but /dev/video10 is missing.${NC}"
        echo -e "  ${DIM}That device is the V4L2 M2M hardware H.264 encoder. Without${NC}"
        echo -e "  ${DIM}it, FFmpeg falls back to software libx264 — fine for one${NC}"
        echo -e "  ${DIM}camera, but two 720p30 streams will thermal-throttle a Pi 4.${NC}"
        echo -e "  ${DIM}To enable: ensure ${CYAN}bcm2835-codec${NC}${DIM} kernel module is loaded${NC}"
        echo -e "  ${DIM}(default on Pi OS; may be missing on minimal / Ubuntu Server images).${NC}"
    fi
fi

# ── Detect whether the node is already registered ─────────────────
# The setup wizard writes `node.db` (SQLite) into the CWD it was run
# from — so $HOME/data/node.db is the wizard's default and the most
# common place.  If either it or an older $INSTALL_DIR/data/node.db
# exists, skip the wizard (re-running it would create a duplicate node
# entry in Command Center).  The variable IS_REGISTERED is consumed
# below by both the setup step and the systemd-unit template.
IS_REGISTERED=false
DATA_DIR=""
if [ -f "$HOME/data/node.db" ]; then
    IS_REGISTERED=true
    DATA_DIR="$HOME"
elif [ -f "$INSTALL_DIR/data/node.db" ]; then
    IS_REGISTERED=true
    DATA_DIR="$INSTALL_DIR"
fi

# When credentials were passed on the command line, treat the existing
# node.db as stale (likely from a previous cargo-run / different node)
# and force re-registration with the new creds.  Without this, an
# operator clicking "Add Node" → copying the new one-liner → running
# it on a Pi that already has a node.db would silently keep the old
# credentials and start a service that doesn't actually work — exactly
# the bug a user reported tonight.
if [ "$HAVE_QUICK_ARGS" = true ] && [ "$IS_REGISTERED" = true ]; then
    echo -e "${YELLOW}Existing node.db detected at ${DATA_DIR}/data/node.db — overwriting with new credentials.${NC}"
    # The setup binary itself handles atomic node.db replacement; we
    # don't `rm` it here, in case setup fails partway through and the
    # operator wants to fall back to the old registration manually.
    IS_REGISTERED=false
fi

# ── Run setup (non-interactive when creds passed, wizard otherwise) ─
# A brand-new install is useless without a node_id + api_key, so we
# chain setup straight onto the binary install.  Two paths now:
#
#   Quick path (--url/--node-id/--key passed on the install.sh
#   command line): run `setup --url X --node-id Y --key Z` directly.
#   No prompts, no /dev/tty needed — works in CI, cloud-init, Ansible,
#   anywhere the dashboard's one-liner gets pasted.
#
#   Interactive path (no args): run the TUI wizard from /dev/tty so
#   the operator can paste creds.  We redirect stdin from /dev/tty
#   because `curl | bash` leaves stdin as the pipe from curl — without
#   that redirect the wizard's first `read` returns EOF and every
#   prompt falls through to an empty answer.
SETUP_RAN=false
if [ "$IS_REGISTERED" = false ] && [ "$HAVE_QUICK_ARGS" = true ]; then
    # Non-interactive path.  Run from $HOME so node.db lands at the
    # canonical location the systemd unit uses as WorkingDirectory.
    echo ""
    echo -e "${BOLD}  Registering node with Command Center...${NC}"
    if (cd "$HOME" && "$INSTALL_DIR/sourcebox-sentry-cameranode" setup \
            --url "$ARG_URL" --node-id "$ARG_NODE_ID" --key "$ARG_KEY"); then
        SETUP_RAN=true
        if [ -f "$HOME/data/node.db" ]; then
            IS_REGISTERED=true
            DATA_DIR="$HOME"
        fi
        echo -e "  ${GREEN}Registered.${NC}"
    else
        echo -e "${RED}Quick setup failed — check the URL and key, then try again.${NC}"
        echo -e "  ${CYAN}cd ~ && ${INSTALL_DIR}/sourcebox-sentry-cameranode setup --url ${ARG_URL} --node-id ${ARG_NODE_ID} --key <key>${NC}"
        # Exit non-zero so the calling shell (or the dashboard's one-
        # liner copy box) can detect the failure.
        exit 1
    fi
elif [ "$IS_REGISTERED" = false ] && [ -r /dev/tty ] && [ -t 1 ]; then
    # Interactive path — the original wizard flow.
    echo ""
    echo -e "${BOLD}  Register this node with Command Center${NC}"
    echo -e "  ${DIM}We'll run the setup wizard now.  You'll need a node ID and${NC}"
    echo -e "  ${DIM}API key from ${CYAN}https://opensentry-command.fly.dev${NC}${DIM} → Nodes → Add node.${NC}"
    echo ""
    if prompt_yes "Run setup wizard now?"; then
        if (cd "$HOME" && "$INSTALL_DIR/sourcebox-sentry-cameranode" setup </dev/tty); then
            SETUP_RAN=true
            if [ -f "$HOME/data/node.db" ]; then
                IS_REGISTERED=true
                DATA_DIR="$HOME"
            fi
        else
            echo -e "  ${YELLOW}Setup wizard exited with an error.  You can re-run it later:${NC}"
            echo -e "  ${CYAN}cd ~ && ${INSTALL_DIR}/sourcebox-sentry-cameranode setup${NC}"
        fi
    else
        echo -e "  ${DIM}Skipped.  Run later:  ${CYAN}cd ~ && ${INSTALL_DIR}/sourcebox-sentry-cameranode setup${NC}"
    fi
fi

# ── Offer systemd auto-start on Linux ─────────────────────────────
# Only fires on Linux + systemd + interactive TTY. Skips on WSL, Docker,
# CI, and anywhere without /dev/tty so the one-liner stays safe to run
# in automated contexts.  Defaults to yes when registration is in
# place (we have everything we need) and to no otherwise (service
# would immediately fail on "no credentials").
install_systemd_service() {
    local svc_name="sourcebox-sentry-cameranode"
    local svc_file="/etc/systemd/system/${svc_name}.service"
    local run_user="${SUDO_USER:-$USER}"
    # WorkingDirectory must contain (or create) ./data where node.db
    # lives.  Prefer the dir where registration already sits so an
    # existing install keeps working; fall back to $HOME (wizard's
    # default) for fresh installs.
    local work_dir="${DATA_DIR:-$HOME}"

    # Render the unit file to a temp location first so we can inspect it
    # if the install step fails, and so the sudo move is the only
    # privileged action. Keeps blast-radius tiny.
    local tmp_unit
    tmp_unit=$(mktemp) || return 1

    cat >"$tmp_unit" <<UNIT
[Unit]
Description=Sentinel CameraNode
Documentation=https://opensentry-command.fly.dev
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${run_user}
# 'video' is the standard group that owns /dev/video* on Debian/Ubuntu
# /Raspberry Pi OS — the CameraNode needs it to open USB cameras.
SupplementaryGroups=video
# Inherit a sane PATH so ffmpeg (installed via apt above) is found even
# when systemd's default PATH is missing /usr/local/bin.
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
# NO_COLOR + TERM=dumb suppress the TUI's ANSI cursor escapes so
# journalctl entries stay line-oriented instead of full-screen redraws.
Environment=NO_COLOR=1
Environment=TERM=dumb
Environment=RUST_LOG=info
WorkingDirectory=${work_dir}
ExecStart=${INSTALL_DIR}/sourcebox-sentry-cameranode run
StandardOutput=journal
StandardError=journal
Restart=on-failure
RestartSec=5s
# If the service fails to start 5 times in a minute, stop retrying
# — operator needs to see the logs rather than a busy-loop hiding them.
StartLimitIntervalSec=60
StartLimitBurst=5

[Install]
WantedBy=multi-user.target
UNIT

    echo ""
    echo -e "${DIM}Installing systemd unit to ${svc_file}...${NC}"
    if ! sudo install -m 0644 "$tmp_unit" "$svc_file"; then
        rm -f "$tmp_unit"
        echo -e "  ${RED}Failed to install unit file. Skipping auto-start.${NC}"
        return 1
    fi
    rm -f "$tmp_unit"

    sudo systemctl daemon-reload
    if ! sudo systemctl enable "$svc_name" >/dev/null 2>&1; then
        echo -e "  ${YELLOW}Unit installed but enable failed. Check: systemctl status ${svc_name}${NC}"
        return 1
    fi
    echo -e "  ${GREEN}Service enabled — will start on boot.${NC}"

    # Start (or restart, if someone re-ran the installer) so the new
    # binary/unit actually takes effect.  `restart` is idempotent across
    # both "not yet running" and "was running the old binary".
    echo -e "  ${DIM}Starting service...${NC}"
    if sudo systemctl restart "$svc_name"; then
        sleep 2
        if sudo systemctl is-active --quiet "$svc_name"; then
            echo -e "  ${GREEN}Service is running.${NC}"
            echo -e "  ${DIM}Status:     ${CYAN}systemctl status ${svc_name}${NC}"
            echo -e "  ${DIM}Live logs:  ${CYAN}journalctl -u ${svc_name} -f${NC}"
            echo -e "  ${DIM}Stop:       ${CYAN}sudo systemctl stop ${svc_name}${NC}"
            SERVICE_RUNNING=true
        else
            echo -e "  ${YELLOW}Service started but not active — check:${NC}"
            echo -e "  ${CYAN}journalctl -u ${svc_name} -n 50 --no-pager${NC}"
        fi
    else
        echo -e "  ${YELLOW}Failed to start service — check:${NC}"
        echo -e "  ${CYAN}journalctl -u ${svc_name} -n 50 --no-pager${NC}"
    fi
}

SERVICE_RUNNING=false
if [ "$PLATFORM" = "linux" ] && check_cmd systemctl && [ -d /etc/systemd/system ]; then
    UNIT_EXISTS=false
    [ -f /etc/systemd/system/sourcebox-sentry-cameranode.service ] && UNIT_EXISTS=true

    # Two paths fire the systemd install:
    #
    #   1. The operator explicitly passed --install-service.  This is
    #      the deliberate "I want unattended 24/7" opt-in — same model
    #      as the Windows MSI's optional Service registration.
    #
    #   2. The unit ALREADY EXISTS from a prior install.  In that case
    #      the operator opted in last time, and re-running install.sh
    #      should refresh the unit (pick up bug fixes in the unit
    #      template) and restart the service against fresh credentials.
    #      This keeps existing systemd-using installs working without
    #      surprise-removing them on upgrade.
    #
    # Crucially: a fresh install with NO --install-service and NO
    # pre-existing unit does NOT install systemd.  Operator gets the
    # foreground-TUI start hint in the "Done" summary below.  Matches
    # the Windows pattern where the MSI registers an optional service
    # but the Start menu shortcut launches the foreground TUI.
    if [ "$IS_REGISTERED" = true ] \
        && { [ "$ARG_INSTALL_SERVICE" = true ] || [ "$UNIT_EXISTS" = true ]; }; then
        echo ""
        if [ "$UNIT_EXISTS" = true ]; then
            echo -e "${DIM}Refreshing existing systemd unit + restarting...${NC}"
        else
            echo -e "${DIM}Installing systemd service (--install-service passed)...${NC}"
        fi
        install_systemd_service || true
    fi
fi

# ── Add to PATH hint ──────────────────────────────────────────────
IN_PATH=false
case ":$PATH:" in
    *":$INSTALL_DIR:"*) IN_PATH=true ;;
esac

# ── Done ──────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}${BOLD}  CameraNode installed successfully.${NC}"
echo ""

if [ "$SERVICE_RUNNING" = true ]; then
    # Operator opted in to the systemd service (or had it from a prior
    # install).  Service is up — they're done.
    echo -e "  ${GREEN}${BOLD}CameraNode is streaming via systemd.${NC}"
    echo -e "  ${DIM}View your cameras at ${CYAN}https://opensentry-command.fly.dev${NC}"
    echo ""
    echo -e "  ${DIM}Live logs:  ${CYAN}journalctl -u sourcebox-sentry-cameranode -f${NC}"
    echo -e "  ${DIM}Stop:       ${CYAN}sudo systemctl stop sourcebox-sentry-cameranode${NC}"
elif [ "$IS_REGISTERED" = true ]; then
    # Registered but no service running — the foreground-TUI path is
    # primary.  Operator runs the binary directly and sees the live
    # dashboard with cameras, segments, and slash commands.  Same
    # model as the Windows MSI's Start menu shortcut.
    echo -e "  ${BOLD}Start CameraNode (foreground dashboard):${NC}"
    echo -e "  ${CYAN}${INSTALL_DIR}/sourcebox-sentry-cameranode${NC}"
    echo ""
    echo -e "  ${DIM}You'll see the live TUI: cameras, segments, FFmpeg state, slash${NC}"
    echo -e "  ${DIM}commands.  Close the window or Ctrl+C to stop.${NC}"
    echo ""
    echo -e "  ${BOLD}For 24/7 unattended operation${NC} ${DIM}(camera-in-a-closet, no SSH session):${NC}"
    if [ "$HAVE_QUICK_ARGS" = true ]; then
        # We have the quick-args saved, can give them the exact re-run
        # one-liner with --install-service.
        echo -e "  ${CYAN}curl -fsSL https://opensentry-command.fly.dev/install.sh | bash -s -- \\\\${NC}"
        echo -e "  ${CYAN}    --url \"${ARG_URL}\" --node-id ${ARG_NODE_ID} --key <key> \\\\${NC}"
        echo -e "  ${CYAN}    --install-service${NC}"
    else
        echo -e "  ${CYAN}curl -fsSL https://opensentry-command.fly.dev/install.sh | bash -s -- --install-service${NC}"
    fi
    echo -e "  ${DIM}Registers a systemd unit and starts it.  Verify the foreground${NC}"
    echo -e "  ${DIM}flow works first before flipping to unattended.${NC}"
else
    # Not registered yet — setup was declined or failed.
    echo -e "  ${BOLD}Next steps:${NC}"
    echo ""
    echo -e "  1. Register:         ${CYAN}cd ~ && ${INSTALL_DIR}/sourcebox-sentry-cameranode setup${NC}"
    echo -e "  2. Start streaming:  ${CYAN}${INSTALL_DIR}/sourcebox-sentry-cameranode${NC}"
    echo ""
    echo -e "  ${DIM}Get your node ID + API key at ${CYAN}https://opensentry-command.fly.dev${NC}"
fi

echo ""
if [ "$IN_PATH" = false ]; then
    echo -e "  ${DIM}Tip: Add to PATH for easier access:${NC}"
    if [ "$PLATFORM" = "macos" ]; then
        echo -e "  ${CYAN}echo 'export PATH=\"\$HOME/.sourcebox-sentry:\$PATH\"' >> ~/.zshrc${NC}"
    else
        echo -e "  ${CYAN}echo 'export PATH=\"\$HOME/.sourcebox-sentry:\$PATH\"' >> ~/.bashrc${NC}"
    fi
    echo ""
fi
