"""
MCP API Key management endpoints.
Users generate keys on the /mcp page; keys are stored hashed (SHA-256).
"""

import hashlib
import json
import secrets

from fastapi import APIRouter, Depends, HTTPException, Request
from sqlalchemy.orm import Session

from app.core.audit import audit_label, write_audit
from app.core.auth import AuthUser, require_active_billing, require_admin
from app.core.database import get_db
from app.core.limiter import limiter
from app.mcp.server import MCP_ALL_TOOLS, MCP_READ_TOOLS, MCP_WRITE_TOOLS, mcp
from app.models.models import McpApiKey
from app.schemas.schemas import McpKeyCreate

router = APIRouter(prefix="/api/mcp", tags=["mcp"])

KEY_PREFIX = "osc_"


def _generate_key() -> str:
    """Generate a random MCP API key: osc_ + 32 hex chars."""
    return KEY_PREFIX + secrets.token_hex(16)


@router.post("/keys")
@limiter.limit("10/hour")
async def create_mcp_key(
    request: Request,
    payload: McpKeyCreate,
    user: AuthUser = Depends(require_active_billing),
    db: Session = Depends(get_db),
):
    """Generate a new MCP API key for the organization with optional tool scoping."""
    scope_mode = payload.scope_mode
    scope_tools: list[str] | None = None

    if scope_mode == "custom":
        if not payload.scope_tools:
            raise HTTPException(
                status_code=400,
                detail="scope_tools must be a non-empty list when scope_mode='custom'.",
            )
        unknown = [t for t in payload.scope_tools if t not in MCP_ALL_TOOLS]
        if unknown:
            raise HTTPException(
                status_code=400,
                detail=f"Unknown tool names: {', '.join(unknown)}.",
            )
        scope_tools = list(payload.scope_tools)

    raw_key = _generate_key()
    key_hash = hashlib.sha256(raw_key.encode()).hexdigest()

    mcp_key = McpApiKey(
        org_id=user.org_id,
        key_hash=key_hash,
        name=payload.name,
        scope_mode=scope_mode,
        scope_tools=json.dumps(scope_tools) if scope_tools else None,
        kind="mcp",  # explicit; integration keys (osi_) are minted in api/integration.py
    )
    db.add(mcp_key)
    db.commit()
    db.refresh(mcp_key)

    write_audit(
        db,
        org_id=user.org_id,
        event="mcp_key_created",
        user_id=user.user_id,
        username=audit_label(user),
        details={
            "key_id": mcp_key.id,
            "name": payload.name,
            "scope_mode": scope_mode,
            "scope_tool_count": len(scope_tools) if scope_tools else None,
        },
        request=request,
    )

    # Notify admins (inbox + email if enabled) — security audit
    # signal.  Includes the actor in the body so a recipient who
    # IS the actor immediately recognises their own action vs. a
    # potential compromise.
    try:
        from app.api.notifications import create_notification
        actor = audit_label(user) or user.user_id or "unknown user"
        scope_summary = (
            "all tools" if scope_mode != "custom"
            else f"{len(scope_tools or [])} scoped tool(s)"
        )
        create_notification(
            org_id=user.org_id,
            kind="mcp_key_created",
            title=f"New MCP API key created: {payload.name}",
            body=(
                f"{actor} just created a new MCP API key "
                f"\"{payload.name}\" with access to {scope_summary}. "
                f"If this was you, no action needed.  If not, revoke "
                f"it from the MCP settings page immediately."
            ),
            severity="warning",
            audience="admin",
            link="/mcp",
            meta={
                "key_id": mcp_key.id,
                "key_name": payload.name,
                "scope_mode": scope_mode,
                "actor_user_id": user.user_id,
            },
            db=db,
        )
    except Exception:
        # Audit row already written above — losing the notification
        # email is annoying but not a security regression.  Don't
        # fail the API call.
        import logging
        logging.getLogger(__name__).exception(
            "[McpKeys] notification emit failed for key_id=%s", mcp_key.id,
        )

    return {
        "id": mcp_key.id,
        "name": mcp_key.name,
        "key": raw_key,  # Only returned once — never stored in plaintext
        "created_at": mcp_key.created_at.isoformat(),
        "scope_mode": mcp_key.scope_mode or "all",
        "scope_tools": mcp_key.get_scope_tools(),
        "warning": "Save this key now. You won't be able to see it again.",
    }


@router.get("/tools")
async def list_mcp_tools(
    user: AuthUser = Depends(require_admin),
):
    """Return the MCP tool catalog so the UI can render the scope picker.

    Tools are classified into ``read`` and ``write`` categories matching the
    sets used by ``compute_allowed_tools``. Descriptions are pulled from the
    live FastMCP registration so a UI edit never desyncs from the server —
    ``run_middleware=False`` skips our own ScopeMiddleware so the full catalog
    is returned regardless of who's calling.
    """
    registered = {t.name: t for t in await mcp.list_tools(run_middleware=False)}

    def _describe(name: str) -> str:
        tool = registered.get(name)
        if tool is None:
            return ""
        return (tool.description or "").strip()

    read_tools = sorted(
        (
            {"name": n, "description": _describe(n), "category": "read"}
            for n in MCP_READ_TOOLS
        ),
        key=lambda t: t["name"],
    )
    write_tools = sorted(
        (
            {"name": n, "description": _describe(n), "category": "write"}
            for n in MCP_WRITE_TOOLS
        ),
        key=lambda t: t["name"],
    )
    return {
        "read": read_tools,
        "write": write_tools,
        "total": len(MCP_ALL_TOOLS),
    }


@router.get("/keys")
async def list_mcp_keys(
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """List all MCP API keys for the organization (without the actual key values)."""
    keys = (
        db.query(McpApiKey)
        # kind="mcp": this page lists ONLY MCP keys. Integration keys
        # (osi_) live in the same table but are managed at /api/integration/keys.
        .filter_by(org_id=user.org_id, revoked=False, kind="mcp")
        .order_by(McpApiKey.created_at.desc())
        .all()
    )
    return [k.to_dict() for k in keys]


@router.delete("/keys/{key_id}")
@limiter.limit("30/hour")
async def revoke_mcp_key(
    key_id: int,
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """Revoke an MCP API key."""
    mcp_key = (
        db.query(McpApiKey)
        # kind="mcp": this endpoint revokes ONLY MCP keys. An integration
        # key id passed here 404s rather than crossing surfaces.
        .filter_by(id=key_id, org_id=user.org_id, kind="mcp")
        .first()
    )
    if not mcp_key:
        raise HTTPException(status_code=404, detail="Key not found")

    mcp_key.revoked = True
    db.commit()

    write_audit(
        db,
        org_id=user.org_id,
        event="mcp_key_revoked",
        user_id=user.user_id,
        username=audit_label(user),
        details={"key_id": mcp_key.id, "name": mcp_key.name},
        request=request,
    )

    # Notify admins — security audit signal, paired with the
    # creation notification so the audit trail is symmetric.
    try:
        from app.api.notifications import create_notification
        actor = audit_label(user) or user.user_id or "unknown user"
        create_notification(
            org_id=user.org_id,
            kind="mcp_key_revoked",
            title=f"MCP API key revoked: {mcp_key.name}",
            body=(
                f"{actor} just revoked the MCP API key "
                f"\"{mcp_key.name}\". Any AI client that was using "
                f"this key will start receiving 401 errors on its "
                f"next request and will need to be reconfigured."
            ),
            severity="info",
            audience="admin",
            link="/admin/audit-log",
            meta={
                "key_id": mcp_key.id,
                "key_name": mcp_key.name,
                "actor_user_id": user.user_id,
            },
            db=db,
        )
    except Exception:
        import logging
        logging.getLogger(__name__).exception(
            "[McpKeys] notification emit failed for revoke key_id=%s",
            mcp_key.id,
        )

    return {"success": True, "revoked": key_id}
