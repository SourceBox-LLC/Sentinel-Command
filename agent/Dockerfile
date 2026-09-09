# Matches the three sibling services (Command Center, License, Sync):
# uv with a committed uv.lock, installed --frozen.
#
# This repo previously ran `pip install -r requirements.txt` with no
# lockfile, so every build resolved fresh from PyPI. That produced three
# separate problems in two days — mcp 2.x renaming a symbol and crashing
# the app on boot, Starlette removing on_startup and doing the same, and
# a ceiling added to stop the second one silently holding the service on
# a Starlette line with 7 advisories. All three were rebuild-time
# surprises in a repo that had not been rebuilt since June.
FROM ghcr.io/astral-sh/uv:python3.12-bookworm-slim

WORKDIR /app

# Dependency layer first so app changes don't invalidate it.
COPY pyproject.toml uv.lock ./
RUN uv sync --frozen --no-install-project --no-dev

COPY . .
RUN uv sync --frozen --no-dev

# uv installs into /app/.venv; put it first so `uvicorn` resolves to it.
ENV PATH="/app/.venv/bin:$PATH"

EXPOSE 8080
CMD ["uvicorn", "app.main:app", "--host", "0.0.0.0", "--port", "8080"]
