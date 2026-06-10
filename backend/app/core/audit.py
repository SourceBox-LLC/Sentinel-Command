"""
Audit log helper.

Writes rows to the ``AuditLog`` table so admin actions leave a durable
paper trail — separate from application logs (which are ephemeral) and
from ``McpActivityLog`` (which tracks MCP tool invocations, not human
admin actions).

Every mutating endpoint that changes security-sensitive state (MCP keys,
node lifecycle, danger-zone wipes, recording settings) should call
``write_audit()`` after its DB commit.  Call failures are swallowed so
a bad audit row can never take down the caller.
"""

from __future__ import annotations

import json
import logging
from typing import Optional

from fastapi import Request
from sqlalchemy.orm import Session

logger = logging.getLogger(__name__)


def _client_ip(request: Optional[Request]) -> str:
    if request is None:
        return ""
    # Behind Fly's edge, request.client.host can be the proxy hop rather
    # than the real client.  The rate limiter already solved this —
    # Fly-Client-IP → XFF first hop → socket — and the audit trail (a
    # paid compliance surface) must record the same real source IP, not
    # an edge address.  Reuse that resolver.
    try:
        from app.core.limiter import _real_client_ip
        return _real_client_ip(request) or ""
    except Exception:
        if request.client is None:
            return ""
        return request.client.host or ""


def write_audit(
    db: Session,
    *,
    org_id: str,
    event: str,
    user_id: str = "",
    username: str = "",
    details: Optional[dict] = None,
    request: Optional[Request] = None,
) -> None:
    """Persist a single audit-log row.

    Arguments:
      db         — existing session (we piggyback on the caller's txn)
      org_id     — tenant identifier; required for isolation
      event      — short machine-readable verb: ``"mcp_key_created"``,
                   ``"node_deleted"``, ``"full_reset"`` etc.  Keep under 50 chars.
      user_id    — Clerk user id of the acting admin (if any)
      username   — human-readable label (email, username, or fallback)
      details    — free-form dict serialised as JSON in the ``details`` column
      request    — if provided, the client IP is recorded for forensics
    """
    try:
        from app.models.models import AuditLog

        row = AuditLog(
            org_id=org_id,
            event=event[:50],
            ip_address=_client_ip(request)[:45] or None,
            username=(username or "")[:80] or None,
            user_id=(user_id or "")[:100] or None,
            details=json.dumps(details) if details else None,
        )
        db.add(row)
        db.commit()
    except Exception:
        logger.exception("[Audit] Failed to write audit log for event=%s", event)
        try:
            db.rollback()
        except Exception:
            pass


def audit_label(user) -> str:
    """Best-effort human label for an AuthUser — email > username > user_id."""
    return (
        getattr(user, "email", None)
        or getattr(user, "username", None)
        or (getattr(user, "user_id", "") or "")[:32]
    )
