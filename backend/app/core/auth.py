import asyncio

import httpx
from clerk_backend_api.security import AuthenticateRequestOptions
from fastapi import Depends, HTTPException, Request, status
from sqlalchemy.orm import Session

from app.core.clerk import clerk
from app.core.config import settings
from app.core.database import get_db


class AuthUser:
    def __init__(
        self,
        user_id: str,
        org_id: str,
        org_role: str = "",
        org_permissions: list = None,
        email: str = "",
        username: str = "",
        plan: str = "free_org",
        features: list = None,
    ):
        self.user_id = user_id
        self.sub = user_id
        self.org_id = org_id
        self.org_role = org_role
        self.org_permissions = org_permissions or []
        self.email = email
        self.username = username
        self.plan = plan
        self.features = features or []

    def has_permission(self, permission: str) -> bool:
        return permission in self.org_permissions

    @property
    def is_admin(self) -> bool:
        return self.org_role in ("org:admin", "admin") or self.has_permission(
            "org:cameras:manage_cameras"
        )

    @property
    def can_view_cameras(self) -> bool:
        return True  # All org members can view cameras


def decode_v2_permissions(claims: dict) -> list:
    """
    Decode permissions from Clerk V2 JWT format.
    V2 uses compact o claim with permission bitmap.
    """
    o_claim = claims.get("o", {})
    fea_claim = claims.get("fea", "")

    if not o_claim or not fea_claim:
        return []

    # Get permission names from o.per
    per_str = o_claim.get("per", "")
    if not per_str:
        return []

    permission_names = per_str.split(",")

    # Get features from fea (strip 'o:' prefix)
    features = []
    for f in fea_claim.split(","):
        if f.startswith("o:"):
            features.append(f[2:])
        else:
            features.append(f)

    # Get feature-permission map from o.fpm
    fpm_str = o_claim.get("fpm", "")
    fpm_values = []
    if fpm_str:
        try:
            fpm_values = [int(x) for x in fpm_str.split(",")]
        except (ValueError, TypeError):
            pass

    # Reconstruct full permission keys: org:{feature}:{permission}
    permissions = []
    for i, feature in enumerate(features):
        if i < len(fpm_values):
            fpm_value = fpm_values[i]
            # Check each permission bit
            for j, perm_name in enumerate(permission_names):
                if fpm_value & (1 << j):
                    permissions.append(f"org:{feature}:{perm_name}")

    return permissions


def convert_to_httpx_request(fastapi_request: Request) -> httpx.Request:
    return httpx.Request(
        method=fastapi_request.method,
        url=str(fastapi_request.url),
        headers=dict(fastapi_request.headers),
    )


async def get_current_user(request: Request) -> AuthUser:
    if not settings.is_clerk_configured():
        raise HTTPException(
            status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
            detail="Clerk authentication not configured. Set CLERK_SECRET_KEY and CLERK_PUBLISHABLE_KEY.",
        )

    httpx_request = convert_to_httpx_request(request)

    try:
        # to_thread: verification is local RS256 most of the time, but the
        # SDK refreshes its JWKS cache every 5 minutes via a SYNC HTTPS
        # fetch (httpx transport with retries=10).  Inline, that fetch —
        # and especially its multi-second retry ladder during a Clerk
        # blip — froze the entire event loop for every tenant.  The
        # steady-state cost of the thread hop is microseconds.
        request_state = await asyncio.to_thread(
            clerk.authenticate_request,
            httpx_request,
            AuthenticateRequestOptions(authorized_parties=[settings.FRONTEND_URL]),
        )

        if not request_state.is_signed_in:
            raise HTTPException(
                status_code=status.HTTP_401_UNAUTHORIZED, detail="Not authenticated"
            )

        claims = request_state.payload

        # Decode permissions: try V1 format first, then V2 format
        org_permissions = claims.get("org_permissions") or claims.get("permissions")
        if not org_permissions:
            org_permissions = decode_v2_permissions(claims)

        user_id = claims.get("sub")
        email = claims.get("email", "")
        username = claims.get("username", "")

        # Extract active plan from V2 JWT (e.g. "o:pro" -> "pro")
        plan_claim = claims.get("pla", "")
        active_plan = plan_claim.split(":")[-1] if plan_claim else "free_org"

        # Extract active features from fea claim
        fea_claim = claims.get("fea", "")
        active_features = []
        for f in fea_claim.split(","):
            f = f.strip()
            if f.startswith("o:"):
                active_features.append(f[2:])
            elif f:
                active_features.append(f)

        # Extract org_id and org_role from V1 or V2 JWT format
        # V1: top-level org_id and org_role claims
        # V2: compact "o" claim with id and rol fields
        o_claim = claims.get("o", {})
        org_id = claims.get("org_id") or o_claim.get("id", "")
        org_role = claims.get("org_role", "") or o_claim.get("rol", "")

        if not user_id:
            raise HTTPException(
                status_code=status.HTTP_401_UNAUTHORIZED, detail="Not authenticated"
            )

        if not org_id:
            raise HTTPException(
                status_code=status.HTTP_400_BAD_REQUEST,
                detail="No organization selected. Please create or join an organization.",
            )

        auth_user = AuthUser(
            user_id=user_id,
            org_id=org_id,
            org_role=org_role,
            org_permissions=org_permissions,
            email=email,
            username=username,
            plan=active_plan,
            features=active_features,
        )

        # Tag the Sentry scope for this request.  Safe no-op if Sentry
        # isn't initialised (local dev, tests).  We deliberately skip
        # email/username — they fall under PII and we don't need them
        # for triage.
        try:
            from app.core.sentry import set_user_context

            set_user_context(user_id=user_id, org_id=org_id, plan=active_plan)
        except Exception:
            # Monitoring should never fail auth.
            pass

        # Stamp the org_id into the per-request contextvar so the
        # logging filter (app/core/logging_setup.py) injects it onto
        # every log record this request produces.  Auth failures
        # don't reach this line, so their log lines render as "org=-"
        # which is the correct signal.
        try:
            from app.core.request_context import set_org_id

            set_org_id(org_id)
        except Exception:
            # Logging context wiring should never fail auth.
            pass

        return auth_user
    except HTTPException:
        raise
    except Exception:
        import logging
        logging.getLogger(__name__).error("Authentication failed", exc_info=True)
        # Don't leak the underlying auth-internal error in the chain — the log
        # line above already captured it for operators; clients only see 401.
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Authentication failed",
        ) from None


async def require_view(user: AuthUser = Depends(get_current_user)) -> AuthUser:
    if not user.can_view_cameras:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN, detail="View permission required"
        )
    return user


async def require_admin(user: AuthUser = Depends(get_current_user)) -> AuthUser:
    if not user.is_admin:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN, detail="Admin permission required"
        )
    return user


async def require_active_billing(
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
) -> AuthUser:
    """Admin + payment must not be past due.  Use for write operations
    (create node, create key, etc.) so past-due orgs can still read
    their data but can't provision new resources.
    Reuses the request's existing DB session instead of opening a new one."""
    from app.models.models import Setting

    if Setting.get(db, user.org_id, "payment_past_due", "false") == "true":
        raise HTTPException(
            status_code=status.HTTP_402_PAYMENT_REQUIRED,
            detail="Your payment is past due. Please update your billing information before making changes.",
        )
    return user
