#!/usr/bin/env python3
"""Regenerate tests/fixtures/email_corpus.json.

Every notification that goes out by email is rendered from the Jinja2
templates in `backend/app/templates/emails/` — fifteen kinds, three
files each, wrapped in a shared layout. The rendered strings are not a
detail of the mail transport: they are written into `email_outbox` rows,
which the write differential compares column by column. So the port has
to produce the same bytes, not merely the same information.

This renders each kind through the backend's own
`email_templates.render`, over a set of notification shapes chosen to
reach the branches the templates actually have: a missing camera, an
empty body, a link that falls back to `/dashboard`, the `meta` keys the
digests and membership mails read, and text that has to survive
escaping in HTML while staying literal in the plain-text part.

Usage: backend/.venv/bin/python tests/differential/gen_email_corpus.py
"""
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[3] / "backend"))

from app.core import email_templates  # noqa: E402

# What `create_notification` passes as the placeholder, substituted per
# recipient at enqueue time.
UNSUB = "UNSUB-URL-PLACEHOLDER-7f3a"

KINDS = [
    "camera_offline", "camera_online", "node_offline", "node_online",
    "incident_created", "mcp_key_created", "mcp_key_revoked",
    "cameranode_disk_low", "member_added", "member_role_changed",
    "member_removed", "member_promotion_requested", "motion",
    "motion_digest", "welcome",
    # Not a real kind: the render falls back to a generic body, which is
    # its own code path.
    "no_such_kind",
]


class FakeNotification:
    """The attributes `_NotificationProxy` forwards, and nothing else."""

    def __init__(self, **fields):
        self.id = fields.get("id", 1)
        self.title = fields.get("title", "Driveway went offline")
        self.body = fields.get("body", "No heartbeat received in over 90 seconds.")
        self.severity = fields.get("severity", "warning")
        self.link = fields.get("link", "/dashboard?camera=cam-live")
        self.camera_id = fields.get("camera_id", "cam-live")
        self.node_id = fields.get("node_id", "node-aaaa1111")
        self.meta_json = fields.get("meta_json")


SHAPES = [
    ("plain", {}),
    ("no camera", {"camera_id": None}),
    ("no link", {"link": None}),
    ("empty body", {"body": ""}),
    ("critical", {"severity": "critical"}),
    ("info", {"severity": "info"}),
    ("error", {"severity": "error"}),
    ("unknown severity", {"severity": "nonsense"}),
    # Every template escapes into HTML and must not escape into text.
    ("markup in the title", {"title": "<script>alert(1)</script> & \"quotes\" 'more'"}),
    ("markup in the body", {"body": "5 < 6 & 7 > 2, <b>bold</b>"}),
    ("unicode", {"title": "Café — Ünïcødé 😀", "body": "naïve façade"}),
    # The keys the digests and membership mails read.
    ("meta for digests", {"meta_json": json.dumps({
        "event_count": 12, "window_start": "2026-09-01T10:00:00",
        "window_end": "2026-09-01T10:30:00", "score": 87,
    })}),
    ("meta for members", {"meta_json": json.dumps({
        "role": "org:admin", "new_role": "org:member",
        "requester_email": "someone@example.com",
    })}),
    ("meta for disk", {"meta_json": json.dumps({"percent_used": 96})}),
    ("meta that is not an object", {"meta_json": json.dumps([1, 2, 3])}),
    ("meta that is not json", {"meta_json": "{not json"}),
    ("no meta", {"meta_json": None}),
    # The subject lines cut a title down with Python's `str.replace`
    # and `str.split`, and the default title above matches only one of
    # the separators they look for — so without these the whole of
    # that machinery renders identically however it is written.  Each
    # is the shape its own kind really produces.
    ("title a node subject cuts", {"title": "Node 'garage-pi' went offline"}),
    ("title an online subject cuts", {"title": "Driveway is back online"}),
    ("title a motion subject cuts", {"title": "Motion on Driveway"}),
    ("title a disk subject cuts", {"title": "CameraNode disk low: garage-pi"}),
    ("title an incident subject splits", {"title": "Incident: Person at the back door"}),
    ("title a digest subject splits", {"title": "3 motion events on Driveway"}),
    # Two separators, so a split that caps at the wrong number keeps
    # the wrong tail.
    ("title with repeated separators", {"title": "Incident: escalated: Person at the door"}),
    # Two occurrences, so replacing all differs from replacing one.
    ("title with a repeated substring", {"title": "Driveway went offline went offline"}),
    # A camera name is operator-controlled and an incident title is
    # agent-written, so a subject can carry a CR/LF the renderer has to
    # scrub before a future SMTP provider reads it as a header.
    ("title with a header injection", {
        "title": "Driveway\r\nBcc: attacker@example.com went offline",
    }),
]

# The dashboard URL is right-stripped of slashes before every link is
# built from it, and `FRONTEND_URL` is configuration — an operator who
# sets it with a trailing slash must not get `//dashboard` in their
# mail.  One kind with a real body template and one that falls back to
# the generic body are enough to reach both places that join it.
DASH_VARIANTS = [
    ("https://example.test", KINDS, SHAPES),
    ("https://example.test///", ["camera_offline", "no_such_kind"], SHAPES[:3]),
]


def main() -> None:
    corpus = []
    for dashboard_url, kinds, shapes in DASH_VARIANTS:
        for kind in kinds:
            for label, fields in shapes:
                notif = FakeNotification(**fields)
                subject, text, html = email_templates.render(
                    kind, notif, unsubscribe_url=UNSUB, dashboard_url=dashboard_url,
                )
                corpus.append({
                    "kind": kind,
                    "shape": label,
                    "dashboard_url": dashboard_url,
                    "notification": {
                        "title": notif.title,
                        "body": notif.body,
                        "severity": notif.severity,
                        "link": notif.link,
                        "camera_id": notif.camera_id,
                        "node_id": notif.node_id,
                        "meta_json": notif.meta_json,
                    },
                    "subject": subject,
                    "body_text": text,
                    "body_html": html,
                })

    out = pathlib.Path(__file__).resolve().parents[2] / "tests" / "fixtures" / "email_corpus.json"
    out.write_text(json.dumps(corpus, indent=1) + "\n")
    print(f"wrote {len(corpus)} renders ({len(KINDS)} kinds x {len(SHAPES)} shapes, "
          f"plus {len(corpus) - len(KINDS) * len(SHAPES)} with a trailing-slash dashboard URL) to {out}")


if __name__ == "__main__":
    main()
