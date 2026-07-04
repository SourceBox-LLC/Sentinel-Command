"""
RFC 9116 ``security.txt`` and friends served from ``/.well-known/``.

Why a route instead of a static file:
  - RFC 9116 requires an ``Expires:`` field with a date no more than
    one year in the future.  A static file would silently rot — six
    months after deploy the export is "still valid" but the next
    person to bump it forgets, and a year later we're serving an
    expired file that scanners flag as broken.  Generating it on
    each request rolls the expiry forward automatically.
  - ``Canonical:`` should point at the public URL we're served from
    (RFC 9116 §2.5.2 — defends against a copy of the file being
    served from another domain).  A route can compose it from the
    configured ``FRONTEND_URL`` rather than hardcoding the prod
    hostname into the static file.

Public endpoint — no auth, no rate limit beyond the global SPA
middleware.  Designed for automated scanners (security-research
firms, bug-bounty platforms) that grep for it.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta

from fastapi import APIRouter
from fastapi.responses import PlainTextResponse

from app.core.config import settings

router = APIRouter()


# ── Configuration ──────────────────────────────────────────────────
#
# Single contact channel: GitHub Security Advisories.  Standard for
# OSS projects, structured private triage workflow, supports the
# CVE process if one is warranted, and works today without us
# needing to set up DNS/MX for a security@ mailbox.
#
# An email fallback was published here briefly but pulled because
# the domain isn't provisioned yet — a bounced reporter is worse
# than no email channel at all.  Add one back when MX is live for
# sourceboxsentry.com (likely security@ or notifications@).

_PRIMARY_CONTACT = "https://github.com/SourceBox-LLC/Sentinel-Command/security/advisories/new"

# Expiry window — RFC 9116 §2.5.5 says ≤ 1 year from generation.
# We use ~11 months to give ourselves a comfortable buffer; the
# value is recomputed every request so it never goes stale at rest.
_EXPIRY_DAYS = 330


def _build_canonical_url() -> str:
    """Return the public URL this file is canonical at.

    Strips any trailing slash from FRONTEND_URL so the result is the
    well-formed ``<origin>/.well-known/security.txt``.  In dev where
    ``FRONTEND_URL`` is ``http://localhost:5173`` the canonical URL
    won't match the actual served origin — that's fine; the field is
    only operationally meaningful in production.
    """
    base = (settings.FRONTEND_URL or "").rstrip("/")
    return f"{base}/.well-known/security.txt"


def _build_security_txt() -> str:
    """Render the RFC 9116 file contents."""
    expires = (
        datetime.now(tz=UTC) + timedelta(days=_EXPIRY_DAYS)
    ).strftime("%Y-%m-%dT%H:%M:%SZ")

    # The security policy page now lives on the standalone website
    # (sentinel-command.com), not this app's frontend.  The Policy: URL
    # must point there so researchers land on the actual disclosure page.
    policy_url = "https://sentinel-command.com/security#vulnerability-disclosure"

    # Order follows RFC 9116 §2.5 examples for readability.  Comments
    # at the top help human readers; scanners ignore them.
    lines = [
        "# Sentinel by SourceBox -- security contact information (RFC 9116).",
        "# Public report channel + acknowledgement window for security",
        "# researchers.  See the policy URL for in-scope/out-of-scope",
        "# and our coordinated-disclosure expectations.",
        "",
        f"Contact: {_PRIMARY_CONTACT}",
        f"Expires: {expires}",
        f"Canonical: {_build_canonical_url()}",
        f"Policy: {policy_url}",
        "Preferred-Languages: en",
        "",
    ]
    return "\n".join(lines)


# ── Endpoints ──────────────────────────────────────────────────────
#
# RFC 9116 §3 requires the file at ``/.well-known/security.txt``.
# The historical ``/security.txt`` location is also served as an
# alias because some legacy scanners only check the root path.

@router.get("/.well-known/security.txt", response_class=PlainTextResponse)
async def well_known_security_txt() -> PlainTextResponse:
    """RFC 9116 — canonical location."""
    return PlainTextResponse(
        _build_security_txt(),
        media_type="text/plain; charset=utf-8",
        headers={
            # Short cache so the daily-rolling Expires actually rolls.
            # 1 hour gives CDNs / scanners deduping behaviour without
            # serving last week's expiry value indefinitely.
            "Cache-Control": "public, max-age=3600",
        },
    )


@router.get("/security.txt", response_class=PlainTextResponse)
async def root_security_txt() -> PlainTextResponse:
    """Legacy alias — predates RFC 9116's ``/.well-known/`` requirement.

    Kept because some older scanners and scripts only check the root
    path.  Returns the identical content as the ``/.well-known/``
    endpoint so reporters land on the same contact info regardless
    of which URL their tooling probes.
    """
    return await well_known_security_txt()
