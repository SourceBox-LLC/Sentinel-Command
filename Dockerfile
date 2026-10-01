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

# Clerk's publishable key, overridable at build time.
#
# WHY THIS EXISTS: `VITE_*` variables are read by Vite at BUILD time and
# baked into the bundle, so they cannot be changed with `fly secrets set`
# — a secret would appear to apply and change nothing. Until this ARG, the
# only key the production build could use was the one committed in
# frontend/.env.production, which is a `pk_test_` key. Verified in the
# live bundle on 2026-09-12: the only real key literal served to users was
# `pk_test_` (55 chars).
#
# Note that `.gitignore` already says `.env.*` should not be committed;
# `.env.production` predates that rule and stayed tracked, which is how a
# test key ended up as the production default.
#
# HOW TO GO LIVE: set a CLERK_PUBLISHABLE_KEY repo secret to the
# `pk_live_...` value. deploy.yml passes it through as this build arg, it
# lands in .env.production.local, and Vite prefers that over
# .env.production (precedence: .env.[mode].local > .env.[mode] > .env).
# Then frontend/.env.production can be deleted.
#
# Deliberately defaults to empty and writes nothing when unset, so a build
# with no secret behaves exactly as before rather than shipping a bundle
# with no key at all — which would break sign-in instead of merely keeping
# it in test mode.
# THE ARG IS DELIBERATELY NOT CALLED `VITE_CLERK_PUBLISHABLE_KEY`.
#
# A Dockerfile `ARG` becomes a build-time environment variable for every
# later RUN in the stage, and Vite reads `VITE_*` from the process
# environment with HIGHER precedence than any .env file. So a
# `ARG VITE_CLERK_PUBLISHABLE_KEY=""` would put an empty VITE_ variable in
# `npm run build`'s environment and silently override the real key in
# .env.production — producing a bundle with an empty publishable key, which
# makes auth/index.jsx throw "Missing VITE_CLERK_PUBLISHABLE_KEY" at
# runtime and renders a blank page.
#
# That is not hypothetical; it is what the first version of this block did.
# Caught by building both stages and grepping the bundle:
#
#   ARG VITE_CLERK_… (empty)  -> index-CuMgJ-I8.js, no key literal
#   no ARG at all             -> index-R6eCH0Wp.js, pk_test_anVzdC1r…
#
# Naming it without the prefix keeps it invisible to Vite's env lookup, so
# the only way it can affect the build is the file written below.
ARG CLERK_PUBLISHABLE_KEY=""
RUN if [ -n "$CLERK_PUBLISHABLE_KEY" ]; then \
      printf 'VITE_CLERK_PUBLISHABLE_KEY=%s\n' "$CLERK_PUBLISHABLE_KEY" > .env.production.local; \
      echo "Clerk key: build-arg override in effect"; \
    else \
      echo "Clerk key: no build arg set — falling back to committed .env.production"; \
    fi

# Build React app (outputs to /frontend/dist/)
RUN npm run build

# ============================================================
# Stage 2: Build the Rust web tier
# ============================================================
# Pinned to the toolchain this was developed and tested against rather
# than `latest`: a compiler bump is a change to the artefact, and it
# should be a deliberate one made when someone is watching.
FROM rust:1.98-bookworm AS backend-builder

WORKDIR /build

# Manifests first, so a source-only change does not re-resolve and
# re-download the dependency graph. `src/` is faked just deeply enough
# for `cargo build` to have something to compile — the real sources
# replace it on the next COPY and the dependency layer stays cached.
COPY backend-rs/Cargo.toml backend-rs/Cargo.lock ./
RUN mkdir -p src/bin \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && echo 'fn main() {}' > src/bin/hash_password.rs \
    && echo 'fn main() {}' > src/bin/restore_from_cloud.rs \
    && cargo build --release 2>/dev/null || true

# The real thing. `migrations/` and `assets/` are both compiled IN —
# `sqlx::migrate!` embeds the SQL and `api/docs.rs` embeds the harvested
# OpenAPI document — so they are build inputs, not runtime files, and
# forgetting either is a compile error rather than a 500 in production.
COPY backend-rs/src ./src
COPY backend-rs/migrations ./migrations
COPY backend-rs/assets ./assets
# The 46 email templates are `include_str!`d by src/email_templates.rs, so
# they are a build input like the two above. They used to live under
# backend/app/ and be read from disk at runtime, which is how the image
# came to ship without them at all for one commit: no COPY, no error, and
# every notification email failing at render time in production. Embedding
# them makes a missing file a compile error instead.
COPY backend-rs/templates ./templates
COPY backend-rs/tests ./tests
COPY backend-rs/examples ./examples
# Touched so cargo does not trust the fake sources' timestamps.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked \
    && strip target/release/sentinel-command \
    && strip target/release/sentinel-hash-password \
    && strip target/release/sentinel-restore-from-cloud

# ============================================================
# Stage 3: Runtime
# ============================================================
# Debian rather than a distroless or Alpine base for two concrete
# reasons, not habit:
#
#   * postgresql-client-18 is REQUIRED by scripts/backup_db.sh and
#     restore_db.sh and by every documented recovery path in
#     docs/runbooks/. Version 18 specifically, from PGDG rather than
#     Debian: pg_dump REFUSES to dump a server whose major version is
#     newer than its own, and bookworm ships client 15 against an 18.x
#     server. Verified directly against this cluster. Bump this pin
#     whenever the cluster's major version moves.
#   * the `agent` process group is still Python, so this image needs a
#     Python runtime regardless. See the note at [processes] in fly.toml
#     and the one below.
FROM ghcr.io/astral-sh/uv:python3.12-bookworm-slim

WORKDIR /app

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

# ── The Sentinel AI agent, which is still Python ──────────────────────
#
# The rewrite deliberately stops here. `sentinel_agent/` owns the only
# LiteLLM import, was out of scope from the start, and ships as the
# `agent` process group from this same image — Fly gives one image per
# app and differs the groups only by command.
#
# Its dependency set is now its OWN rather than the whole web tier's,
# because the web tier's is gone: fastapi, sqlalchemy, psycopg, pyjwt,
# svix, clerk-backend-api, slowapi, redis, websockets and fastmcp all
# left with it. What remains is what the agent's module tree actually
# imports, checked by importing it rather than by reading the old list.
COPY backend/pyproject.toml backend/uv.lock* ./
RUN uv sync --frozen --no-dev
COPY backend/app/__init__.py ./app/__init__.py
COPY backend/app/sentinel_agent ./app/sentinel_agent

# ── The Rust web tier ────────────────────────────────────────────────
COPY --from=backend-builder /build/target/release/sentinel-command /usr/local/bin/
COPY --from=backend-builder /build/target/release/sentinel-hash-password /usr/local/bin/
COPY --from=backend-builder /build/target/release/sentinel-restore-from-cloud /usr/local/bin/

# The React build. `/app/static` is where SPA serving looks by default
# (`STATIC_DIR`), the same path main.py used, so nothing about the
# frontend deploy changed.
COPY --from=frontend-builder /frontend/dist ./static

# The operator shell scripts, and the two the install routes serve.
# `SCRIPTS_DIR` defaults to /app/scripts, which is where these land —
# `GET /install.sh` and `/mcp-setup.{sh,ps1}` read them off disk, so a
# missing copy here is a 500 on the route a new CameraNode fetches
# first.
COPY scripts ./scripts

ENV PYTHONUNBUFFERED=1
ENV STATIC_DIR=/app/static
ENV SCRIPTS_DIR=/app/scripts

EXPOSE 8000

# `[processes]` in fly.toml OVERRIDES this for both groups, so the `app`
# command there must stay in sync with this line. It is here for a plain
# `docker run` and for anyone reading the image.
#
# Note what is no longer needed: uvicorn's `--forwarded-allow-ips=*`,
# which existed so the MCP mount redirect would emit https rather than
# http behind Fly's edge. The Rust tier reads X-Forwarded-Proto directly
# in `app.rs::mcp_redirect`, and there is no access log to disable.
CMD ["/usr/local/bin/sentinel-command"]
