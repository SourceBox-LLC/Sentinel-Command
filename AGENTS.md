# AGENTS.md

The developer reference for **Sentinel Command Center**: the cloud dashboard and API for the **Sentinel by SourceBox** security-camera system. It is written for engineers and coding agents changing this repository. For how the whole system fits together, read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) first. For the AI agent in depth, read [docs/SENTINEL_AGENT.md](docs/SENTINEL_AGENT.md).

**In one paragraph.** A Rust backend ([axum](https://github.com/tokio-rs/axum)) and a React 19 frontend, in one repo and one Docker image. Browsers sign in with Clerk (hosted) or a local admin account (self-hosted). CameraNodes push live video into an in-memory segment cache, and browsers play it as HLS. There is no object storage in the live path: no S3, no Tigris, no presigned URLs. Data lives in PostgreSQL (hosted) or SQLite (the self-hosted default). The Sentinel AI agent is a second binary from the same crate.

## Contents

| | |
| --- | --- |
| [Names and identifiers](#names-and-identifiers) | [Authentication](#authentication) |
| [Repository layout](#repository-layout) | [Data model](#data-model) |
| [Build, run and test](#build-run-and-test) | [API routes](#api-routes) |
| [Configuration](#configuration) | [MCP server](#mcp-server) |
| [Code map](#code-map) | [Plans and limits](#plans-and-limits) |
| [How a request flows](#how-a-request-flows) | [Webhooks](#webhooks) |
| [Live video](#live-video) | [Background loops](#background-loops) |
| [Serving the frontend](#serving-the-frontend) | [Notifications and email](#notifications-and-email) |
| [Errors](#errors) | [Conventions](#conventions) |
| [Two databases](#two-databases) | [History: the Rust rewrite](#history-the-rust-rewrite) |

---

## Names and identifiers

The product has had three names: **OpenSentry** (early), **SourceBox Sentry** (mid-2026), and **Sentinel by SourceBox** (current). **Sentinel AI** is the name of the AI-agent feature only.

| Thing | Current name |
| --- | --- |
| This repo | `SourceBox-LLC/Sentinel-Command` (was `OpenSentry-Command`; GitHub redirects the old URL) |
| Camera daemon repo | `SourceBox-LLC/Sentinel-CameraNode` (was `opensentry-cloud-node`) |
| The app (dashboard and API) | `https://app.sentinel-command.com`, the Fly app `sentinel-command` |
| Marketing site and user docs | `https://sentinel-command.com` (GitHub Pages), docs at `/documentation/` |
| CameraNode binary | `sourcebox-sentry-cameranode` (renamed from `…-cloudnode` on 2026-09-09) |

**Do not rename these without a migration plan.** They outlived the rebrands on purpose:

- the env-var prefix `SOURCEBOX_SENTRY_*` (CameraNode);
- the Windows install path `C:\ProgramData\SourceBoxSentry\`;
- the agent env vars `OPENSENTRY_API_BASE`, `OPENSENTRY_MCP_AGENT_KEY`, `OPENSENTRY_MCP_URL`;
- the CameraNode AES key-derivation domain `opensentry-cameranode-machine-id-v2` (`database.rs::KEY_DOMAIN_V2` in that repo). This one is dangerous: change it and an existing encrypted `node.db` silently fails to open. It was renamed once, on 2026-09-09, while there were zero installs; from the first real install on, it needs a migration.

**Use `app.sentinel-command.com`, not `sentinel-command.fly.dev`.** Both reach the same machine. But Clerk session tokens carry the origin they were minted for (`azp`), and the backend accepts only `FRONTEND_URL`. So sign-in works on `app.sentinel-command.com` only.

## Repository layout

Command Center and the Sentinel AI agent ship from **one repo, one image, one deploy**. They run as two Fly *process groups* of the `sentinel-command` app, on separate machines.

| | Command Center | Sentinel AI agent |
| --- | --- | --- |
| Code | `backend-rs/` + `frontend/` | `backend-rs/src/agent/` (same crate) |
| Process group | `app` | `agent` |
| Command | `/usr/local/bin/sentinel-command` | `/usr/local/bin/sentinel-agent` |
| Machine | 1 GB, always on, owns the `/data` volume | 512 MB, always on, no volume |

```text
backend-rs/     Rust: the web tier, the agent, two operator tools
frontend/       React 19 + Vite; built into backend's static dir
scripts/        installer and MCP setup scripts the backend serves; DB backup/restore
docs/           architecture, agent, runbooks, ADRs, legal drafts
Dockerfile      one image: frontend build + both Rust builds (Postgres and SQLite)
fly.toml        both process groups; read its comments before changing memory or scaling
```

**Five rules. Break one and a deploy breaks.**

1. **One `Cargo.toml` builds both binaries, so a dependency change is a change to both.** `rig-core` and `rig-reqwest` are the agent's alone and are pre-1.0, so a minor version is a breaking release. Dependabot is told to leave their minors alone (`.github/dependabot.yml`). Move them by hand and check the agent's wire behaviour.
2. **`[[mounts]]` must stay scoped to `processes = ["app"]`.** Unscoped, it applies to every group, and the agent machine fails to boot competing for the volume's single attachment slot.
3. **`[processes]` overrides the Dockerfile `CMD`.** The `app` command in `fly.toml` must stay in sync with that `CMD` (both are `/usr/local/bin/sentinel-command`, no arguments).
4. **Required checks are matched by name, so renaming a CI job hangs every PR.** `master` requires `Backend tests (sqlite)`, `Backend tests (postgres)` and `Frontend audit + build`, all produced by `.github/workflows/deploy.yml` (which also runs `Backend tests (no database)`). GitHub reports *no status at all* for a check that never runs, so a renamed job looks like a stuck check, not a config error.
5. **CI path filtering is asymmetric on purpose.** `push` is filtered (docs-only pushes don't deploy). `pull_request` is **never** filtered, for the reason in rule 4.

CI runs on pull requests and on pushes to `master`, **not** on pushes to other branches. Its first backend step is `cargo fmt --check`: run `cargo fmt` before pushing.

## Build, run and test

**Prerequisites:** the Rust toolchain pinned in the `Dockerfile` builder stage (`rust:1.98`), and Node 18+.

```bash
# Backend (PostgreSQL build)
cd backend-rs
cargo run                              # http://localhost:8000, needs DATABASE_URL=postgresql://…
DATABASE_URL=sqlite:///./sentinel.db cargo run --features sqlite   # the SQLite build

# Tests
cargo test                             # no database needed; DB-gated tests skip themselves
TEST_DATABASE_URL=postgresql://… cargo test   # also runs the Postgres integration tests
cargo test --features sqlite           # SQLite build; its DB tests always run, on a fresh file
cargo fmt                              # CI rejects unformatted code
cargo clippy --all-targets -- -D warnings                    # zero warnings, both builds
cargo clippy --all-targets --features sqlite -- -D warnings

# Frontend
cd frontend
npm install
npm run dev                            # http://localhost:5173, proxies /api to :8000
npm run build                          # → frontend/dist, copied to /app/static in the image
npx vitest run                         # frontend tests
```

Give `TEST_DATABASE_URL` a database of its own: the integration tests insert rows.

### Self-hosted, with Docker Compose

`docker-compose.yml` runs Command Center with PostgreSQL. `docker-compose.sqlite.yml` runs one container with the database as a file on its volume. Both use `AUTH_PROVIDER=local`: one admin account, no Clerk, no billing.

```bash
cp backend-rs/.env.example .env              # set LOCAL_ADMIN_USERNAME and LOCAL_ADMIN_EMAIL
openssl rand -hex 32                         # → APP_SECRET_KEY
docker compose run --rm --no-deps app sentinel-hash-password    # → LOCAL_ADMIN_PASSWORD_HASH
docker compose up -d                         # http://localhost:8000
```

**Put the password hash in single quotes in `.env`.** An argon2 hash is full of `$`, and Compose interpolates `.env` values: unquoted, `$argon2id$v=19$m=65536…` arrives as `=19=65536…` and every login fails with no explanation. Use `-f docker-compose.sqlite.yml` for the SQLite variant.

Without Docker: `cargo run --bin sentinel-hash-password` (or `--stdin`), set the variables under [Local auth](#local-auth-self-hosted), and set `VITE_AUTH_PROVIDER=local` in `frontend/.env`.

### Operator tools

Both ship in the image on `PATH`:

```bash
fly ssh console -a sentinel-command -C sentinel-hash-password
fly ssh console -a sentinel-command -C "sentinel-restore-from-cloud --list"
```

### Running the agent locally

```bash
cd backend-rs
OLLAMA_API_KEY=… SENTINEL_AGENT_KEY=dev-secret AGENT_MODE=poll \
OPENSENTRY_API_BASE=http://localhost:8000 WEBHOOK_VERIFY_SIGNATURE=false \
  cargo run --bin sentinel-agent          # :8080; also reads a .env in its working directory
```

## Configuration

Everything is an environment variable. `backend-rs/.env.example` is a commented starting point; this section is the full reference. `fly.toml`'s `[env]` sets the two non-secret production values (`FRONTEND_URL`, `SEGMENT_CACHE_MAX_TOTAL_BYTES`); everything else in production is a Fly secret.

### Required

| Variable | When | Notes |
| --- | --- | --- |
| `CLERK_PUBLISHABLE_KEY`, `CLERK_SECRET_KEY` | `AUTH_PROVIDER=clerk` (the default) | The issuer and JWKS URL are derived from the publishable key. |
| `APP_SECRET_KEY`, `LOCAL_ADMIN_USERNAME`, `LOCAL_ADMIN_PASSWORD_HASH`, `LOCAL_ADMIN_EMAIL` | `AUTH_PROVIDER=local` | See [Local auth](#local-auth-self-hosted). |
| `DATABASE_URL` | hosted | Unset, it is `sqlite:///./sentinel.db`. A hosted machine without it would start on an empty file, which is why it stays a Fly secret. |

### Core

| Variable | Default | Purpose |
| --- | --- | --- |
| `AUTH_PROVIDER` | `clerk` | `clerk` or `local`. Must match the frontend's `VITE_AUTH_PROVIDER`. |
| `DATABASE_URL` | `sqlite:///./sentinel.db` | `postgresql://…` / `postgres://…`, or `sqlite:///relative` / `sqlite:////absolute`. A `postgresql+psycopg://` suffix is stripped. |
| `FRONTEND_URL` | `http://localhost:5173` | The address people open the dashboard at. It is the Clerk `azp` the backend accepts, a CORS origin, and the base of every link in an email. Production: `https://app.sentinel-command.com`. |
| `CORS_ALLOWED_ORIGINS` | — | Extra CORS origins, comma-separated. `http://localhost:5173` and `http://localhost:8000` are always allowed. |
| `TRUST_PROXY_HEADERS` | on under Fly, else off | Take the client address from `Fly-Client-IP` / `X-Forwarded-For`. Off, the TCP peer is used. It decides rate-limit buckets and the IP in the audit log. Turn it on only behind your own reverse proxy, and make the proxy overwrite `X-Forwarded-For`. |
| `PORT` | `8000` | Listen port. |
| `STATIC_DIR` | `/app/static` | The built frontend. |
| `SCRIPTS_DIR` | `/app/scripts` | Where `/install.sh` and `/mcp-setup.*` are read from. |
| `REDIS_URL` | — | Shared rate-limit counters. Without it, counters are per process: fine on one machine. Production has used Upstash on Fly; confirm with `fly secrets list`. |
| `API_DOCS_ENABLED` | off under Fly, else on | Serves `/api-docs`, `/api-redoc` and `/api/openapi.json`. |
| `CLERK_WEBHOOK_SECRET` | — | Svix signing secret for `/api/webhooks/clerk`. Unset, every delivery is refused. |
| `LOCAL_ORG_ID` | `self-host` | The single org in local-auth mode. |

### Live video cache

| Variable | Default | Purpose |
| --- | --- | --- |
| `SEGMENT_CACHE_MAX_PER_CAMERA` | `60` | Segments kept per camera (~60 s at CameraNode's 1-second segments). |
| `SEGMENT_CACHE_MAX_TOTAL_BYTES` | `402653184` (384 MiB) | Ceiling across all cameras; oldest segments go first. **Must stay well under machine RAM**, or the OOM killer takes every org's streams at once. Change it together with `[[vm]] memory_mb`; see the `fly.toml` comment. |
| `SEGMENT_PUSH_MAX_BYTES` | `2097152` | Largest accepted segment. |
| `PLAYLIST_PUSH_MAX_BYTES` | `65536` | Largest accepted playlist. |
| `CLEANUP_INTERVAL` | `20` | Run cache eviction every N playlist updates. |
| `SEGMENT_CACHE_EVICT_INTERVAL_SECONDS` | `60` | How often caches of cameras that stopped pushing are dropped. |
| `VIEWER_USAGE_FLUSH_INTERVAL_SECONDS` | `60` | How often viewer-seconds are written to the database. |
| `AUTH_CACHE_SECONDS` | `10` | How long a segment request's auth result is cached. |

### Sentinel AI and self-hosted licensing

| Variable | Default | Purpose |
| --- | --- | --- |
| `SENTINEL_AGENT_KEY` | — | Shared secret for the run-queue API (`X-Sentinel-Agent-Key`) and the HMAC on wakeup webhooks. ⚠️ Multi-tenant: its holder can drain every org's queue. Never give it to a customer; issue an `osa_` key instead. Unset disables only the first-party agent path. |
| `SENTINEL_AGENT_MCP_KEY` | — | The first-party agent's bearer for the MCP tools. |
| `SENTINEL_AGENT_WEBHOOK_URL` | — | Where wakeups go. Production: `http://sentinel-command.flycast:8080/wakeup` (the `agent` process group over Fly's private network). Unset, no webhook is sent and the agent must poll. |
| `SENTINEL_DISPATCH_ENABLED` | `true` | Kill switch for creating new runs. |
| `SENTINEL_GLOBAL_MONTHLY_RUN_CAP` | `0` (none) | Fleet-wide ceiling on runs per month, on top of the per-plan caps. |
| `SENTINEL_LICENSE_KEY` | — | Self-hosted only. Unlocks Sentinel AI, and cloud data-sync if the licence includes it. |
| `SENTINEL_LICENSE_SERVICE_URL` | `https://sentinel-license.fly.dev` | |
| `SENTINEL_SYNC_SERVICE_URL` | `https://sentinel-sync.fly.dev` | |

The agent's own variables are in [docs/SENTINEL_AGENT.md](docs/SENTINEL_AGENT.md#configuration).

### Email (Resend)

| Variable | Default | Purpose |
| --- | --- | --- |
| `EMAIL_ENABLED` | `false` | Global kill switch. Off, the worker still runs and logs "would have sent". |
| `RESEND_API_KEY` | — | |
| `RESEND_WEBHOOK_SECRET` | — | Svix secret for `/api/webhooks/resend`. |
| `EMAIL_FROM_ADDRESS` | `notifications@sentinel-command.com` | Must be on a Resend-verified domain. No-reply by design. |
| `EMAIL_FROM_NAME` | `Sentinel by SourceBox` | |
| `EMAIL_WORKER_INTERVAL_SECONDS` | `5` | Outbox drain interval. |
| `EMAIL_WORKER_BATCH_SIZE` | `20` | Rows per drain. |
| `EMAIL_MAX_ATTEMPTS` | `3` | Retries before a row is marked failed. |
| `EMAIL_TEMPLATES_DIR` | — | Preview template edits without rebuilding. Unset in production: the 46 templates are compiled in, so a missing one is a build error. |

### Error tracking (Sentry)

| Variable | Default | Purpose |
| --- | --- | --- |
| `SENTRY_DSN` | — | Unset, Sentry is a no-op. Production gets it from the Fly Sentry extension. |
| `SENTRY_TRACES_SAMPLE_RATE` | `0.1` | |
| `SENTRY_ENVIRONMENT` | `production` under Fly, else `development` | |
| `SENTRY_RELEASE` | `FLY_MACHINE_VERSION` | |

Sentry is wired through `tracing`, **so `tracing::error!` is an alert.** ERROR lines become Sentry events and INFO/WARN become breadcrumbs. The disk-critical alert (`loops.rs`) depends on this. Query strings are stripped from every event before it leaves (`sentry.rs::scrub`), because an old CameraNode WebSocket handshake carried `api_key=` in the URL.

### CameraNode versions and loop intervals

| Variable | Default | Purpose |
| --- | --- | --- |
| `MIN_SUPPORTED_NODE_VERSION` | see `config.rs` | CameraNodes below this are refused at registration (426). |
| `LATEST_NODE_VERSION` | see `config.rs` | Fallback for "update available" until GitHub is reached. |
| `RELEASE_CACHE_REFRESH_INTERVAL_SECONDS` | `600` | How often the latest CameraNode release is fetched. |
| `OFFLINE_SWEEP_INTERVAL_SECONDS` | `30` | |
| `MOTION_DIGEST_INTERVAL_SECONDS` | `60` | |
| `SENTINEL_REAPER_INTERVAL_SECONDS` | `300` | |
| `DISK_CHECK_INTERVAL_SECONDS` | `300` | |

**Not read, despite appearing in old docs:** `INACTIVE_CAMERA_CLEANUP_HOURS` (caches are dropped a minute after a camera stops pushing), `LOG_RETENTION_DAYS` (retention is per plan), and `SENTINEL_LICENSE_GRACE_HOURS` (the 72-hour grace is a constant in `license.rs`).

### Frontend

`frontend/.env`: `VITE_AUTH_PROVIDER` (`clerk` or `local`), `VITE_CLERK_PUBLISHABLE_KEY`, `VITE_API_URL` (empty in development: Vite proxies), `VITE_LOCAL_HLS` (development only: stream straight from a CameraNode on `localhost:8080`).

## Code map

```text
backend-rs/
├── src/
│   ├── main.rs             start-up: config, pool, migrations, loops, serve
│   ├── app.rs              THE ROUTE TABLE. Every route is registered here. Read it first.
│   ├── api/                one file per area: cameras, nodes, node_register, node_writes,
│   │                       hls, incidents, motion, notifications, sentinel, sentinel_config,
│   │                       integration, keys, mcp_activity, audit, stream_logs, settings,
│   │                       groups, recording, timezone, gdpr, install, health, docs,
│   │                       local_auth, clerk_webhook, webhooks (Resend), well_known, ws
│   ├── mcp/                server.rs (rmcp handler), tools.rs (23 tools), scope.rs (scope gate,
│   │                       rate limits, tool catalog), auth.rs, pre_auth.rs, activity.rs, snapshot.rs
│   ├── agent/              the Sentinel AI agent (see docs/SENTINEL_AGENT.md)
│   ├── auth.rs, auth/      Clerk JWT (V1 + V2 claims), local auth, extractors
│   ├── hls.rs              the in-memory segment cache and viewer-hour metering
│   ├── ws.rs               the CameraNode WebSocket connection manager
│   ├── loops.rs            background sweeps (see Background loops)
│   ├── notifications.rs    inbox, broadcaster, email fan-out
│   ├── email*.rs           sending, templates, worker, unsubscribe tokens
│   ├── plans.rs            plan limits, camera cap, payment grace
│   ├── sentinel_dispatch.rs  when a Sentinel run is created, and the caps
│   ├── license.rs, sync.rs   self-hosted licence check-in and cloud mirror
│   ├── ratelimit.rs        per-route rate limits and the client-address rule
│   ├── error.rs, query.rs  the error envelope, request validation
│   ├── db.rs               which database this build opens; the helpers that differ
│   ├── spa.rs, headers.rs, cors.rs, sentry.rs, csv_export.rs, versions.rs
│   ├── py*.rs, zoneinfo.rs CPython behaviours the port had to match exactly
│   │                       (json.dumps spacing, round(), int(), fromisoformat, str())
│   └── bin/                agent.rs (sentinel-agent), hash_password.rs, restore_from_cloud.rs
├── migrations/             PostgreSQL schema, embedded at compile time
├── migrations-sqlite/      SQLite schema (the same 21 tables)
├── templates/emails/       46 Jinja templates, compiled in
├── assets/agent/           the agent's prompts, compiled in
├── assets/openapi.json     the API schema served at /api/openapi.json
└── tests/                  *_db.rs integration tests, routing.rs, agent_contract.rs,
                            differential/ (the port's verification record; see History)
```

## How a request flows

```text
Browser ───── Clerk or local JWT ─────▶ axum ──SQL──▶ PostgreSQL | SQLite
CameraNode ── X-Node-API-Key (HTTP) ──▶  │  ──RAM──▶ segment cache, broadcasters
           ── WebSocket /ws/node ─────▶  │
MCP client ── Bearer osc_… / osa_ ────▶ rmcp → scope gate → tools
Agent ─────── X-Sentinel-Agent-Key ───▶ run-queue API
```

Every query filters by the `org_id` taken from the authenticated credential, never from the request body.

## Live video

1. CameraNode cuts 1-second `.ts` segments with FFmpeg.
2. It pushes each one: `POST /api/cameras/{id}/push-segment?filename=segment_NNNNN.ts`, raw body, `X-Node-API-Key`.
3. The backend keeps the bytes in `hls.rs`'s cache (camera → filename → bytes), evicting the oldest past `SEGMENT_CACHE_MAX_PER_CAMERA`.
4. CameraNode pushes the rolling playlist: `POST /api/cameras/{id}/playlist`.
5. The backend rewrites segment names to relative `segment/<file>` URLs and caches the playlist. It strips any `#EXT-X-CODECS` line, which is only valid in a master playlist. The codec CameraNode reports (`POST /api/cameras/{id}/codec`) is stored on the camera and node rows instead.
6. The browser fetches `GET /api/cameras/{id}/stream.m3u8` and each `segment/{file}` with its JWT (HLS.js `xhrSetup`), served from memory.
7. Every served segment counts against the org's monthly viewer-hours (see [Plans and limits](#plans-and-limits)).
8. A 60-second sweep drops the caches of any camera that has stopped pushing.

A camera suspended by its plan's camera cap gets **402** with a `plan_limit_hit` body on push-segment. CameraNode treats that as non-retryable, and the heartbeat response lists suspended cameras so the node stops uploading them.

## Serving the frontend

`spa.rs::fallback` runs only when no route matched:

- Paths under `/api`, `/ws`, `/install.`, `/mcp-setup.`, `/downloads/`, `/.well-known/` and `/security.txt` get **404 `{"detail": "Not Found"}`**, never the React page. A CameraNode asking for a removed route must not get a 200 HTML page.
- `POST /mcp` redirects (307) to `/mcp/`, the MCP endpoint. `GET /mcp` is the React MCP page.
- `/assets/*` is served from the built frontend; anything else gets `index.html` for client-side routing.
- A known path with the wrong method is **405** with `Allow` and the body `{"detail": "Method Not Allowed"}`.

`/` redirects hosted visitors to the marketing site. A self-hosted install (`VITE_AUTH_PROVIDER=local`) goes to `/dashboard` instead.

Every response carries `X-Request-Id`, `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, a referrer policy and a permissions policy, plus HSTS over HTTPS (`headers.rs`). There is no Content-Security-Policy yet.

## Errors

- Most errors are `{"detail": "message"}`. Newer endpoints use `ApiError`'s structured form, `{"detail": {"error": "code", "message": "…", …}}`.
- Validation failures are **422** with `{"detail": {"error": "validation_failed", "message": "…", "errors": [{type, loc, msg, input}]}}` (`query.rs`). Undeclared fields in a JSON body are refused, not ignored, where ignoring would be unsafe (`/api/mcp/keys`). A NUL character anywhere in a JSON body is a 422.
- Rate limits are **429** with `Retry-After` and `{"error": "rate_limit_exceeded", "message", "limit", "retry_after_seconds"}`.
- An unexpected failure is a 500 whose detail is logged once, as an error, so it reaches Sentry.
- The frontend's `services/api.js::parseErrorBody` understands all of these shapes. MCP tools answer with a tool error instead, because JSON-RPC fixes its own error shape.

## Two databases

PostgreSQL in production; SQLite is the self-hosted default. **The driver is chosen at build time** (`--features sqlite`); `src/db.rs` explains why. The image carries both builds, and `sentinel-command` replaces itself with `sentinel-command-sqlite` when `DATABASE_URL` is a `sqlite://` URL, so the URL is the only choice an operator makes.

- **Schema.** `migrations/0001_adopt_production_schema.sql` is production's `pg_dump --schema-only`, not a transcription. `migrations-sqlite/` is what the old Python models produced on SQLite, so a `sentinel.db` from the last Python release opens unchanged. Both are applied at start-up by `sqlx::migrate!`. History: [ADR 0001](docs/adr/0001-sync-schema-vs-alembic.md).
- **Pragmas (SQLite).** WAL, `synchronous=NORMAL`, 30-second `busy_timeout`, foreign keys on.
- **Pool (PostgreSQL).** 10 connections, 10-second acquire timeout.
- **Queries** are runtime-checked (`sqlx::query*`), not the compile-time macros, so building needs no database.

**Writing SQL that runs on both** (full list in [backend-rs/README.md](backend-rs/README.md#two-databases)):

- `CAST(x AS TEXT)`, never `x::text`.
- Every `ORDER BY` over a nullable column says `NULLS LAST` or `NULLS FIRST`; the engines sort NULL at opposite ends, and a unit test enforces it.
- Bind times from Rust (`now_naive()`), not `now()`.
- Lists use `db::any` / `db::list`; case-insensitive matching uses `db::ILIKE`.
- Serialise check-then-write races with a conditional `UPDATE`, or with `db::lock_for_update(tx, key)` (a transaction-scoped advisory lock on PostgreSQL; SQLite's `BEGIN IMMEDIATE` already serialises writers).

**Production database.** One shared Fly Postgres cluster, `sentinel-postgres`, holds one database per service: `sentinel_command`, `sentinel_license` and `sentinel_sync`. Each database is owned by its own role, `CONNECT` is revoked from `PUBLIC`, and no role is a superuser. **`fly postgres attach` creates a superuser role**, so any future attach must be re-hardened ([DISASTER_RECOVERY.md](docs/runbooks/DISASTER_RECOVERY.md)).

## Authentication

Six kinds of credential. Each has its own scope and its own way to revoke it.

| Credential | Used by | Checked by | Scope |
| --- | --- | --- | --- |
| Clerk session JWT | browsers (hosted) | `auth.rs` | one user in one org, with role |
| Local JWT | browser (self-hosted) | `auth/local.rs` | the single admin |
| Node API key (`X-Node-API-Key`) | CameraNodes | `api/node_register.rs` | one node |
| MCP key (`Bearer osc_…`) | Claude and other MCP clients | `mcp/auth.rs` | one org; scope `all`, `readonly` or `custom` |
| Integration key (`Bearer osi_…`) | Home Assistant | `api/integration.rs` | one org, `/api/integration/*` only |
| Agent key (`osa_…`, or the shared `SENTINEL_AGENT_KEY`) | the Sentinel AI agent | `api/sentinel.rs`, `mcp/auth.rs` | `osa_`: one org. Shared key: every org |

Node, MCP, integration and `osa_` keys are stored as SHA-256 hashes and shown in full once, at creation.

### Clerk (hosted)

The token comes from `Authorization: Bearer`, or else the Clerk session cookie. It is verified locally against a cached JWKS (RS256), with no Clerk SDK and no network call per request. Checks: issuer, expiry, not-before (5 s leeway), and **`azp` must equal `FRONTEND_URL` exactly**. `org_id`, role and permissions come from the claims, in either Clerk's V1 or V2 (`o`, `fea`) format (`auth/claims.rs`).

- **`RequireView`**: any member of the org.
- **`RequireAdmin`**: role `org:admin` / `admin`, or the `org:cameras:manage_cameras` permission.
- **`RequireActiveBilling`**: admin, and the org not past due.

A 401 from any API call makes the frontend end the Clerk session and return to `/sign-in`.

### Local auth (self-hosted)

`AUTH_PROVIDER=local` replaces Clerk with one admin account and one org (`LOCAL_ORG_ID`). Both produce the same `AuthUser`, so every route and plan check works unchanged.

- **Login.** `POST /api/auth/local/login` checks `LOCAL_ADMIN_USERNAME` and the argon2 `LOCAL_ADMIN_PASSWORD_HASH` (from `sentinel-hash-password`, whose parameters match python-argon2's), and returns a 30-day HS256 token signed with `APP_SECRET_KEY`. It is limited to 10 attempts a minute **per client address**, whatever credentials the request carries (`PerMinuteByIp`).
- **Refresh.** `POST /api/auth/local/refresh` re-signs a valid token, so an open dashboard never hits the 30-day wall.
- **Plan.** Every plan lookup short-circuits to the `self_host` tier. Everything is unlocked except Sentinel AI, which needs a licence.
- **Not available:** the Clerk webhook route is not mounted, the hourly plan reconcile does not run, and email recipients are just `LOCAL_ADMIN_EMAIL`.
- **Frontend.** `frontend/src/auth/` is a facade: with `VITE_AUTH_PROVIDER=local` it supplies a local stand-in for Clerk's hooks, so pages need not know which mode is active.
- This login is unrelated to a CameraNode's own local-dashboard password.

### Self-hosted licence and cloud sync

- **Licence.** `license.rs` checks `SENTINEL_LICENSE_KEY` with Sentinel-License-Service at start-up and every 15 minutes, caching the verdict in `settings` rows. If the service is unreachable, the last good verdict holds for **72 hours**. If it answers "no", access ends at once. One predicate, `license::sentinel_blocked_by_license`, gates Sentinel AI wherever the plan does: dispatch, the manual run, the Sentinel config API, and agent MCP auth.
- **Cloud sync.** A second entitlement on the same licence (`sync_enabled`). `sync.rs` pushes changed rows to Sentinel-Sync-Service every 30 minutes, keyset-paged on `(timestamp, id)` with a 10-second settle window, so a slow commit is never skipped. Payloads are raw column values. The exceptions: `camera_nodes.api_key_hash` and `incident_evidence.data` are never sent. Deletions propagate only for cameras, groups and nodes; log tables keep history the local install has pruned. Recovery is `sentinel-restore-from-cloud` ([DISASTER_RECOVERY.md](docs/runbooks/DISASTER_RECOVERY.md#self-hosted-installs-restoring-from-the-cloud-mirror)).

### Node keys

The key's SHA-256 is matched against `camera_nodes.api_key_hash`, and the org comes from the node row. A wrong key at `/register` or `/validate` records "Invalid API key" on the node only if it has **never connected**. Node IDs are not secret, so on a working node that note would let anyone tell an org to rotate a good key.

### MCP and integration keys

Both live in `mcp_api_keys`, split by `kind` (`mcp` or `integration`). **Every query filters on `kind`**, so neither kind works on the other's surface. Integration keys work on every plan.

## Data model

21 tables (`migrations/0001_adopt_production_schema.sql`). Every table has `org_id` except `processed_webhooks` (webhook dedup, keyed on the Svix message id) and `email_suppression` (one bounce/complaint list for every org), which are cross-tenant by design.

| Table | Holds |
| --- | --- |
| `cameras` | a camera: node, name, status, codecs, group, recording policy, `disabled_by_plan`, `last_seen` |
| `camera_nodes` | a CameraNode: `api_key_hash`, hostname, status, version, storage stats, `last_register_error` |
| `camera_groups` | user-defined groups (name, colour, icon) |
| `settings` | per-org key/value: plan, timezone, notification toggles, payment flags, licence verdict, sync cursors, cooldown anchors |
| `audit_log` | security audit trail |
| `stream_access_logs` | who watched which camera, when, from where |
| `mcp_api_keys` | MCP (`osc_`) and integration (`osi_`) keys; `scope_mode`, `scope_tools` |
| `mcp_activity_logs` | one row per MCP tool call |
| `sentinel_agent_keys` | per-org agent keys (`osa_`) |
| `sentinel_config` | per-org Sentinel AI settings; created on first read, switched **off** (the Privacy Policy promises it is off until an admin turns it on) |
| `sentinel_runs` | one row per agent run: trigger, outcome, severity, incident, tool trace |
| `incidents` | title, summary, markdown report, severity, status (`open`, `acknowledged`, `resolved`, `dismissed`) |
| `incident_evidence` | snapshot (JPEG), clip (MPEG-TS) or text observation; bytes stored inline |
| `motion_events` | motion reported by CameraNode (score 0–100) |
| `notifications` | the inbox (see [Notifications and email](#notifications-and-email)) |
| `user_notification_state` | each user's read cursor |
| `org_monthly_usage` | viewer-seconds per org per month |
| `email_outbox` | queued emails, drained by the worker |
| `email_log` | per-org email audit |
| `email_suppression` | addresses never to mail again (cross-tenant) |
| `processed_webhooks` | Clerk and Resend webhook dedup (cross-tenant) |

## API routes

**`backend-rs/src/app.rs` is the authoritative list.** Auth levels: *view* = any org member, *admin* = org admin, *billing* = admin and not past due. Rate limits are per minute unless marked `/h`. HLS `GET`s are not rate limited; they are metered against viewer-hours.

### Cameras, groups and settings

| Method | Path | Auth | Limit |
| --- | --- | --- | --- |
| GET | `/api/cameras`, `/api/cameras/{id}` | view | |
| POST | `/api/cameras/{id}/snapshot`: the node captures and stores a snapshot | view | 30 |
| POST | `/api/cameras/{id}/recording`: start/stop (flips `continuous_24_7`) | admin | 30 |
| PATCH | `/api/cameras/{id}/recording-settings`: continuous / scheduled window | admin | 30 |
| GET / POST | `/api/camera-groups` | view / admin | – / 20 |
| DELETE | `/api/camera-groups/{id}`: its cameras are unassigned | admin | 60 |
| PUT | `/api/cameras/{id}/group` | admin | 60 |
| GET | `/api/settings`, `/api/settings/notifications`, `/api/settings/motion-ingestion` | view | |
| POST | `/api/settings/notifications`, `/api/settings/motion-ingestion`, `/api/settings/timezone` | admin | 30 |
| POST | `/api/settings/danger/wipe-logs`: delete stream and MCP logs | admin | 5/h |
| POST | `/api/settings/danger/full-reset`: GDPR erasure of all org data | admin | 3/h |
| POST | `/api/gdpr/export`: export all org data | admin | 3/h |
| GET | `/api/audit-logs`, `/api/audit/stream-logs`, `/api/audit/stream-logs/stats` | admin | 120 / 120 / 60 |

`/api/audit-logs`, `/api/audit/stream-logs` and `/api/mcp/activity/logs` also answer `?format=csv`, streamed (`csv_export.rs`). String cells starting with `=`, `+`, `-`, `@`, tab or CR are prefixed with `'` so spreadsheets don't run them as formulas.

### Nodes (CameraNode)

| Method | Path | Auth | Limit |
| --- | --- | --- | --- |
| POST | `/api/nodes/validate`: check a node ID and key pair (setup wizard) | node key | 10 |
| POST | `/api/nodes/register`: register cameras, report version | node key | 10 |
| POST | `/api/nodes/heartbeat` | node key | 60 |
| POST | `/api/nodes/self/decommission`: the node deletes itself before a local wipe | node key | 10/h |
| GET | `/ws/node`: WebSocket (see below) | node key | |
| GET | `/api/nodes`, `/api/nodes/{id}`, `/api/nodes/ws-status` | admin | |
| GET | `/api/nodes/plan`: plan, usage and limits | view | |
| POST | `/api/nodes`: create a node, returns its key once | billing | 20/h |
| DELETE | `/api/nodes/{id}`: deletes its cameras and caches | admin | 20/h |
| POST | `/api/nodes/{id}/rotate-key` | admin | 5 |

**WebSocket `/ws/node`.** The node authenticates with `X-Node-API-Key` and `X-Node-Id` headers; an old `?api_key=&node_id=` query string still works but logs a deprecation warning. Node → backend messages are `heartbeat` and `command_result`. Backend → node messages are `ack`, `command` (`take_snapshot`, `list_snapshots`, `list_recordings`, `wipe_data`) and `error`. Any other message type gets an `error` frame, and frames containing a NUL are refused. Motion never travels over the socket.

### Live video (HLS)

| Method | Path | Auth | Limit |
| --- | --- | --- | --- |
| GET | `/api/cameras/{id}/stream.m3u8`, `/api/cameras/{id}/segment/{file}` | any signed-in member | viewer-hours |
| POST | `/api/cameras/{id}/push-segment?filename=…` | node key | 1200 |
| POST | `/api/cameras/{id}/playlist` | node key | 600 |
| POST | `/api/cameras/{id}/codec` | node key | 30 |
| POST | `/api/cameras/{id}/motion`: a no-op answer when the org has motion ingestion off | node key | 120 |

### Incidents

| Method | Path | Auth | Limit |
| --- | --- | --- | --- |
| GET | `/api/incidents` (filters: `status`, `severity`, `camera_id`, `limit`, `offset`), `/api/incidents/counts`, `/api/incidents/{id}` | admin | |
| POST | `/api/incidents` | admin | 60 |
| PATCH / DELETE | `/api/incidents/{id}`: deleting removes its evidence | admin | 120 / 60 |
| GET | `/api/incidents/{id}/evidence/{eid}`, `…/playlist.m3u8` (single-segment clip playback) | admin | 120 |

### Motion and notifications

| Method | Path | Auth | Limit |
| --- | --- | --- | --- |
| GET | `/api/motion/events`, `/api/motion/events/stats` | view | |
| GET | `/api/motion/events/stream` (SSE) | view | 60 |
| GET | `/api/notifications`, `/api/notifications/unread-count` (capped at 99) | view | |
| GET | `/api/notifications/stream` (SSE) | view | 60 |
| POST | `/api/notifications/mark-viewed`, `/api/notifications/clear-all`: a per-user cursor; deletes nothing | view | |
| POST | `/api/notifications/request-admin-promotion` | view | 3/h |
| GET / POST | `/api/notifications/email/preferences` | view / admin | |
| GET | `/api/notifications/email/unsubscribe?t=…`: signed one-click link; returns an HTML page | signed token | 60 |

Members never see `audience = "admin"` notifications, in the list, the count or the stream.

### Sentinel AI

| Method | Path | Auth | Limit |
| --- | --- | --- | --- |
| GET | `/api/sentinel/config`: always 200; non-eligible orgs get a read-only payload | view | |
| PATCH | `/api/sentinel/config` | admin + eligible plan | |
| GET | `/api/sentinel/runs`, `/api/sentinel/runs/{id}` | view | |
| POST | `/api/sentinel/runs/manual`: "Run now" (skips schedule and scope, not the cap) | admin + eligible plan | |
| GET / POST | `/api/sentinel/agent-keys`: issue a per-org `osa_` key | admin / billing | – / 10/h |
| DELETE | `/api/sentinel/agent-keys/{id}` | admin | 30/h |
| GET | `/api/sentinel/runs/pending`: oldest first | agent key | |
| POST | `/api/sentinel/runs/{id}/start`: claim, `pending → running`; exactly one caller wins | agent key | |
| POST | `/api/sentinel/runs/{id}/complete`: `incident`, `no_action` or `error`, plus the tool trace | agent key | |

`/complete` is idempotent: a finished run cannot be changed, except that an `error` (for example, from the reaper) can still be upgraded to a real outcome. An `incident_id` is accepted only if it belongs to the run's org.

### Keys, MCP activity and integrations

| Method | Path | Auth | Limit |
| --- | --- | --- | --- |
| GET / POST | `/api/mcp/keys`: body `{name, scope_mode, scope_tools?}`; any other field is a 422 | admin / billing | – / 10/h |
| DELETE | `/api/mcp/keys/{id}` | admin | 30/h |
| GET | `/api/mcp/tools`: the tool catalog with read/write kind | admin | |
| GET | `/api/mcp/activity/{recent,sessions,stats}` | admin | |
| GET | `/api/mcp/activity/logs`, `…/logs/stats` | admin | 120 / 60 |
| GET | `/api/mcp/activity/stream` (SSE) | admin | 60 |
| GET / POST / DELETE | `/api/integration/keys`, `/api/integration/keys/{id}` | admin | – / 10/h / 30/h |
| GET | `/api/integration/cameras`: with LAN `local_url` and recording state | `osi_` key | 120 |
| GET | `/api/integration/cameras/{id}/snapshot`: live JPEG via the node | `osi_` key | 30 |
| POST | `/api/integration/cameras/{id}/recording`: body `{recording: bool}` | `osi_` key | 60 |
| GET | `/api/integration/status`: camera and node counts, disk, versions, plan | `osi_` key | 120 |
| GET | `/api/integration/motion/stream` (SSE; its own subscriber pool) | `osi_` key | 60 |

### Auth, webhooks, health and public files

| Method | Path | Auth | Limit |
| --- | --- | --- | --- |
| POST | `/api/auth/local/login` | – | 10 per address |
| POST | `/api/auth/local/refresh` | local JWT | 30 |
| POST | `/api/webhooks/clerk` | Svix signature | 120 |
| POST | `/api/webhooks/resend` | Svix signature | 600 |
| GET | `/api/health`: `{"status":"healthy","version":"2.1.2"}`, no database work | – | |
| GET | `/api/health/ready`: 503 if a critical probe fails | – | |
| GET | `/api/health/detailed`: database latency, cache, SSE and usage counters. Public; metric-shaped only, never an identifier | – | |
| GET | `/install.sh` (CameraNode installer, Linux/macOS), `/mcp-setup.sh`, `/mcp-setup.ps1` | – | 30 |
| GET | `/downloads/{os}/{arch}`: 302 to the matching CameraNode release asset | – | 60 |
| GET | `/.well-known/security.txt`, `/security.txt` | – | |
| GET | `/api-docs`, `/api-redoc`, `/api/openapi.json` (off under Fly) | – | |
| POST | `/mcp/`: the MCP server | MCP or agent key | 600 |

## MCP server

`/mcp/` serves the [Model Context Protocol](https://modelcontextprotocol.io) through rmcp's streamable-HTTP transport, stateless, answering in JSON. Before any request reaches it, `mcp/pre_auth.rs` requires `Content-Length`, caps the body at 2 MB and limits callers to 600 requests a minute. rmcp's DNS-rebinding guard is **off** (`app.rs`): its default allows only a localhost `Host`, which would refuse every real client, and every call carries a key anyway.

**The scope gate** (`mcp/scope.rs`, applied in `mcp/server.rs`) runs before `tools/list` and every `tools/call`:

- It looks the bearer key up and computes the allowed tools from the key's `scope_mode`: `all` (or NULL) is every tool; `readonly` is the 16 read tools; `custom` is the intersection of `scope_tools` with the real tool list, so a typo cannot enable anything.
- Agent keys (`osa_` and the shared key) get the **agent allowlist**: every tool except `set_camera_recording_policy`, because an agent steered by what a camera sees must not be able to turn the camera off.
- A key it does not recognise (missing, mistyped, revoked) gets a JSON-RPC error, even for `tools/list`. It is not an HTTP 401, because in MCP a 401 sends clients into OAuth discovery.
- Tool arguments containing a NUL are refused. Every call is logged to `mcp_activity_logs`, including refused ones.
- Rate limits per key: Pro 30/minute and 5,000/day; Pro Plus and self-hosted 120/minute and 30,000/day. Free plans have no MCP access.

**Tools (23).**

| Read (16) | Write (7) |
| --- | --- |
| `list_cameras`, `get_camera`, `get_stream_url`, `view_camera` (live JPEG), `watch_camera` (2–10 frames, 1–30 s apart), `list_camera_groups`, `list_nodes`, `get_node`, `get_camera_recording_policy`, `get_stream_logs`, `get_stream_stats`, `get_system_status`, `list_incidents`, `get_incident`, `get_incident_snapshot`, `get_incident_clip` | `create_incident`, `add_observation`, `attach_snapshot`, `attach_clip` (from the live cache), `update_incident`, `finalize_incident`, `set_camera_recording_policy` |

`limit` arguments are clamped to 1–500 and `offset` to 0 or more. Live frames come from the node over the WebSocket (`mcp/snapshot.rs`), so `view_camera` needs a connected node.

## Plans and limits

`plans.rs` is the source of truth. Three hosted tiers, plus `self_host` for local-auth installs.

| | Free (`free_org`) | Pro (`pro`) | Pro Plus (`pro_plus`) | Self-hosted |
| --- | --- | --- | --- | --- |
| Cameras | 5 | 25 | 200 | 999 |
| Nodes | 2 | 10 | 999 | 999 |
| Seats | 2 | 10 | 20 | 1 |
| **Viewer-hours per month** | 30 | 300 | 1,500 | unlimited (999,999) |
| Live (SSE) connections per stream type | 10 | 30 | 100 | 50 |
| Log retention | 30 days | 90 days | 365 days | 365 days |
| Sentinel AI runs per month | – | 100 | 500 | 500 (with licence) |
| MCP | – | 30/min, 5k/day | 120/min, 30k/day | 120/min, 30k/day |

- **Viewer-hours are the real tier axis** ([ADR 0002](docs/adr/0002-viewer-hour-billing.md)). Each served segment adds a second to an in-memory counter. The counter is flushed to `org_monthly_usage` every 60 seconds, so the hot path never waits on the database. Past the cap, segment requests get 429 with `Retry-After: 3600`. `GET /api/nodes/plan` reports the usage.
- **Plan resolution.** `resolve_org_plan` reads the `org_plan` setting and falls back to Clerk for orgs without one. `effective_plan_for_caps` returns `free_org` instead once an org has been past due for more than **7 days**. Short card failures don't punish anyone; long-unpaid accounts lose Pro limits.
- **Camera cap.** `enforce_camera_cap` keeps the oldest N cameras and flags the rest `disabled_by_plan`. Upgrading clears the flags in the same call. It runs on subscription webhooks, on every registration, and on heartbeats while an org is past due.
- **Sentinel AI** is available on Pro, Pro Plus and licensed self-hosted installs. Caps reset on the 1st of each month (UTC). At the cap, dispatch pauses, and nothing else is affected.

## Webhooks

Both endpoints verify a Svix signature and dedupe on the Svix message ID (`processed_webhooks`), so retries are harmless.

**`POST /api/webhooks/clerk`** (refused when `CLERK_WEBHOOK_SECRET` is unset):

| Event | Effect |
| --- | --- |
| `subscription.created` / `updated` / `active`, `subscriptionItem.active` | store `org_plan`, set the org's Clerk member limit, `enforce_camera_cap` (`subscriptionItem.active` on a paid plan also clears past-due) |
| `subscription.pastDue`, `subscriptionItem.pastDue` | set `payment_past_due` and its timestamp (the 7-day grace starts) |
| `paymentAttempt.updated` with `status=paid` | clear past-due, `enforce_camera_cap` |
| `subscriptionItem.canceled` / `ended` | back to `free_org`, `enforce_camera_cap` (cameras are kept, not deleted) |
| `subscriptionItem.freeTrialEnding` | logged only |
| `organization.created` | `welcome` notification |
| `organization.deleted` | erase all the org's data (the same function as full-reset) |
| `organizationMembership.created` / `updated` / `deleted` | `member_added` / `member_role_changed` / `member_removed` notification for admins |

Clerk only sends what the endpoint subscribes to. Its configuration is in the Clerk dashboard (Webhooks); the event list is in [LAUNCH_HANDOFF.md](docs/LAUNCH_HANDOFF.md).

**`POST /api/webhooks/resend`**: `email.bounced` and `email.complained` add the address to `email_suppression`, and the worker never mails it again, for any org.

## Background loops

Started from `main.rs`. All are `tokio` tasks in the `app` process.

| Loop | Where | Every | Does |
| --- | --- | --- | --- |
| Offline sweep | `loops.rs` | 30 s | Nodes and cameras silent for 90 s go `offline`, with a notification each. |
| Log cleanup | `loops.rs` | 24 h | Deletes logs older than each org's retention; terminal outbox rows after 7 days; webhook dedup markers after 30 days. |
| Sentinel reaper | `loops.rs` | 5 min | `running` runs older than 20 min become `error`; `pending` runs unclaimed for 2 min re-fire the wakeup; after 6 h they are abandoned. |
| Motion digest | `loops.rs` | 60 s | Closes each camera's motion-email cooldown and sends one digest for what happened inside it. |
| Disk check | `loops.rs` | 5 min | At 95 % full, one `tracing::error!` (a Sentry alert) per 6 h. Operator-only: never a customer notification. |
| Plan reconcile | `loops.rs` | 1 h | Clerk mode only: re-reads plans from Clerk in case a webhook was missed. |
| Licence check-in | `loops.rs` | 15 min | Local mode with a licence key. |
| Cloud sync | `loops.rs` | 30 min | Local mode with the sync entitlement. |
| Viewer-usage flush | `hls.rs` | 60 s | Writes viewer-seconds to `org_monthly_usage`. |
| Stale-camera eviction | `hls.rs` | 60 s | Drops caches of cameras that stopped pushing. |
| Release refresh | `versions.rs` | 10 min | Fetches the latest CameraNode release for "update available". |
| Email worker | `email_worker.rs` | 5 s | Drains the outbox; checks suppression first; reclaims rows stuck sending for 60 s; Resend idempotency keys prevent duplicates. |

## Notifications and email

`notifications.rs::create_notification` writes an inbox row (unless the org turned that kind off), broadcasts it to SSE subscribers, and queues an email if the kind's email toggle is on.

**Kinds:** `motion`, `motion_digest`, `camera_online`, `camera_offline`, `node_online`, `node_offline`, `incident_created`, `mcp_key_created`, `mcp_key_revoked`, `sentinel_agent_key_created`, `sentinel_agent_key_revoked`, `cameranode_disk_low`, `member_added`, `member_role_changed`, `member_removed`, `member_promotion_requested`, `welcome`. Each has an inbox toggle; most have an email toggle.

- **Transitions.** A camera or node is announced going offline from any non-offline state, and coming back online only from `offline`. A camera that is `streaming` counts as online. Register, HTTP heartbeat, WebSocket heartbeat and the sweep all follow this one rule.
- **Motion email** has a per-camera cooldown: the first event emails at once, and later ones within the window become one digest. The cooldown claim is not atomic, so two simultaneous first events can each send a mail. That is accepted: a duplicate beats a lost alert.
- **Email** goes through the `email_outbox` table, so it survives restarts. Templates are minijinja with autoescape, compiled in. Unsubscribe links carry a signed token, derived from `CLERK_SECRET_KEY` (or `APP_SECRET_KEY` in local mode).

## Conventions

- **Tenant isolation.** Every query is scoped by the `org_id` of the authenticated credential. The two-org isolation check (every route, MCP tool, node route and WebSocket message, aimed at another org's IDs) passes.
- **Key order is on the wire.** `serde_json` uses `preserve_order`. `Map::remove` is then a *swap*-remove; use `shift_remove`.
- **Validation lives in `query.rs`**, which reproduces FastAPI's 422 shape and parameter order. Use its helpers rather than serde's rejection, which would answer 400.
- **Races** are closed with a conditional `UPDATE` or `db::lock_for_update`, not check-then-write.
- **Errors that reach Sentry** are `tracing::error!`. Don't log expected refusals at ERROR.
- **Tests** live beside the code (`src/**`) and in `backend-rs/tests/`. Anything touching the database gets a `*_db.rs` test, which runs on PostgreSQL with `TEST_DATABASE_URL` and always under `--features sqlite`.
- **Frontend**: `useSharedToken` shares the Clerk token with HLS.js; `HeartbeatBanner` polls a new node until its first heartbeat; `WelcomeHero.jsx` shows admins a setup checklist and members a capability tour.

## History: the Rust rewrite

The backend was Python (FastAPI) and the agent a Python worker (LiteLLM and the MCP SDK) until the rewrite. It merged to `master` on 2026-10-05 (PR #348). The port was a strangler: Rust took the port first and proxied what it had not yet absorbed, and each slice was checked against the running Python before it moved.

- **Verification.** `backend-rs/tests/differential/` ran both stacks against one database and compared responses **and** table contents. The final run before the Python was deleted: reads 592/592, writes 729/729, MCP 150/150, background loops 7/7, SSE 29/29, HLS 49/49, WebSocket 20/20, plans 34/34. The agent was compared on all three provider wires (Ollama 20/20, OpenAI 15/15, Anthropic 12/12).
- **The record.** Those harnesses stay in the tree as the record and run against the commit before the deletion. [backend-rs/README.md](backend-rs/README.md) says what replaced them.
- **Bugs found along the way.** `backend-rs/PYTHON_BUGS.md` lists the Python bugs the port found. Most were fixed in Rust once the two no longer had to agree.

After the merge, the live production check (2026-10-05) confirmed the following on `app.sentinel-command.com` with a real Clerk user:

- sign-in and every main API read;
- node registration, video push and dashboard playback;
- a snapshot over `wss://`;
- incidents and notifications;
- Clerk's `organization.deleted` webhook erasing an org.
