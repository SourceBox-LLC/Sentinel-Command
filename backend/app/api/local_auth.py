"""Local (self-hosted) admin login/refresh — only mounted when
AUTH_PROVIDER=local. See app/core/local_auth.py for the credential
check + token issuance.
"""

import asyncio

from fastapi import APIRouter, HTTPException, Request
from pydantic import BaseModel

from app.core.config import settings
from app.core.limiter import limiter
from app.core.local_auth import issue_token, refresh_token, verify_local_credentials

router = APIRouter(prefix="/api/auth/local", tags=["local-auth"])

_NOT_CONFIGURED_DETAIL = (
    "Local authentication not configured. Set APP_SECRET_KEY, "
    "LOCAL_ADMIN_USERNAME, and LOCAL_ADMIN_PASSWORD_HASH."
)


class LoginRequest(BaseModel):
    username: str
    password: str


class RefreshRequest(BaseModel):
    token: str


class TokenResponse(BaseModel):
    token: str


@router.post("/login", response_model=TokenResponse)
@limiter.limit("10/minute")
async def login(request: Request, payload: LoginRequest):
    if not settings.is_local_auth_configured():
        raise HTTPException(status_code=503, detail=_NOT_CONFIGURED_DETAIL)
    # argon2's verify is deliberately slow (~100ms) — run it off the
    # event loop the same way _get_current_user_clerk offloads Clerk's
    # sync SDK call, so one login request doesn't stall every
    # concurrent request (camera segment pushes, SSE streams) sharing
    # this single-threaded loop.
    credentials_ok = await asyncio.to_thread(
        verify_local_credentials, payload.username, payload.password
    )
    if not credentials_ok:
        raise HTTPException(status_code=401, detail="Invalid username or password")
    return TokenResponse(token=issue_token())


@router.post("/refresh", response_model=TokenResponse)
@limiter.limit("30/minute")
async def refresh(request: Request, payload: RefreshRequest):
    if not settings.is_local_auth_configured():
        raise HTTPException(status_code=503, detail=_NOT_CONFIGURED_DETAIL)
    new_token = refresh_token(payload.token)
    if new_token is None:
        raise HTTPException(status_code=401, detail="Token invalid or expired")
    return TokenResponse(token=new_token)
