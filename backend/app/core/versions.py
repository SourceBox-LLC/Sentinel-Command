"""
CameraNode version compatibility checks.

CameraNode reports its build version (from `Cargo.toml`'s `version` field) on
register and heartbeat.  We use that to:

  1. Reject very old nodes that cannot speak the current wire protocol
     (returns HTTP 426 Upgrade Required so the operator knows to update).
  2. Hint to slightly-out-of-date nodes that a newer release is available,
     so the dashboard can surface a "node update available" badge.

We deliberately do NOT auto-update CameraNode — operators install it on their
own hardware and we don't want to push code to a camera in someone's home
without their consent.  This module just produces the metadata; the dashboard
turns it into a user-visible nudge.

Versions are simple `MAJOR.MINOR.PATCH` semver-style strings.  We don't need
the full semver grammar (no pre-release suffixes, no build metadata) because
CameraNode releases follow plain `X.Y.Z`.  Malformed strings (non-empty but
not parseable) sort as 0.0.0 and will usually be gated.  A *missing* field
(`None` / not sent) is special-cased as supported so legacy CameraNodes that
pre-date version reporting can still register — they're flagged with
`update_available = LATEST` so the dashboard can nudge the operator.  See
``check_node_version`` for the exact rules.
"""

import logging
import re

from .config import settings
from .release_cache import latest_node_version

logger = logging.getLogger(__name__)

_VERSION_RE = re.compile(r"^\s*v?(\d+)\.(\d+)\.(\d+)")


def parse_version(version: str | None) -> tuple[int, int, int]:
    """Parse a `MAJOR.MINOR.PATCH` string into a tuple for ordered comparison.

    Anything that doesn't match returns `(0, 0, 0)`.  Pre-release / build
    suffixes after the patch number are tolerated and ignored — `1.2.3-rc1`
    parses as `(1, 2, 3)`.  This is intentional: we only care about the
    leading numeric triple for compatibility decisions.
    """
    if not version:
        return (0, 0, 0)
    m = _VERSION_RE.match(version)
    if not m:
        return (0, 0, 0)
    return (int(m.group(1)), int(m.group(2)), int(m.group(3)))


def format_version(parts: tuple[int, int, int]) -> str:
    """Format a version tuple back to canonical `X.Y.Z` form."""
    return f"{parts[0]}.{parts[1]}.{parts[2]}"


def check_node_version(reported: str | None) -> dict:
    """Compare a node-reported version against MIN_SUPPORTED / LATEST.

    Returns a dict the caller can use to drive the response:

        {
            "reported": "0.0.9" | None,        # echoed back as-is
            "parsed": "0.0.9",                  # canonical X.Y.Z form
            "supported": bool,                  # False => return HTTP 426
            "min_supported": "0.1.0",
            "latest": "0.2.0",
            "update_available": "0.2.0" | None, # set when reported < latest
        }

    Callers should:
      - Refuse the request with HTTP 426 when `supported` is False, including
        the dict in the error body so the node knows what to download.
      - Pass `update_available` through in the success response so the node
        can log a one-line "update available" hint and the dashboard can
        flag the camera/node row.

    A missing / unparseable version is always considered ``supported`` so
    very old CameraNodes that pre-date version reporting (and so don't send
    the field at all) can still register and heartbeat — they'll just show
    up with ``update_available = LATEST`` and the dashboard will flag them.
    Once we ship a wire change that actually breaks pre-version-reporting
    nodes, bump MIN_SUPPORTED past the version that started reporting and
    the unknown-version path can be tightened to reject as well.
    """
    parsed = parse_version(reported)
    min_parts = parse_version(settings.MIN_SUPPORTED_NODE_VERSION)
    # Sourced via release_cache — prefers the freshest GitHub release
    # tag the process has cached, falls back to settings.LATEST_NODE_VERSION
    # when the cache is cold (tests, first-boot pre-refresh, GitHub outage).
    latest_parts = parse_version(latest_node_version())

    if reported:
        supported = parsed >= min_parts
    else:
        # Unknown version — tolerate, but flag as needing an update.
        supported = True

    update_available = (
        format_version(latest_parts) if parsed < latest_parts else None
    )

    return {
        "reported": reported,
        "parsed": format_version(parsed),
        "supported": supported,
        "min_supported": format_version(min_parts),
        "latest": format_version(latest_parts),
        "update_available": update_available,
    }
