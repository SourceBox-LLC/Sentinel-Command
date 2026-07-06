"""
Tests for the Jinja2 email renderer (app/core/email_templates.py).

Cover the bits with logic — autoescape selection per file
extension, the NotificationProxy meta parsing, fallback when a
template file is missing.  The actual template content (subject
strings, body copy) is exercised end-to-end via test_notifications.
"""

from __future__ import annotations

import logging
from types import SimpleNamespace

import pytest

from app.core import email_templates


def _fake_notif(**overrides):
    """Build a SimpleNamespace that quacks like a Notification for
    the renderer's purposes.  Avoids needing a full DB fixture for
    template-only tests."""
    base = {
        "title": "Front Door went offline",
        "body": "No heartbeat in 90s.",
        "severity": "warning",
        "camera_id": "cam_front_door",
        "node_id": None,
        "link": "/dashboard?camera=cam_front_door",
        "meta_json": None,
    }
    base.update(overrides)
    return SimpleNamespace(**base)


# ── Autoescape selection ─────────────────────────────────────────────

@pytest.mark.parametrize("name,expected", [
    ("camera_offline.body.html.j2", True),
    ("camera_offline.body.txt.j2", False),
    ("camera_offline.subject.txt.j2", False),
    ("_layout.html.j2", True),
    ("foo.html", True),
    ("foo.txt", False),
    (None, False),
])
def test_should_autoescape_picks_html_only(name, expected):
    """The .html.j2 vs .txt.j2 distinction drives autoescape — without
    it, plain-text emails would render literal '&amp;' instead of
    the original ampersand."""
    assert email_templates._should_autoescape(name) is expected


# ── Notification proxy ───────────────────────────────────────────────

def test_notification_proxy_parses_meta_json():
    """Templates access ``notification.meta.incident_id`` as if it
    were a regular dict; the proxy parses meta_json on construction
    so templates don't need to call json.loads themselves."""
    notif = _fake_notif(meta_json='{"incident_id": 42, "severity": "high"}')
    proxy = email_templates._NotificationProxy(notif)

    assert proxy.meta == {"incident_id": 42, "severity": "high"}
    # Other fields forwarded to the wrapped notification.
    assert proxy.title == "Front Door went offline"
    assert proxy.camera_id == "cam_front_door"


def test_notification_proxy_handles_missing_meta():
    """No meta_json → empty dict, NOT None.  Lets templates use
    ``notification.meta.foo`` without a guard."""
    notif = _fake_notif(meta_json=None)
    proxy = email_templates._NotificationProxy(notif)
    assert proxy.meta == {}


def test_notification_proxy_handles_invalid_meta_json():
    """Garbage in meta_json → empty dict, not crash.  meta_json is
    notionally controlled by us (we serialize it from
    create_notification's meta arg) but defense in depth."""
    notif = _fake_notif(meta_json="this is not json")
    proxy = email_templates._NotificationProxy(notif)
    assert proxy.meta == {}


def test_notification_proxy_handles_non_dict_meta():
    """Templates expect ``notification.meta`` to be a dict.  A
    JSON list or string in meta_json must not surface as the wrong
    type — return {} instead."""
    notif = _fake_notif(meta_json='["a", "b"]')
    proxy = email_templates._NotificationProxy(notif)
    assert proxy.meta == {}


# ── Every-kind coverage sweep ────────────────────────────────────────
#
# Only camera_offline was previously rendered end-to-end.  With
# EMAIL_ENABLED=true in production, every one of these 15 kinds actually
# sends, so a broken per-kind template (Jinja syntax error, or an
# operation that raises on its real context) would ship.  Because the
# renderer catches template errors and silently substitutes the GENERIC
# fallback (see _render_or_fallback), such a break would NOT crash the
# worker — it would just email a degraded, less-specific message.  This
# sweep renders every kind with realistic per-kind context and fails if
# the renderer had to fall back for any of them.
#
# Each entry is (kind, meta_dict).  meta covers exactly the
# ``notification.meta.*`` keys that kind's templates reference (grepped
# from app/templates/emails/); a value listed here that then shows up in
# the rendered body proves the real template consumed it.
_ALL_EMAIL_KINDS = [
    ("camera_offline", {}),
    ("camera_online", {}),
    ("node_offline", {}),
    ("node_online", {}),
    ("incident_created", {"incident_id": 7}),
    ("mcp_key_created", {}),
    ("mcp_key_revoked", {}),
    ("cameranode_disk_low", {"percent_used": 92}),
    ("member_added", {"role": "admin"}),
    ("member_role_changed", {"new_role": "admin"}),
    ("member_removed", {}),
    ("member_promotion_requested", {"requester_email": "member@example.test"}),
    ("motion", {"score": 78}),
    ("motion_digest", {
        "event_count": 12,
        "window_start": "2026-07-05T09:00:00+00:00",
        "window_end": "2026-07-05T09:15:00+00:00",
        "cooldown_minutes": 15,
    }),
    ("welcome", {}),
]


@pytest.mark.parametrize("kind,meta", _ALL_EMAIL_KINDS)
def test_every_email_kind_renders_its_own_template(kind, meta, caplog):
    """Render every notification kind with realistic context and assert
    its DEDICATED template rendered — i.e. the renderer did NOT log a
    "template not found" / "template render failed" and silently fall
    back to the generic block.

    This is the regression catch for a per-kind template that ships
    broken: with EMAIL_ENABLED on it would send a degraded email to a
    real customer for a real event (a new incident, an admin promotion,
    a disk-full warning) without any loud failure."""
    import json

    notif = _fake_notif(
        title=f"{kind} title",
        body=f"{kind} body copy.",
        # A camera-scoped link so camera/motion/incident templates that
        # gate on notification.link render their CTA branch.
        link="/dashboard?camera=cam_x",
        camera_id="cam_x",
        meta_json=json.dumps(meta) if meta else None,
    )

    with caplog.at_level(logging.WARNING, logger="app.core.email_templates"):
        subject, body_text, body_html = email_templates.render(
            kind, notif, unsubscribe_url="https://x.test/u",
        )

    # No fallback path was taken — both TemplateNotFound (warning) and
    # render failure (exception) log through this logger.
    template_errors = [
        r.getMessage() for r in caplog.records
        if r.name == "app.core.email_templates"
    ]
    assert not template_errors, (
        f"kind {kind!r} fell back instead of using its template: {template_errors}"
    )

    # Shape: all three parts present, HTML wrapped by the brand layout.
    assert subject.strip(), f"{kind}: empty subject"
    assert body_text.strip(), f"{kind}: empty text body"
    assert "<!DOCTYPE html>" in body_html, f"{kind}: HTML not layout-wrapped"
    assert "https://x.test/u" in body_html, f"{kind}: unsubscribe link missing"

    # Meta-bearing kinds must actually surface their meta value — proves
    # the template consumed the field rather than rendering a blank slot
    # (Jinja's default Undefined renders empty without erroring).
    for value in meta.values():
        assert str(value) in body_text or str(value) in body_html, (
            f"{kind}: meta value {value!r} never appeared in the rendered "
            f"email — template likely references a different key"
        )


# ── render() integration ─────────────────────────────────────────────

def test_render_camera_offline_produces_three_strings():
    """Smoke test the full pipeline for the canonical kind.  Don't
    pin specific copy (templates change) — just verify the shape."""
    notif = _fake_notif()

    subject, body_text, body_html = email_templates.render(
        "camera_offline", notif,
        unsubscribe_url="https://x.test/api/notifications/email/unsubscribe?t=abc",
    )

    assert isinstance(subject, str) and subject.strip()
    assert "Sentinel" in subject
    assert "Front Door" in subject
    assert isinstance(body_text, str)
    assert "Front Door went offline" in body_text
    assert "https://x.test/api/notifications/email/unsubscribe?t=abc" in body_text

    assert isinstance(body_html, str)
    # Layout wrap brings in brand header.
    assert "<!DOCTYPE html>" in body_html
    assert "Sentinel" in body_html
    # Severity bar coloured for warning.
    assert "#f59e0b" in body_html  # severity="warning"


def test_render_severity_color_propagates():
    """Severity drives the colored bar in the layout — test each
    mapped severity at the boundary."""
    cases = {
        "critical": "#ef4444",
        "error": "#ef4444",
        "warning": "#f59e0b",
        "info": "#22c55e",
    }
    for severity, expected_color in cases.items():
        notif = _fake_notif(severity=severity)
        _, _, body_html = email_templates.render(
            "camera_offline", notif,
            unsubscribe_url="https://x.test/u",
        )
        assert expected_color in body_html, f"missing {expected_color} for {severity}"


def test_render_unknown_severity_falls_back_to_green():
    """A future severity value we don't recognise renders green
    (default) instead of crashing."""
    notif = _fake_notif(severity="psychic_damage")
    _, _, body_html = email_templates.render(
        "camera_offline", notif,
        unsubscribe_url="https://x.test/u",
    )
    assert "#22c55e" in body_html  # info default


def test_render_unknown_kind_uses_generic_fallback():
    """A kind without dedicated templates still emits a usable
    email — fallback subject + generic body block.  Important
    because the inbox supports kinds the email layer doesn't yet."""
    notif = _fake_notif(title="Mystery event", body="Something happened.")

    subject, body_text, body_html = email_templates.render(
        "kind_we_havent_built_a_template_for", notif,
        unsubscribe_url="https://x.test/u",
    )

    # Generic fallback subject.
    assert "Mystery event" in subject
    # Body still includes the title + body text + unsubscribe link.
    assert "Mystery event" in body_text
    assert "Something happened" in body_text
    assert "https://x.test/u" in body_text


def test_render_html_escapes_user_content():
    """Notification title/body fields containing HTML must be
    escaped in the HTML body block.  Defense in depth — the field
    is operator-controlled but a malformed camera name shouldn't
    open an XSS path."""
    notif = _fake_notif(
        title="<script>alert(1)</script>",
        body="Body with <b>bold</b>",
    )
    _, _, body_html = email_templates.render(
        "camera_offline", notif,
        unsubscribe_url="https://x.test/u",
    )
    assert "<script>alert(1)</script>" not in body_html
    assert "&lt;script&gt;" in body_html


def test_render_text_does_not_escape_user_content():
    """Plain-text body must NOT escape — would render '&amp;' as
    literal text in the email instead of '&'.  The .txt.j2
    templates are autoescape-off because of _should_autoescape."""
    notif = _fake_notif(body="Tom & Jerry < or >")
    _, body_text, _ = email_templates.render(
        "camera_offline", notif,
        unsubscribe_url="https://x.test/u",
    )
    assert "Tom & Jerry < or >" in body_text


def test_render_uses_dashboard_url_override():
    """Caller can override the dashboard URL (tests, multi-domain
    deploys).  Without override, falls back to settings.FRONTEND_URL."""
    notif = _fake_notif()
    _, body_text, _ = email_templates.render(
        "camera_offline", notif,
        unsubscribe_url="https://x.test/u",
        dashboard_url="https://override.example.com",
    )
    assert "https://override.example.com" in body_text


def test_render_strips_embedded_newlines_from_subject():
    """A title containing CR/LF (operator-controlled camera name OR
    AI-agent-supplied incident title) must NOT leak into the rendered
    subject as embedded newlines.  Resend's API rejects subject
    header injection today, but a future provider swap that forwards
    subjects raw to SMTP would turn this into a Bcc-injection vector.

    Covers `\\n`, `\\r`, and `\\r\\n` separators for completeness."""
    notif = _fake_notif(title="Front Door\r\nBcc: attacker@evil.test")

    subject, _, _ = email_templates.render(
        "camera_offline", notif,
        unsubscribe_url="https://x.test/u",
    )

    assert "\r" not in subject
    assert "\n" not in subject
    # The original characters survive (just as spaces / removed CRs)
    # so the alert remains intelligible to the recipient.
    assert "Front Door" in subject
    assert "Bcc: attacker@evil.test" in subject
