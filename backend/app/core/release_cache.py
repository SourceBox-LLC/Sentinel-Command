"""
Latest CameraNode release cache.

The Command Center asks GitHub for "what's the newest CameraNode release"
in two places:

  1. ``app.api.install._pick_asset`` — needs the full release JSON so it
     can resolve ``/downloads/{os}/{arch}`` to the right asset URL.
  2. ``app.core.versions.check_node_version`` — needs only the version
     string so it can tell out-of-date nodes that an update is available.

Historically (1) ran its own 10-minute cache while (2) read a hardcoded
env-var (``LATEST_NODE_VERSION``) that had to be bumped on every
CameraNode release.  That was a release-checklist trip wire — easy to
forget, and when forgotten every node in the fleet silently stopped
seeing the ``update_available`` hint until someone noticed.

This module centralises the lookup:

  - One process-wide cache of the GitHub ``/releases/latest`` payload.
  - Async ``get_latest_release()`` for callers that need the full dict.
  - Sync ``latest_node_version()`` for callers (heartbeat path) that
    need just a version string and cannot block on the network.
  - ``refresh_latest_release()`` for an explicit refresh, called from
    a background lifespan task so the cache stays warm without the
    heartbeat path ever waiting on GitHub.

The env var (``settings.LATEST_NODE_VERSION``) is still honoured as a
disaster fallback when GitHub is unreachable on cold boot, so a
network blip during deploy can't make every node show "update
available to your current version".
"""

import logging
import time

import httpx

from app.core.config import settings

logger = logging.getLogger(__name__)

# GitHub repo that hosts CameraNode release artifacts.  Must match
# ``REPO`` in ``scripts/install.sh`` and the constant in ``install.py``
# so all paths agree on where binaries come from.
CAMERANODE_GH_REPO = "SourceBox-LLC/Sentinel-CameraNode"

# 10 minutes — short enough for a fresh release to reach nodes within
# one heartbeat-window of the next refresh tick, long enough to keep
# us comfortably under GitHub's 60/hour unauthenticated rate limit
# even with a few replicas all polling.
_RELEASE_TTL_S = 600

# Module-level cache.  Holds the full GitHub /releases/latest JSON so
# both the asset-URL resolver (install.py) and the version-string
# lookup (versions.py) can share it.
_cached_release: dict | None = None
_cached_at: float | None = None


def _strip_leading_v(tag: str) -> str:
    """Normalize ``v0.1.39`` → ``0.1.39``.

    GitHub release tags conventionally start with ``v`` but the rest of
    the codebase compares plain ``X.Y.Z``.  Strip here so callers don't
    each have to remember to.
    """
    return tag[1:] if tag.startswith(("v", "V")) else tag


async def get_latest_release(*, force_refresh: bool = False) -> dict | None:
    """Return the GitHub ``/releases/latest`` JSON, refreshing if stale.

    Returns ``None`` only when GitHub is unreachable AND nothing has
    ever been cached — callers should fall back gracefully.

    Use this when you need the full payload (asset list, body, etc.).
    For a plain version-string lookup with sync semantics, see
    ``latest_node_version``.
    """
    global _cached_release, _cached_at

    now = time.time()
    if (
        not force_refresh
        and _cached_release is not None
        and _cached_at is not None
        and (now - _cached_at) < _RELEASE_TTL_S
    ):
        return _cached_release

    url = f"https://api.github.com/repos/{CAMERANODE_GH_REPO}/releases/latest"
    try:
        async with httpx.AsyncClient(timeout=5.0) as client:
            resp = await client.get(
                url,
                headers={"Accept": "application/vnd.github+json"},
            )
        if resp.status_code != 200:
            logger.warning(
                "[ReleaseCache] GitHub returned %d for %s", resp.status_code, url,
            )
            # Serve stale on non-200 — fresh-but-stale beats "I forgot
            # which version exists" on the heartbeat path.
            return _cached_release
        data = resp.json()
        _cached_release = data
        _cached_at = now
        return data
    except (httpx.HTTPError, ValueError):
        logger.warning("[ReleaseCache] Failed to fetch %s", url, exc_info=True)
        return _cached_release


def latest_node_version() -> str:
    """Return the latest CameraNode version known to this process.

    Sync — never does I/O.  Resolution order:

      1. Cached release tag (with leading ``v`` stripped) if a fetch
         has succeeded since process start.
      2. ``settings.LATEST_NODE_VERSION`` — disaster fallback for cold
         boot before the background refresher has run, or for sustained
         GitHub outages.

    Step 2 is what makes this safe to call from the heartbeat path:
    even if GitHub never responds, every node still gets a well-formed
    answer.  The env var is also what keeps existing tests green —
    they monkeypatch ``settings.LATEST_NODE_VERSION`` and never
    populate the cache.
    """
    if _cached_release is not None:
        tag = _cached_release.get("tag_name")
        if tag:
            return _strip_leading_v(tag)
    return settings.LATEST_NODE_VERSION


async def refresh_latest_release() -> str | None:
    """Force-refresh the cache.  Returns the new version string or
    ``None`` if the fetch failed.

    Called from the lifespan startup hook (so the cache is warm before
    the first heartbeat) and from a periodic background task (so it
    stays warm without blocking any request path).
    """
    release = await get_latest_release(force_refresh=True)
    if release is None:
        return None
    tag = release.get("tag_name")
    return _strip_leading_v(tag) if tag else None


def _reset_cache_for_tests() -> None:
    """Test helper — clear the module cache so tests run in isolation.

    Production code never calls this.  Tests that exercise both the
    cached and fallback paths use it to flip between them deterministically.
    """
    global _cached_release, _cached_at
    _cached_release = None
    _cached_at = None
