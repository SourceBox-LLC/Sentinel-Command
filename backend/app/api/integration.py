"""Integration API — ``/api/integration/*``.

Phase 1: REST integration key (``osi_``) management. These keys authenticate
the data-plane endpoints added in later phases (camera discovery, snapshots,
recording control, motion) via
``app.core.integration_auth.require_integration_org``.

Keys reuse the ``mcp_api_keys`` table with ``kind="integration"``; see that
model plus the cross-kind guards in ``app/mcp/server.py`` and
``app/api/mcp_keys.py`` for why the two key kinds can't cross surfaces.
"""

import hashlib
import logging
import secrets

from fastapi import APIRouter, Depends, HTTPException, Request
from sqlalchemy.orm import Session

from app.core.audit import audit_label, write_audit
from app.core.auth import AuthUser, require_admin
from app.core.database import get_db
from app.core.limiter import limiter
from app.models.models import McpApiKey
from app.schemas.schemas import IntegrationKeyCreate

logger = logging.getLogger(__name__)

router = APIRouter(prefix="/api/integration", tags=["integration"])

KEY_PREFIX = "osi_"


def _generate_key() -> str:
    """Generate a random integration API key: ``osi_`` + 32 hex chars."""
    return KEY_PREFIX + secrets.token_hex(16)


@router.post("/keys")
@limiter.limit("10/hour")
async def create_integration_key(
    request: Request,
    payload: IntegrationKeyCreate,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Mint an integration key for the org.

    Admin-only. The raw key is returned exactly once; only its SHA-256 hash
    is stored. Not gated on billing — the integration control plane is a
    free, trust-building feature (proxied video still inherits the
    viewer-hour cap downstream).
    """
    raw_key = _generate_key()
    key_hash = hashlib.sha256(raw_key.encode()).hexdigest()

    key = McpApiKey(
        org_id=user.org_id,
        key_hash=key_hash,
        name=payload.name,
        kind="integration",
        scope_mode=None,  # integration keys have no per-tool scoping
        scope_tools=None,
    )
    db.add(key)
    db.commit()
    db.refresh(key)

    write_audit(
        db,
        org_id=user.org_id,
        event="integration_key_created",
        user_id=user.user_id,
        username=audit_label(user),
        details={"key_id": key.id, "name": payload.name},
        request=request,
    )

    # Admin notification — same security-signal pattern as MCP keys: a new
    # credential is a sensitive event, and naming the actor lets a recipient
    # who IS the actor recognise their own action vs. a possible compromise.
    try:
        from app.api.notifications import create_notification
        actor = audit_label(user) or user.user_id or "unknown user"
        create_notification(
            org_id=user.org_id,
            kind="integration_key_created",
            title=f"New integration key created: {payload.name}",
            body=(
                f"{actor} just created a new integration API key "
                f"\"{payload.name}\" (used to connect tools like Home "
                f"Assistant to your cameras). If this was you, no action "
                f"needed. If not, revoke it immediately."
            ),
            severity="warning",
            audience="admin",
            link="/mcp",
            meta={
                "key_id": key.id,
                "key_name": payload.name,
                "actor_user_id": user.user_id,
            },
            db=db,
        )
    except Exception:
        # Audit row already written; losing the notification email is
        # annoying but not a security regression. Don't fail the API call.
        logger.exception(
            "[IntegrationKeys] notification emit failed for key_id=%s", key.id,
        )

    return {
        "id": key.id,
        "name": key.name,
        "key": raw_key,  # Only returned once — never stored in plaintext.
        "created_at": key.created_at.isoformat(),
        "kind": "integration",
        "warning": "Save this key now. You won't be able to see it again.",
    }


@router.get("/keys")
async def list_integration_keys(
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """List the org's integration keys (hashes / plaintext never returned)."""
    keys = (
        db.query(McpApiKey)
        .filter_by(org_id=user.org_id, revoked=False, kind="integration")
        .order_by(McpApiKey.created_at.desc())
        .all()
    )
    return [k.to_dict() for k in keys]


@router.delete("/keys/{key_id}")
@limiter.limit("30/hour")
async def revoke_integration_key(
    key_id: int,
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Revoke an integration key. ``kind="integration"`` scoping means an
    MCP key id passed here 404s rather than crossing surfaces."""
    key = (
        db.query(McpApiKey)
        .filter_by(id=key_id, org_id=user.org_id, kind="integration")
        .first()
    )
    if not key:
        raise HTTPException(status_code=404, detail="Key not found")

    key.revoked = True
    db.commit()

    write_audit(
        db,
        org_id=user.org_id,
        event="integration_key_revoked",
        user_id=user.user_id,
        username=audit_label(user),
        details={"key_id": key_id, "name": key.name},
        request=request,
    )
    return {"success": True}
