"""
GDPR helpers — single source of truth for "what data belongs to an org".

Used by two consumers:

  1. ``app/api/gdpr.py``       — the export + delete endpoints customers
                                 hit directly.
  2. ``app/api/cameras.py``    — the existing ``danger/full-reset`` flow.
  3. ``app/api/webhooks.py``   — the Clerk ``organization.deleted`` handler.

All three previously had drift: each enumerated a different subset of
the org-scoped tables, leaking customer data on cancellation (Article 17
violation) and exposing nothing for portability (Article 20 violation).
Centralising the list here means a future migration that adds a new
org-scoped table only has to add one line — both delete paths and the
export endpoint pick it up automatically.

Data inventory (every table with ``org_id``, in delete-safe order):

  Setting, AuditLog, StreamAccessLog, McpActivityLog, McpApiKey,
  OrgMonthlyUsage, EmailLog, EmailOutbox, UserNotificationState,
  Notification, MotionEvent, CameraGroup, Incident, CameraNode

Plus the cascade-deleted children that aren't org-scoped directly:

  Camera           → cascades from CameraNode
  IncidentEvidence → cascades from Incident

Tables NOT in the list (NOT org-scoped, intentionally retained):

  ProcessedWebhook  — global Svix dedupe ledger.  Contains svix message
                      ids (event-shaped UUIDs), no per-org payload, no
                      personal data.  Survives org deletion.
  EmailSuppression  — global suppression list keyed by email address.
                      Resend's webhook (or the user's own unsubscribe
                      click) wrote the row; deleting it would re-enable
                      sending to a user who explicitly opted out.  Per
                      CAN-SPAM + Resend best practice the suppression
                      list outlives any single org relationship.
"""

from __future__ import annotations

import logging
from collections.abc import Iterator
from typing import Any

from sqlalchemy.orm import Session

from app.models.models import (
    AuditLog,
    Camera,
    CameraGroup,
    CameraNode,
    EmailLog,
    EmailOutbox,
    Incident,
    IncidentEvidence,
    McpActivityLog,
    McpApiKey,
    MotionEvent,
    Notification,
    OrgMonthlyUsage,
    SentinelConfig,
    SentinelRun,
    Setting,
    StreamAccessLog,
    UserNotificationState,
)

logger = logging.getLogger(__name__)


# ── Inventory ────────────────────────────────────────────────────────
#
# Order matters for delete: tables with no inbound FK references first,
# parents (CameraNode → Camera, Incident → IncidentEvidence) last so the
# cascade ON DELETE clauses don't trip.  SQLAlchemy's session-level
# cascade ("all, delete-orphan" on the relationship) handles the actual
# child-row deletion when the parent is removed via session.delete();
# bulk Query.delete() bypasses that cascade and goes to FK constraint
# instead, so we delete CameraNode + Incident via session.delete() one-
# by-one rather than .filter_by(...).delete() to keep the cascade
# behaviour intact.
#
# Why org_id not foreign keys: most of these tables don't have a true
# FK to an "organizations" table because Clerk owns org identity.  We
# scope by string org_id ("org_28X...") and rely on the application
# always filtering, which is enforced by the every-endpoint org_id
# tests.
ORG_SCOPED_MODELS = [
    Setting,
    AuditLog,
    StreamAccessLog,
    McpActivityLog,
    McpApiKey,
    OrgMonthlyUsage,
    EmailLog,
    EmailOutbox,
    UserNotificationState,
    Notification,
    MotionEvent,
    CameraGroup,
    # Sentinel — agent config + run history are both org-scoped and
    # have no inbound FKs, so a plain bulk Query.delete() is correct.
    SentinelConfig,
    SentinelRun,
]

# Models with cascading children that need session.delete() per row
# rather than bulk Query.delete() so the SQLAlchemy cascade fires.
ORG_SCOPED_CASCADE_PARENTS = [
    Incident,    # cascades to IncidentEvidence
    CameraNode,  # cascades to Camera
]

# Cascade-children that have their own org_id column.  The parent
# cascade is the primary deletion path (so that the SQLAlchemy
# relationship invariants are respected), but we ALSO bulk-delete
# these by org_id afterward to mop up edge cases:
#   - Camera rows where node_id IS NULL (transient state during
#     node deletion — see model docstring) wouldn't be reached by
#     a CameraNode cascade.
#   - Defensive against ORM-cascade misconfigurations in future
#     model changes — if someone removes ``cascade="all, delete-
#     orphan"`` from a relationship by accident, the bulk pass
#     still gets the rows.
# IncidentEvidence is NOT here because it has no org_id column;
# its only deletion path is the Incident cascade.
ORG_SCOPED_CASCADE_CHILDREN = [
    Camera,
]


# ── Public: delete ──────────────────────────────────────────────────


def delete_org_data(db: Session, org_id: str) -> dict[str, int]:
    """Delete every row belonging to ``org_id`` across every
    org-scoped table.

    Returns ``{table_name: rows_deleted}`` for the audit-row payload.
    Caller is responsible for:

      - Sending CloudNode ``wipe_data`` commands BEFORE this runs
        (this function only touches the Command Center DB; the
        per-node local data is the node's responsibility).
      - Cleaning up in-memory caches (HLS segment cache, broadcaster
        subscribers, monthly viewer-second counters) — those live
        outside the DB and survive a Query.delete().
      - Calling ``db.commit()`` AFTER this returns so the deletes
        are durable.  We deliberately don't commit inside the
        function so the caller can compose with adjacent work in
        a single transaction.

    Raises ``Exception`` on any per-table error.  Caller should
    log + roll back; partial deletes leave the org in an
    inconsistent half-deleted state which is bad for both compliance
    and operability.
    """
    counts: dict[str, int] = {}

    # Cascade-parent models go FIRST (per-row session.delete so the
    # cascade ON DELETE clauses on Camera + IncidentEvidence fire).
    # The bulk-delete loop below would bypass the SQLAlchemy
    # cascade and either FK-constraint-fail or leave orphans.
    for Model in ORG_SCOPED_CASCADE_PARENTS:
        rows = db.query(Model).filter_by(org_id=org_id).all()
        for row in rows:
            db.delete(row)
        counts[Model.__tablename__] = len(rows)

    # Bulk delete the rest — fast, doesn't materialize objects.
    for Model in ORG_SCOPED_MODELS:
        count = db.query(Model).filter_by(org_id=org_id).delete(
            synchronize_session=False,
        )
        counts[Model.__tablename__] = count

    # Mop-up bulk delete of cascade-child tables — most rows are
    # already gone via the parent cascade above, but this catches
    # the orphan-with-null-FK edge case (see ORG_SCOPED_CASCADE_CHILDREN
    # docstring above).  Returns 0 in the common case.
    for Model in ORG_SCOPED_CASCADE_CHILDREN:
        count = db.query(Model).filter_by(org_id=org_id).delete(
            synchronize_session=False,
        )
        # Add to existing count from the cascade phase if present.
        prev = counts.get(Model.__tablename__, 0)
        counts[Model.__tablename__] = prev + count

    # Flush so subsequent queries in the same transaction see the
    # deletes.  Commit is the caller's responsibility (see docstring).
    db.flush()

    logger.info(
        "[GDPR] delete_org_data org=%s totals=%s",
        org_id, counts,
    )
    return counts


# ── Public: export ──────────────────────────────────────────────────


def export_org_data(db: Session, org_id: str) -> Iterator[tuple[str, list[dict]]]:
    """Yield ``(table_name, list_of_row_dicts)`` for every org-scoped
    table.  Generator-based so the caller can stream a ZIP without
    materialising every row at once for a large org.

    Excludes binary blob fields (IncidentEvidence.data — recordings).
    The metadata about each blob IS exported so the customer can
    correlate.  For the actual blob bytes, the customer can fetch
    each ``/api/incidents/{id}/evidence/{eid}`` URL during their
    portability window — those serve the binary content authenticated.

    Order: cascade parents last so a re-importer can recreate
    references (CameraGroup before CameraNode → Camera before Camera
    inside Node, etc.).  Not strictly required by GDPR Article 20
    but useful for any "import to a different controller" workflow.
    """
    # Bulk-deletable tables first (small, fast).
    for Model in ORG_SCOPED_MODELS:
        rows = db.query(Model).filter_by(org_id=org_id).all()
        yield Model.__tablename__, [_serialize(r) for r in rows]

    # Cascade parents (and their cascading children).  The children
    # don't have org_id of their own — they're scoped via the parent
    # relationship — so we walk them explicitly.
    incidents = db.query(Incident).filter_by(org_id=org_id).all()
    yield "incidents", [_serialize(i) for i in incidents]
    yield "incident_evidence", [
        _serialize(ev, exclude={"data"})
        for inc in incidents
        for ev in inc.evidence
    ]

    nodes = db.query(CameraNode).filter_by(org_id=org_id).all()
    yield "camera_nodes", [_serialize(n) for n in nodes]
    yield "cameras", [
        _serialize(cam) for node in nodes for cam in node.cameras
    ]


# ── Internals ───────────────────────────────────────────────────────


def _serialize(row: Any, *, exclude: set[str] | None = None) -> dict:
    """Convert a SQLAlchemy row to a JSON-safe dict.

    Prefers the model's ``to_dict()`` when defined (every row in our
    schema has one) so the export shape matches what the API returns.
    Falls back to column-introspection for forward-compat with future
    models that haven't gotten a ``to_dict`` yet.

    ``exclude`` is the set of attribute names to drop — used to skip
    binary blob fields that would balloon the export size.  ``to_dict``
    on IncidentEvidence already drops the bytes; the parameter exists
    for future-proofing.
    """
    if hasattr(row, "to_dict") and callable(row.to_dict):
        try:
            data = row.to_dict()
        except Exception:
            # to_dict explosion shouldn't tank the whole export.  Fall
            # back to introspection so the row at least appears.
            data = _introspect(row)
    else:
        data = _introspect(row)

    if exclude:
        for key in exclude:
            data.pop(key, None)
    return data


def _introspect(row: Any) -> dict:
    """Last-resort serializer using SQLAlchemy column introspection."""
    out = {}
    for col in row.__table__.columns:
        val = getattr(row, col.name, None)
        if hasattr(val, "isoformat"):  # datetime
            val = val.isoformat()
        elif isinstance(val, bytes):
            # Binary fields — never inline raw bytes; mark presence only.
            val = f"<binary {len(val)} bytes>"
        out[col.name] = val
    return out


# Re-exports so `from app.core.gdpr import Camera` / IncidentEvidence
# works for tests that want to assert on cascaded children directly.
__all__ = [
    "ORG_SCOPED_MODELS",
    "ORG_SCOPED_CASCADE_PARENTS",
    "delete_org_data",
    "export_org_data",
    "Camera",
    "IncidentEvidence",
]
