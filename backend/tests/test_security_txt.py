"""
Tests for the RFC 9116 ``/.well-known/security.txt`` endpoint.

Pinned invariants:

  1. The endpoint serves at the canonical RFC location AND at the
     legacy ``/security.txt`` alias.  Some scanners only check the
     root path; supporting both means we don't get spurious
     "site has no security contact" findings.
  2. Response is ``text/plain; charset=utf-8`` so a browser renders
     it as text rather than offering to download.
  3. ``Contact:`` field is present (RFC requires ≥ 1).  Pinned to
     the GitHub Security Advisory URL — modern best practice for OSS
     and works today without us needing to operate a security@
     mailbox.  An email fallback was published briefly but pulled
     because the sourceboxsentry.com domain isn't provisioned for
     incoming mail yet — a bounced report is worse than no email
     channel at all.  Add an email Contact: back when MX is live.
  4. ``Expires:`` is in the future and within RFC 9116's 1-year
     window.  Generated dynamically per request so the file never
     goes stale at rest — a regression to a static expiry date
     would silently rot.
  5. ``Canonical:`` and ``Policy:`` URLs are present so security
     scanners can verify the file came from the right host and
     point researchers at the human-readable policy.
  6. The endpoint is publicly accessible (no auth, no rate limit
     beyond the global SPA middleware).  Unauthenticated scanners
     must reach it on the first request.
  7. The SPA middleware must NOT swallow the path — without explicit
     pass-through, ``/.well-known/security.txt`` would return the
     React index.html and silently break every scanner.

The disclosure policy itself is rendered by the React frontend,
so we don't unit-test its content here — that's a job for the
visual review of /security.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta

# ── Both URLs serve the same content ───────────────────────────────


def test_well_known_security_txt_returns_text_plain(unauthenticated_client):
    resp = unauthenticated_client.get("/.well-known/security.txt")
    assert resp.status_code == 200
    # text/plain so browsers render rather than download.  charset=utf-8
    # because comments + future fields may include non-ASCII.
    assert resp.headers["content-type"].startswith("text/plain")
    assert "charset=utf-8" in resp.headers["content-type"].lower()


def test_legacy_security_txt_alias_returns_same_body(unauthenticated_client):
    """Some scanners predate RFC 9116 and only check the root path.
    Returning identical content from both keeps us discoverable
    regardless of which tool runs."""
    well_known = unauthenticated_client.get("/.well-known/security.txt")
    legacy = unauthenticated_client.get("/security.txt")

    assert legacy.status_code == 200

    # Bodies match (the same generator backs both routes — pinning here
    # catches a refactor that accidentally diverges them).  The single
    # exception is the ``Expires:`` line, which is regenerated per
    # request from ``now + 1yr`` at second precision (by design — see
    # _build_security_txt); two sequential requests that straddle a
    # one-second tick legitimately differ only on that line, so compare
    # with it normalized out rather than flaking on the clock.
    def _without_expires(text: str) -> list[str]:
        return [ln for ln in text.splitlines() if not ln.startswith("Expires:")]

    assert _without_expires(legacy.text) == _without_expires(well_known.text)
    # And both DO carry exactly one Expires line (the generator ran on
    # both paths, just at slightly different instants).
    assert sum(ln.startswith("Expires:") for ln in legacy.text.splitlines()) == 1
    assert sum(ln.startswith("Expires:") for ln in well_known.text.splitlines()) == 1


# ── Required + recommended fields ──────────────────────────────────


def test_security_txt_has_contact_field(unauthenticated_client):
    """RFC 9116 §2.5.3 requires at least one Contact: line.  Without
    this the file is malformed and scanners flag it."""
    body = unauthenticated_client.get("/.well-known/security.txt").text
    contacts = [line for line in body.splitlines() if line.startswith("Contact:")]
    assert len(contacts) >= 1, f"expected ≥1 Contact: line, got body:\n{body}"


def test_security_txt_primary_contact_is_github_security_advisories(
    unauthenticated_client,
):
    """Pin the primary contact channel.  GitHub Security Advisories is
    the modern best practice for OSS projects + works without us
    needing to operate a security@ mailbox.  A regression that swaps
    this for a non-existent URL would leave reporters with no working
    channel."""
    body = unauthenticated_client.get("/.well-known/security.txt").text
    assert (
        "https://github.com/SourceBox-LLC/Sentinel-Command/security/advisories/new"
        in body
    )


def test_security_txt_does_not_publish_dead_email_addresses(
    unauthenticated_client,
):
    """Negative pin: we do NOT publish an email contact today because
    sourceboxsentry.com has no MX records and bounces would burn
    reporters.  Catches a regression that re-adds an email Contact
    line referring to an unprovisioned address.

    Once MX is live, replace this test with a positive
    ``test_security_txt_has_email_fallback`` that asserts the
    real working address is present."""
    body = unauthenticated_client.get("/.well-known/security.txt").text
    # No mailto: contact lines until the domain can actually receive mail.
    contact_lines = [line for line in body.splitlines() if line.startswith("Contact:")]
    assert not any("mailto:" in line for line in contact_lines), (
        "security.txt published an email contact, but no MX records are "
        "configured for sourceboxsentry.com -- reports would bounce"
    )


def test_security_txt_expires_in_future_and_within_one_year(
    unauthenticated_client,
):
    """RFC 9116 §2.5.5: Expires must be ≤ 1 year in the future.
    Generated dynamically per request — a regression that hardcoded
    a date would silently rot 11 months after deploy."""
    body = unauthenticated_client.get("/.well-known/security.txt").text
    expires_lines = [line for line in body.splitlines() if line.startswith("Expires:")]
    assert len(expires_lines) == 1, "expected exactly one Expires: line"

    # Expires: 2027-03-31T03:47:33Z   →   parse ISO 8601
    raw = expires_lines[0].split(":", 1)[1].strip()
    expires_at = datetime.fromisoformat(raw.replace("Z", "+00:00"))

    now = datetime.now(tz=UTC)
    assert expires_at > now, "Expires: must be in the future"
    assert expires_at <= now + timedelta(days=366), (
        "Expires: must be ≤ 1 year in the future per RFC 9116 §2.5.5"
    )


def test_security_txt_has_canonical_and_policy(unauthenticated_client):
    """Canonical defends against the file being copy-served from a
    different host (RFC 9116 §2.5.2).  Policy points researchers at
    the human-readable disclosure terms — without it, the file is
    contact-info-only with no scope/safe-harbour signal."""
    body = unauthenticated_client.get("/.well-known/security.txt").text
    assert any(line.startswith("Canonical:") for line in body.splitlines())
    assert any(line.startswith("Policy:") for line in body.splitlines())


def test_security_txt_policy_anchor_matches_security_page(unauthenticated_client):
    """Pin the anchor — the security page on sentinel-command.com renders an
    ``id="vulnerability-disclosure"`` section that this URL deep-links to.
    A regression that renames the section would leave every scanner+researcher
    landing on the page header instead of the policy text."""
    body = unauthenticated_client.get("/.well-known/security.txt").text
    policy_line = [
        line for line in body.splitlines() if line.startswith("Policy:")
    ][0]
    assert "sentinel-command.com/security#vulnerability-disclosure" in policy_line


# ── Public + cacheable for scanners ────────────────────────────────


def test_security_txt_is_public_no_auth_required(unauthenticated_client):
    """Unauthenticated scanners must reach this on the first request.
    A regression that bumped it behind any auth guard would stop
    every external researcher cold."""
    # No Authorization header attached by the unauthenticated_client.
    resp = unauthenticated_client.get("/.well-known/security.txt")
    assert resp.status_code == 200


def test_security_txt_has_short_cache_so_expires_can_roll(unauthenticated_client):
    """The Cache-Control: max-age must be short enough that a
    scanner re-fetching tomorrow doesn't see an old Expires date.
    1 hour is the configured value — anything longer than a few
    hours and the rolling-Expires design defeats itself."""
    resp = unauthenticated_client.get("/.well-known/security.txt")
    cc = resp.headers.get("cache-control", "")
    assert "max-age=" in cc
    # Pull the seconds value and assert it's bounded.
    seconds = int(cc.split("max-age=")[1].split(",")[0].split(";")[0])
    assert seconds <= 86400, "cache must be ≤ 1 day so Expires rolls"


# ── SPA middleware doesn't swallow the path ────────────────────────


def test_well_known_path_not_served_as_react_app(unauthenticated_client):
    """The SPA fallback middleware serves index.html for unknown
    paths.  Without explicit pass-through for /.well-known/, our
    security.txt route would never run + scanners would receive
    HTML instead of the contact file.  Pin the pass-through by
    checking the body doesn't smell like the React shell."""
    body = unauthenticated_client.get("/.well-known/security.txt").text
    # React index.html starts with <!doctype html ...> — the security.txt
    # generator starts with a # comment.
    assert not body.lstrip().lower().startswith("<!doctype"), (
        "security.txt is being served the React index.html — SPA "
        "middleware is swallowing /.well-known/ requests"
    )
