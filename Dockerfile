# ============================================================
# Stage 1: Build Frontend (React/Vite)
# ============================================================
FROM node:20-alpine AS frontend-builder

WORKDIR /frontend

# Copy package files first for better caching
COPY frontend/package*.json ./

# Install dependencies
RUN npm ci

# Copy frontend source and build
COPY frontend ./

# Build React app (outputs to /frontend/dist/)
RUN npm run build

# ============================================================
# Stage 2: Backend Runtime (FastAPI)
# ============================================================
FROM ghcr.io/astral-sh/uv:python3.12-bookworm-slim

WORKDIR /app

# Install system dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Copy dependency files and install Python packages
# Note: pyproject.toml goes to /app/pyproject.toml (not /app/backend/)
# This ensures uv creates the venv at /app/.venv
COPY backend/pyproject.toml backend/uv.lock* ./
RUN uv sync --frozen --no-dev

# Copy backend application code to /app (so app module is at /app/app/)
COPY backend ./

# Copy frontend build output to /app/static (where main.py expects it)
# main.py: static_dir = Path(__file__).parent.parent / "static"
# __file__ = /app/app/main.py, parent = /app/app, parent.parent = /app
# So static_dir = /app/static
COPY --from=frontend-builder /frontend/dist ./static

# Set environment variables
ENV PYTHONUNBUFFERED=1

# Expose FastAPI port
EXPOSE 8000

# Run FastAPI directly using the venv created during build
# Working directory is /app, so app.main:app resolves to /app/app/main.py
# Note: uv sync creates .venv at /app/.venv
#
# --forwarded-allow-ips="*" tells uvicorn to trust the X-Forwarded-Proto
# (and friends) header from any source. Required because we're behind
# Fly's edge proxy: without this, uvicorn defaults to trusting only
# 127.0.0.1, ignores the "https" forwarded scheme, and any FastAPI
# redirect (e.g. /mcp -> /mcp/ for the mounted MCP app) is emitted as
# http:// instead of https://. Strict HTTPS clients like mcp-remote
# refuse the HTTPS->HTTP downgrade and the request fails with
# "Unexpected content type: text/html". "*" is safe here because Fly's
# private network ensures only their edge can reach this container.
# --no-access-log: at 20 segment-pushes/s/node plus ~2 req/s per live
# viewer, uvicorn's per-request access line is a measurable slice of the
# single shared CPU and drowns the app's structured logs in Fly's
# ingest. Request-id app logging (request_context.py) already covers
# the forensic need.
CMD ["/app/.venv/bin/uvicorn", "app.main:app", "--host", "0.0.0.0", "--port", "8000", "--workers", "1", "--timeout-keep-alive", "65", "--forwarded-allow-ips=*", "--no-access-log"]