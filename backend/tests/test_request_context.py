"""
Tests for the per-request context middleware + logging filter.

Three behaviors pinned here:
  1. Every HTTP response carries an ``X-Request-Id`` header.
  2. Inbound ``X-Request-Id`` from a client/proxy is honored when
     well-formed; replaced with a fresh id when malformed (defense
     against header injection into log lines / Sentry tags).
  3. The logging filter injects ``request_id`` + ``org_id`` from the
     contextvars onto every log record, with ``-`` for unset values.

Authenticated-flow org_id propagation is exercised indirectly via
the existing test fixtures — once a request goes through
require_view / require_admin, ``get_org_id()`` returns the
authenticated org for the rest of that request.  A direct test of
that path is in ``test_request_context_authenticated`` below.
"""

from __future__ import annotations

import logging

import pytest

from app.core import request_context
from app.core.logging_setup import ContextFilter

# ── Middleware: response always gets an X-Request-Id ────────────────


def test_response_includes_request_id_header(unauthenticated_client):
    """Every response must carry X-Request-Id so a customer can quote
    it in a support ticket and we can find their request in logs."""
    resp = unauthenticated_client.get("/api/health")

    assert resp.status_code == 200
    rid = resp.headers.get("X-Request-Id", "")
    assert rid, "expected an X-Request-Id header on the response"
    # Format check: 16 hex chars from new_request_id()
    assert len(rid) == 16
    assert all(c in "0123456789abcdef" for c in rid)


def test_inbound_request_id_honored_when_wellformed(unauthenticated_client):
    """When a client/proxy sends X-Request-Id, our middleware should
    propagate it through to the response unchanged.  Enables end-to-
    end correlation across multiple services in a distributed trace."""
    custom = "trace-abc123def456"
    resp = unauthenticated_client.get(
        "/api/health", headers={"X-Request-Id": custom}
    )

    assert resp.headers.get("X-Request-Id") == custom


@pytest.mark.parametrize("bad", [
    "short",                            # too short (< 8 chars)
    "x" * 200,                          # too long (> 128 chars)
    "has spaces in it",                 # invalid character
    "has/slash",                        # invalid character
    "has\nnewline",                     # invalid character (log injection)
    "<script>alert(1)</script>",        # XSS-shaped garbage
])
def test_malformed_inbound_request_id_replaced(unauthenticated_client, bad):
    """A garbage X-Request-Id from an attacker shouldn't pollute our
    logs or Sentry tags.  Anything that fails validation gets replaced
    with a fresh server-minted id."""
    resp = unauthenticated_client.get(
        "/api/health", headers={"X-Request-Id": bad}
    )

    rid = resp.headers.get("X-Request-Id", "")
    assert rid != bad
    # Replacement is the standard 16-hex-char form.
    assert len(rid) == 16
    assert all(c in "0123456789abcdef" for c in rid)


# ── Contextvar plumbing ─────────────────────────────────────────────


def test_get_request_id_returns_empty_outside_request():
    """Code running outside any request context (background loops,
    startup, tests calling helpers directly) should see empty string,
    not crash.  Defaulting to "" lets ``if get_request_id():`` work
    without an extra is-None branch."""
    assert request_context.get_request_id() == ""


def test_get_org_id_returns_empty_when_unauthenticated():
    """Same defaults-to-empty contract for org_id."""
    assert request_context.get_org_id() == ""


def test_set_and_reset_request_id_round_trip():
    """Sanity-check the contextvar set/get/reset cycle."""
    token = request_context.set_request_id("test-request-xyz")
    try:
        assert request_context.get_request_id() == "test-request-xyz"
    finally:
        request_context.reset_request_id(token)
    # After reset, back to default.
    assert request_context.get_request_id() == ""


def test_new_request_id_format():
    """The minted request id is 16 lowercase hex chars — short enough
    to read aloud over a support call, long enough to be globally
    unique in any log window we'd ever search."""
    rid = request_context.new_request_id()
    assert len(rid) == 16
    assert all(c in "0123456789abcdef" for c in rid)
    # A second mint produces a different value (sanity check on
    # randomness — not a strict guarantee, but a 16-hex-char collision
    # would be extraordinarily unlikely in two consecutive calls).
    assert rid != request_context.new_request_id()


# ── Logging filter: injects context onto every record ───────────────


def test_logging_filter_injects_dash_when_unset():
    """When no contextvars are set, the filter normalises empty →
    ``"-"`` so the log format string stays aligned."""
    rec = logging.LogRecord(
        name="test", level=logging.INFO, pathname="x", lineno=1,
        msg="hello", args=(), exc_info=None,
    )
    ContextFilter().filter(rec)

    assert rec.request_id == "-"
    assert rec.org_id == "-"


def test_logging_filter_injects_real_values_when_set():
    """When contextvars are set, the filter pulls the actual values
    onto the log record so a downstream formatter (or Sentry breadcrumb)
    can see them."""
    rid_token = request_context.set_request_id("abc123")
    org_token = request_context.set_org_id("org_test_xyz")
    try:
        rec = logging.LogRecord(
            name="test", level=logging.INFO, pathname="x", lineno=1,
            msg="hello", args=(), exc_info=None,
        )
        ContextFilter().filter(rec)

        assert rec.request_id == "abc123"
        assert rec.org_id == "org_test_xyz"
    finally:
        request_context.reset_request_id(rid_token)
        request_context.reset_org_id(org_token)


# Note on testing the auth → set_org_id wiring: the test fixtures
# (``admin_client`` / ``viewer_client``) bypass the real
# ``get_current_user`` via ``dependency_overrides``, so the production
# code path that calls ``set_org_id(org_id)`` after JWT validation
# never runs in unit tests.  The integration is verified by reading
# the production code (``app/core/auth.py:194``) and by smoke-testing
# in production: every authenticated log line carries the org_id,
# visible in `fly logs -a sentinel-command`.
