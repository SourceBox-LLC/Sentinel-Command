# ============================================================
# Stage 1: Build Frontend (React/Vite)
# ============================================================
# Node 24 to match deploy.yml's `node-version: "24"`. These were 20 here
# and 24 in CI, which meant the frontend build CI proves green was not the
# frontend build that ships. vite 8 wants node ^20.19.0 || >=22.12.0, so
# the old `node:20` tag satisfied it only by resolving to a late 20.x — a
# floating tag holding a requirement that has already moved once. Keep
# this number and deploy.yml's in step.
FROM node:24-alpine AS frontend-builder

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

# Install system dependencies.
#   curl               — health checks / debugging.
#   postgresql-client  — REQUIRED by scripts/backup_db.sh + restore_db.sh
#                        (pg_dump/pg_restore/psql) and by the ON_CALL
#                        runbook's manual recovery commands. Without it
#                        the scheduled backup workflow and every
#                        documented recovery path fail on the live
#                        machine.
#
# Version 18 specifically, from PGDG rather than Debian: pg_dump REFUSES
# to dump a server whose major version is newer than its own ("aborting
# because of server version mismatch"), and bookworm ships client 15
# against our 18.x server. Verified directly — client 15 fails on this
# exact server. Bump this pin whenever the cluster's major version moves.
#
# The sqlite3 CLI was here until 2026-09 for the SQLite-era backup
# scripts. The hosted database is Postgres now and nothing in this image
# reads a SQLite file; the Python sqlite3 module (stdlib, no apt package)
# is untouched, so a self-hosted SQLite run of this codebase still works.
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl ca-certificates gnupg \
    && install -d /usr/share/postgresql-common/pgdg \
    && curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
         -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc \
    && echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt bookworm-pgdg main" \
         > /etc/apt/sources.list.d/pgdg.list \
    && apt-get update && apt-get install -y --no-install-recommends \
         postgresql-client-18 \
    && apt-get purge -y gnupg && apt-get autoremove -y \
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