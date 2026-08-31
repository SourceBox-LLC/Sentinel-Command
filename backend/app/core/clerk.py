from clerk_backend_api import Clerk

from app.core.config import settings

# Only construct the Clerk client in hosted (Clerk) auth mode. In local
# self-hosted mode (AUTH_PROVIDER=local) there is no Clerk account at
# all, so `clerk` stays None — every caller that might run in local mode
# (app/core/plans.py, app/core/recipients.py) short-circuits before
# touching it.
if settings.is_clerk_auth():
    if not settings.CLERK_SECRET_KEY:
        raise ValueError("CLERK_SECRET_KEY is required. Please set it in your .env file.")
    clerk = Clerk(bearer_auth=settings.CLERK_SECRET_KEY)
else:
    clerk = None
