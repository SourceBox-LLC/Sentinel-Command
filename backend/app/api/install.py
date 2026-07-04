"""
Install script routes for CameraNode and MCP client setup.

CameraNode install:
  Linux/macOS:  curl -fsSL https://app.sentinel-command.com/install.sh | bash
  Windows:      MSI installer from the latest GitHub release. There is no
                PowerShell one-liner — the MSI is the supported Windows
                install path. See the CameraNode README for the download
                URL pattern.

MCP client auto-setup (separate from CameraNode install — these are for
configuring Claude / Cursor / etc. to talk to this Command Center):
  Linux/macOS:  curl -fsSL <origin>/mcp-setup.sh | bash -s -- <key> <url>
  Windows:      & ([scriptblock]::Create((irm <origin>/mcp-setup.ps1))) <key> <url>

  NOTE: ``irm ... | iex -Args ...`` does NOT work — Invoke-Expression has no
  ``-Args`` parameter, so the arguments never reach the script's param block.
  Use the scriptblock pattern above instead.

Direct binary downloads:
  ``GET /downloads/{os}/{arch}`` 302-redirects to the matching asset on the
  latest GitHub release of Sentinel-CameraNode.  Gives us a stable
  vendor-controlled URL to publish in docs (no GitHub URL structure leaking
  into documentation) and a single place to later add caching/mirroring.

Rate limiting: every endpoint here is public and unauthenticated, so
they're bucketed by client IP (see ``tenant_aware_key``).  The limits
are generous enough for a human running the one-liner a few times
while troubleshooting but tight enough that a bot can't hammer the
disk.  A legitimate install hits each script exactly once.
"""

import re
from pathlib import Path

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import PlainTextResponse, RedirectResponse

from app.core.limiter import limiter
from app.core.release_cache import get_latest_release

router = APIRouter(tags=["installation"])

SCRIPTS_DIR = Path(__file__).parent.parent.parent / "scripts"

# Allowed OS/arch combos for /downloads/{os}/{arch}.  Matches what
# install.sh already supports so users don't hit a friendlier URL and
# get a different answer than the one-liner would have given them.
_ALLOWED_OS = {"linux", "macos", "windows"}
_ALLOWED_ARCH = {"x86_64", "aarch64", "armv7"}

# Note: the GitHub "latest release" cache lives in app.core.release_cache
# now — versions.check_node_version() needs the same data on every node
# heartbeat, so a single shared cache lets a refresh paid for by one
# /downloads request also satisfy thousands of subsequent heartbeats.
# See app/core/release_cache.py for the full caching policy.


def _pick_asset(release: dict, os_name: str, arch: str) -> str | None:
    """Find the release asset matching ``<os>.*<arch>`` in its filename.

    Mirrors the regex install.sh uses so both paths resolve to the same
    binary.  Per-OS asset preference:

      - Windows: ``.msi`` (real installer) > ``.zip`` (raw binary in archive).
        The MSI registers the Windows Service, lays the binary into
        ``C:\\Program Files``, and adds an Add/Remove Programs entry.
        The ``.zip`` is just the bare exe — operators who download it
        thinking it's an installer end up with no install at all.
        Until 2026-04-28 the ranker preferred ``.zip`` (treated ``.msi``
        as a "raw binary") which broke ``GET /downloads/windows/x86_64``
        for everyone using the dashboard's "Download CameraNode" link.
      - Linux/macOS: ``.tar.gz`` > raw binary. There is no installer
        format for these; the tarball is the canonical artifact.
    """
    assets = release.get("assets") or []
    pattern = re.compile(rf"{re.escape(os_name)}.*{re.escape(arch)}", re.IGNORECASE)
    is_windows = os_name.lower() == "windows"

    def rank(name: str) -> int:
        lower = name.lower()
        if is_windows:
            # On Windows, .msi is the canonical installer.  Prefer it
            # over .zip so operators get a real install (Program Files
            # + service registration), not a manual extract.
            if lower.endswith(".msi"):
                return 0
            if lower.endswith(".zip"):
                return 1
            return 2
        # Non-Windows: archive > raw binary (matches install.sh).
        if lower.endswith(".tar.gz") or lower.endswith(".tgz"):
            return 0
        if lower.endswith(".zip"):
            return 1
        return 2

    candidates = [a for a in assets if a.get("name") and pattern.search(a["name"])]
    if not candidates:
        return None

    candidates.sort(key=lambda a: rank(a["name"]))
    return candidates[0].get("browser_download_url") or None


def _read_script(filename: str) -> str:
    """Read an install script from the scripts directory."""
    script_path = SCRIPTS_DIR / filename
    return script_path.read_text(encoding="utf-8")


@router.get("/install.sh", response_class=PlainTextResponse)
@limiter.limit("30/minute")
async def install_sh(request: Request):
    """Serve the bash install script for Linux/macOS."""
    content = _read_script("install.sh")
    return PlainTextResponse(
        content=content,
        media_type="text/x-shellscript",
        headers={"Content-Disposition": "inline; filename=install.sh"},
    )


# NOTE: The PowerShell CameraNode install one-liner (`/install.ps1`)
# was removed. Windows users install via the MSI from the latest
# GitHub release — that path supports Windows Service registration,
# Add/Remove Programs integration, and the upgrade/uninstall chain
# the PS one-liner couldn't do cleanly. See CameraNode README for the
# canonical download URL.
#
# The /mcp-setup.ps1 route below is unrelated — it configures MCP
# clients (Claude / Cursor / etc.) to talk to this Command Center,
# not to install CameraNode.


# ── MCP Client Setup Scripts ─────────────────────────

# Cache-Control headers on the setup-script endpoints:
#
# These scripts get fixed in master and re-deployed frequently (we
# shipped 6 changes in one evening during the auto-setup debugging
# session).  Without an explicit Cache-Control header, browsers and
# any intervening CDN are free to cache the response indefinitely
# under their default heuristic, leaving users on the previous
# broken version after a fix lands.  A 60s max-age gives clients a
# chance to coalesce the cost (a refresh costs them ~50KB) while
# bounding the window during which a known-bad script could still
# be served from a cache.
_SETUP_SCRIPT_CACHE_HEADER = "no-cache, max-age=60"


@router.get("/mcp-setup.sh", response_class=PlainTextResponse)
@limiter.limit("30/minute")
async def mcp_setup_sh(request: Request):
    """Serve the MCP client setup script for Linux/macOS."""
    content = _read_script("mcp-setup.sh")
    return PlainTextResponse(
        content=content,
        media_type="text/x-shellscript",
        headers={
            "Content-Disposition": "inline; filename=mcp-setup.sh",
            "Cache-Control": _SETUP_SCRIPT_CACHE_HEADER,
        },
    )


@router.get("/mcp-setup.ps1", response_class=PlainTextResponse)
@limiter.limit("30/minute")
async def mcp_setup_ps1(request: Request):
    """Serve the MCP client setup script for Windows."""
    content = _read_script("mcp-setup.ps1")
    return PlainTextResponse(
        content=content,
        media_type="text/plain",
        headers={
            "Content-Disposition": "inline; filename=mcp-setup.ps1",
            "Cache-Control": _SETUP_SCRIPT_CACHE_HEADER,
        },
    )


# ── Direct binary downloads ─────────────────────────

@router.get("/downloads/{os_name}/{arch}")
@limiter.limit("60/minute")
async def download_binary(request: Request, os_name: str, arch: str):
    """Redirect to the latest CameraNode binary for ``{os_name}/{arch}``.

    Returns ``302`` pointing at the matching asset on the latest GitHub
    release.  This gives docs/users a canonical vendor URL instead of
    leaking a GitHub URL structure that could drift if we ever move
    hosting providers.

    If GitHub is unreachable or the release lacks an asset for the
    requested combo, we 404 so clients fall back to the install
    script (which already has a source-build fallback baked in).
    """
    os_key = os_name.lower()
    arch_key = arch.lower()
    if os_key not in _ALLOWED_OS:
        raise HTTPException(
            status_code=404,
            detail=f"Unsupported OS '{os_name}'. Try one of: {sorted(_ALLOWED_OS)}.",
        )
    if arch_key not in _ALLOWED_ARCH:
        raise HTTPException(
            status_code=404,
            detail=f"Unsupported arch '{arch}'. Try one of: {sorted(_ALLOWED_ARCH)}.",
        )

    release = await get_latest_release()
    if not release:
        raise HTTPException(
            status_code=503,
            detail="Release metadata unavailable. Try /install.sh on Linux/macOS, or download the MSI directly from the latest GitHub release on Windows.",
        )

    asset_url = _pick_asset(release, os_key, arch_key)
    if not asset_url:
        tag = release.get("tag_name", "latest")
        raise HTTPException(
            status_code=404,
            detail=f"No prebuilt binary for {os_key}/{arch_key} in release {tag}. Try the install script for a source fallback.",
        )

    return RedirectResponse(url=asset_url, status_code=302)
