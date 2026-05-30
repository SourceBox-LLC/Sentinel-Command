"""Auth dependency for REST integration keys (``osi_``).

Integration keys (e.g. Home Assistant) authenticate to the
``/api/integration/*`` surface. They share the ``mcp_api_keys`` table with
MCP keys but are distinguished by ``kind="integration"`` — the filter below
is the security boundary that stops an MCP key (``osc_``) from reaching this
surface, mirroring the ``kind="mcp"`` guard on the MCP tool path in
``app/mcp/server.py``.
"""

import hashlib
from datetime import UTC, datetime

from fastapi import Depends, HTTPException, Request, status
from sqlalchemy.orm import Session

from app.core.auth import AuthUser
from app.core.database import get_db
from app.models.models import McpApiKey


async def require_integration_org(
    request: Request,
    db: Session = Depends(get_db),
) -> AuthUser:
    """Resolve a Bearer integration key to its org.

    Returns an ``AuthUser`` scoped to the key's org so downstream handlers
    reuse the same org-isolation patterns as Clerk-authenticated routes.
    Raises 401 on any missing / malformed / unknown / revoked / wrong-kind
    key.
    """
    auth = request.headers.get("authorization", "")
    if not auth.lower().startswith("bearer "):
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Missing Bearer integration key",
        )
    raw_key = auth.split(" ", 1)[1].strip()
    if not raw_key:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Empty Bearer token",
        )

    key_hash = hashlib.sha256(raw_key.encode()).hexdigest()
    key = (
        db.query(McpApiKey)
        # kind="integration" is the boundary — an MCP key (osc_) must NOT
        # authenticate here, just as an integration key must not reach the
        # MCP tool surface (see app/mcp/server.py).
        .filter_by(key_hash=key_hash, revoked=False, kind="integration")
        .first()
    )
    if not key:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid or revoked integration key",
        )

    # Touch last_used_at so the dashboard can surface "last seen" / an
    # orphaned key. Cheap single-row UPDATE on the already-open session.
    key.last_used_at = datetime.now(tz=UTC).replace(tzinfo=None)
    db.commit()

    # Synthetic AuthUser scoped to the key's org. org_role="integration"
    # keeps is_admin False (integration keys read + drive cameras, they
    # don't perform admin actions); can_view_cameras is True for all roles.
    # Handlers that need the real plan must call resolve_org_plan(db,
    # org_id) — the default plan here is not authoritative.
    return AuthUser(
        user_id=f"integration:{key.id}",
        org_id=key.org_id,
        org_role="integration",
        org_permissions=[],
        email="",
        username=key.name,
    )
