"""
Resend transport wrapper.

This module is the *only* place in the backend that touches the Resend
SDK directly.  Everything else (worker, webhook handler, tests) goes
through ``send_email()``.  Keeping the surface area this narrow means:

  - We can stub the whole transport in tests with one monkeypatch on
    ``send_email`` — no need to mock Resend's classes.
  - Switching providers later (Postmark, SES) is a change to one file,
    not a search-and-replace across the codebase.
  - Idempotency / tagging / "would have sent" dev-mode logging are
    enforced consistently for every send, no matter the caller.

Pure transport — knows nothing about notifications, kinds, or orgs.
The worker layers that semantics on top.
"""

from __future__ import annotations

import logging
import uuid
from typing import Optional

import resend  # type: ignore[import-untyped]

from app.core.config import settings

logger = logging.getLogger(__name__)


# ── SDK init ─────────────────────────────────────────────────────────
# Module-level key set so the SDK picks it up on import.  Safe to call
# multiple times (idempotent setattr) — tests that swap RESEND_API_KEY
# can call ``_reset_for_tests`` to re-pin without re-importing.

def _apply_api_key() -> None:
    """Push the configured key into the resend module's global state."""
    resend.api_key = settings.RESEND_API_KEY or None


_apply_api_key()


def _reset_for_tests() -> None:
    """Reset SDK state between tests after monkeypatching the secret."""
    _apply_api_key()


# ── Public API ───────────────────────────────────────────────────────

class EmailSendResult:
    """Outcome of a single ``send_email`` call.

    Wraps the success/failure split so callers don't need to look at
    exception types — just check ``.ok`` and read either
    ``.message_id`` or ``.error``.  The worker uses this to decide
    between marking a row 'sent' vs incrementing 'attempts'.
    """

    __slots__ = ("ok", "message_id", "error", "skipped")

    def __init__(
        self,
        *,
        ok: bool,
        message_id: Optional[str] = None,
        error: Optional[str] = None,
        skipped: bool = False,
    ):
        self.ok = ok
        self.message_id = message_id
        self.error = error
        # ``skipped`` distinguishes "transport disabled" (kill-switch
        # off, no error) from "transport tried and failed" — the
        # worker logs them differently.
        self.skipped = skipped

    def __repr__(self) -> str:
        if self.skipped:
            return "<EmailSendResult skipped>"
        if self.ok:
            return f"<EmailSendResult ok message_id={self.message_id}>"
        return f"<EmailSendResult error={self.error!r}>"


def send_email(
    *,
    to: str,
    subject: str,
    body_text: str,
    body_html: str,
    kind: str,
    idempotency_key: Optional[str] = None,
    from_address: Optional[str] = None,
) -> EmailSendResult:
    """Send a single email via Resend.

    Returns an ``EmailSendResult``.  Never raises — every failure mode
    (kill-switch off, missing key, Resend 4xx/5xx, network error) is
    funneled into ``ok=False`` with an error string the worker can log
    and decide on retry-vs-give-up.

    Parameters
    ----------
    to, subject, body_text, body_html
        Self-explanatory.  ``body_html`` is sent as the HTML part;
        ``body_text`` is the plain-text fallback for clients that
        prefer text.  Sending both improves deliverability — pure-HTML
        emails get scored down by some spam filters.
    kind
        Notification kind (``"camera_offline"``, ``"node_offline"``,
        etc.).  Surfaces in Resend's dashboard as a tag so the
        operator can filter by event type when debugging.
    idempotency_key
        Optional caller-provided key.  When set, Resend treats two
        calls with the same key as one send — important when our
        worker retries a transient failure and the previous attempt
        secretly succeeded.  Defaults to a fresh UUID per call when
        omitted (no idempotency benefit but no harm either).
    """

    # Kill-switch.  Workers that don't read EMAIL_ENABLED themselves
    # rely on this: even if a row makes it into EmailOutbox while the
    # switch is off, the transport refuses to send.  Defense in depth.
    if not settings.EMAIL_ENABLED:
        logger.info(
            "[Email] EMAIL_ENABLED=false — would have sent kind=%s to=%s subject=%r",
            kind, to, subject[:80],
        )
        return EmailSendResult(ok=True, skipped=True)

    # Inline the configured-check (rather than calling
    # ``settings.is_email_configured()``) so test monkeypatches that
    # set the attribute on the ``settings`` instance take effect —
    # the classmethod form reads from ``cls`` which the instance
    # monkeypatch doesn't shadow.  The classmethod survives for the
    # health endpoint use case where shadowing isn't a concern.
    if not (settings.RESEND_API_KEY and settings.EMAIL_FROM_ADDRESS):
        # Distinguishes operator misconfiguration from a real outage.
        # Worker treats this as 'failed' permanently — retrying won't
        # help until the operator fixes the secret.
        return EmailSendResult(
            ok=False,
            error="resend_unconfigured: RESEND_API_KEY or EMAIL_FROM_ADDRESS missing",
        )

    # Use the override (e.g. noreply@) if provided, otherwise the default.
    effective_from = from_address or settings.EMAIL_FROM_ADDRESS
    from_field = (
        f"{settings.EMAIL_FROM_NAME} <{effective_from}>"
        if settings.EMAIL_FROM_NAME
        else effective_from
    )

    payload: dict = {
        "from": from_field,
        "to": [to],
        "subject": subject,
        "text": body_text,
        "html": body_html,
        "tags": [
            {"name": "event", "value": kind},
            {"name": "source", "value": "command_center"},
        ],
    }

    # Resend's HTTP idempotency header — same value on retry tells
    # Resend to short-circuit to the original send instead of
    # delivering a duplicate message.  This MUST go through the
    # SDK's ``options`` arg, not the message-level ``headers`` dict
    # in ``payload``: the SDK reads ``options['idempotency_key']`` and
    # sets the HTTP ``Idempotency-Key`` header, while a ``headers``
    # entry in the payload becomes an SMTP header on the OUTGOING
    # email instead of an HTTP header to Resend's API.  Verified
    # against resend/request.py:61-62 in the SDK.  Passing it via
    # the wrong path is silent — Resend just sends a fresh message
    # every retry — which is why the worker's reclaim path needs
    # this to be wired correctly to be safe.
    options = {"idempotency_key": idempotency_key or str(uuid.uuid4())}

    try:
        # The SDK's ``Emails.send`` returns a dict-like with at least
        # an ``id`` field on success.  Accessing it defensively because
        # SDK responses have changed shape across versions.
        response = resend.Emails.send(payload, options=options)
    except Exception as exc:  # noqa: BLE001 — we want to catch everything
        # Don't leak request bodies into logs (subject + recipient is
        # plenty for triage; PII / message-id stays out of the log).
        logger.warning(
            "[Email] Resend send failed kind=%s to=%s err=%s",
            kind, _redact(to), type(exc).__name__,
        )
        return EmailSendResult(ok=False, error=f"{type(exc).__name__}: {exc}")

    # Success: extract the message id for outbox/audit correlation.
    message_id: Optional[str] = None
    if isinstance(response, dict):
        message_id = response.get("id") or response.get("message_id")
    else:
        message_id = getattr(response, "id", None) or getattr(response, "message_id", None)

    if not message_id:
        # Treat as failure rather than silently lose correlation —
        # without a message id we can't match the inevitable webhook
        # event back to this row.
        logger.warning(
            "[Email] Resend returned no id for kind=%s to=%s response=%r",
            kind, _redact(to), response,
        )
        return EmailSendResult(ok=False, error="resend_no_message_id")

    logger.info(
        "[Email] sent kind=%s to=%s message_id=%s",
        kind, _redact(to), message_id,
    )
    return EmailSendResult(ok=True, message_id=message_id)


# ── Helpers ──────────────────────────────────────────────────────────

def _redact(addr: str) -> str:
    """Partially mask an email address for log output.

    ``alice@example.com`` -> ``a***@example.com``.  Enough to debug
    "is this the right user" while keeping full PII out of structured
    logs that ship to Sentry.
    """
    if not addr or "@" not in addr:
        return "***"
    local, _, domain = addr.partition("@")
    if not local:
        return f"***@{domain}"
    return f"{local[0]}***@{domain}"
