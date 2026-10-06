# AGENTS.md

Sentinel Command Center — cloud dashboard for managing and viewing security cameras under the **Sentinel by SourceBox** product brand. Rust (axum) backend + React 19 frontend with Clerk authentication, on PostgreSQL (hosted) or SQLite (self-hosted default). There is no Python in this repository: the backend was FastAPI and the AI agent was a LiteLLM worker until the rewrite; see [Repository layout](#repository-layout--one-app-two-process-groups). Live video is streamed through an in-memory segment cache — **no Tigris, no S3, no presigned URLs in the live path**.

> **Brand-history note for grep-discoverability:** the product has carried three names — `OpenSentry` (early), `SourceBox Sentry` (mid), and `Sentinel by SourceBox` (current, from May 2026 onward). The `Sentinel AI` name is reserved specifically for the AI-agent feature. Both GitHub repos were renamed in May 2026: Command Center `OpenSentry-Command` → `Sentinel-Command`, and CameraNode `opensentry-cloud-node` → `Sentinel-CameraNode` (note the deliberate "CameraNode" — the repo name now describes the artifact more literally, while the binary, install paths, and product UI keep saying "CameraNode"). GitHub auto-redirects the old URLs, so any hardcoded reference in a release artifact / cached doc / external bookmark continues to resolve. Identifiers preserved verbatim across the rebrands (do **not** rename these without a migration plan): the env-var prefix `SOURCEBOX_SENTRY_*`, the Windows install path `C:\ProgramData\SourceBoxSentry\`, and the production hostname `sentinel-command.com` (tied to the Fly app, decoupled from the repo rename).

Two identifiers that this list previously claimed were preserved **were renamed on 2026-09-09**, while the product had zero installs — the only window in which either is free:

- The binary, `sourcebox-sentry-cloudnode` → `sourcebox-sentry-cameranode`. It had lagged the repo by a full brand, so release assets shipped under a dead name and the installer carried a rename workaround to hide it.
- The AES key-derivation domain, `opensentry-cloudnode-machine-id-v2` → `opensentry-cameranode-machine-id-v2` (CameraNode `database.rs::KEY_DOMAIN_V2`). **This list already recorded the `cameranode` spelling, which was simply wrong** — the code had always said `cloudnode`. It is accurate now, but was not before, so do not treat a match here as evidence.

That second one is the dangerous kind: it is a domain separator, and changing it means an existing encrypted `node.db` silently fails to open — no error, it just does not decrypt. From the first real install onward it needs a migration path, not an edit.

## Contents

Long reference — jump rather than scroll.

| | |
| --- | --- |
| [Repository layout](#repository-layout--one-app-two-process-groups) — one app, two process groups | [Authentication](#authentication) — six credential types |
| [Build & Run](#build--run) | [Data Models](#data-models) |
| [Configuration](#configuration) | [API Routes](#api-routes) |
| [Project Structure](#project-structure) | [MCP Server](#mcp-server) — tools, scope middleware |
| [Architecture](#architecture) — request flow, video pipeline | [Plan Enforcement](#plan-enforcement) |
| [CORS](#cors) · [Rate Limiting](#rate-limiting) | [Background Loops](#background-loops) |
| [Webhook Handling](#webhook-handling) | [Key Patterns](#key-patterns) |
| [Setup Scripts](#setup-scripts) · [Key Dependencies](#key-dependencies) | [Development Notes](#development-notes) |

Wider than this file: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) covers how Command Center relates to the other services. The AI agent has its own reference at [docs/SENTINEL_AGENT.md](docs/SENTINEL_AGENT.md).

## Repository layout — one app, two process groups

Command Center and the Sentinel AI agent ship from **one repo, one image, one deploy**. They run as two Fly *process groups* on separate machines, not as two apps.

| | Command Center | Sentinel AI agent |
| ------ | -------------- | ----------------- |
| Language | **Rust** (axum) | **Rust** (rig + rmcp client) |
| Code | `backend-rs/` + `frontend/` | `backend-rs/src/agent/` — same crate |
| Process group | `app` | `agent` |
| Command | `/usr/local/bin/sentinel-command` | `/usr/local/bin/sentinel-agent` |
| Machine | 1 GB, always-on, owns the volume | 512 MB, always-on, no volume |

**Both groups are binaries from one crate.** Command Center was FastAPI
and the agent was a Python worker on LiteLLM and the Python MCP SDK; both
are Rust now, and the image carries no Python runtime. The agent was
ported last and on its own evidence — `tests/differential/agent_run.sh`
ran both agents against one Command Center and one scripted model on all
three provider wires (Ollama 20/20, OpenAI 15/15, Anthropic 12/12). The
four places it deliberately differs from the Python are listed in
[docs/SENTINEL_AGENT.md](docs/SENTINEL_AGENT.md#what-the-port-changed).

How the rewrite was verified, since the reference it was checked against
no longer exists: `backend-rs/tests/differential/` drove both stacks
against one database and compared responses AND table contents, slice by
slice. The final run before the Python was deleted was 592/592 on reads
and 729/729 on writes, with MCP 150/150, the seven background-loop bodies
7/7, SSE 29/29, HLS 49/49, WebSocket 20/20 and plans 34/34. Those
harnesses stay in the tree as the record; they run against the commit
before the deletion. `backend-rs/README.md` says what replaces them.

Both are the `sentinel-command` Fly app, built from the root `Dockerfile` and deployed by `.github/workflows/deploy.yml`. There is no separate agent app, agent image, agent workflow, or agent lockfile.

Five rules follow, and breaking any of them breaks a deploy:

1. **One `Cargo.toml` builds both binaries, so a dependency change is a change to both.** `rig-core` and `rig-reqwest` are the agent's alone and are pre-1.0: a 0.x minor is a breaking release, the wire shapes they produce are pinned only by the agent differential (which CI cannot run — it needs the deleted Python), and so Dependabot is told to leave their minors alone (`.github/dependabot.yml`). Move them by hand, with `agent_run.sh`.
2. **`[[mounts]]` must stay scoped to `processes = ["app"]`.** Unscoped, it applies to every group and the agent machine fails to boot fighting for the volume's single attachment slot.
3. **`[processes]` overrides the Dockerfile `CMD`.** The `app` command in `fly.toml` must stay in sync with that `CMD`. Both are now `/usr/local/bin/sentinel-command`, with no arguments — uvicorn's flags are gone, and `fly.toml` records why each one was not replaced rather than leaving that to be rediscovered. The `agent` command is `/usr/local/bin/sentinel-agent`.
4. **The required checks are named, so renaming a CI job hangs every PR.** `master` requires `Backend tests (sqlite)`, `Backend tests (postgres)` and `Frontend audit + build` by exact name, and `deploy.yml` produces all three (plus `Backend tests (no database)`). The sqlite leg builds `--features sqlite` — a different binary — and runs the DB-gated tests on a fresh file. There is no agent job: the agent is in the crate the backend legs already test. GitHub reports *no status at all* for a check that never runs, so renaming one leaves every PR waiting on it, presenting as a stuck check rather than a config error.
5. **CI path filtering is asymmetric.** `push` is filtered (docs and Markdown only); `pull_request` is **never** filtered. The required checks are named above, and GitHub reports *no status at all* for a workflow a path filter skipped — so a filtered PR trigger would hang every PR that missed it, presenting as a stuck check rather than a config error.

The agent runs as a separate **process group** — its own machine, kept warm rather than scaled to zero. Both choices are deliberate and both have non-obvious reasons: memory contention with the segment cache, and a boot time that lost a race with Fly's proxy. The second reason belonged to the Python agent and no longer holds, but scale-to-zero has not been switched on, because that race can only be observed in production. Neither is restated here; see [docs/SENTINEL_AGENT.md](docs/SENTINEL_AGENT.md) for the agent's side and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#deployed-services-flyio) for how it compares to the services that *do* sleep.

Self-hosting still works the same way: `sentinel-agent` runs standalone with `AGENT_MODE=poll` and a per-org `osa_` key, needing no inbound connectivity. Agent docs are in `docs/SENTINEL_AGENT.md`. The code came from the `SourceBox-Sentinel` repo (archived 2026-09-09).

## Build & Run

**Prerequisites:** Rust (the toolchain in `Dockerfile`'s builder stage — pinned, not `latest`), Node 18+. No Python.

```bash
# Command Center
cd backend-rs
cargo run                            # http://localhost:8000

# Tests — no database needed; the DB-gated ones skip themselves
cargo test
# With one, so they do not skip. Its OWN database: these tests insert
# rows, and sharing one with a running differential harness corrupts the
# harness's fixture mid-case:
TEST_DATABASE_URL=postgresql://cc:cc@127.0.0.1:15434/cc_unit cargo test
# The SQLite build — its DB-gated tests always run, on a fresh file:
cargo test --features sqlite

cargo clippy --all-targets           # kept at zero warnings, both builds

# Frontend
cd frontend
npm install
npm run dev                          # http://localhost:5173
npm run build                        # Production build → frontend/dist/,
                                     # copied to /app/static in the image
```

**Self-hosted (no Clerk) instead — `docker-compose.yml` at the repo root:**

```bash
cp backend-rs/.env.example .env     # fill in LOCAL_ADMIN_USERNAME / _EMAIL
openssl rand -hex 32                                   # → APP_SECRET_KEY
docker compose run --rm --no-deps app sentinel-hash-password
                                           # → LOCAL_ADMIN_PASSWORD_HASH
docker compose up -d                                   # localhost:8000
```

**Put the hash in SINGLE QUOTES in `.env`.** An argon2 PHC string is full
of `$` and Compose interpolates `.env` values, so unquoted
`$argon2id$v=19$m=65536` arrives as `=19=65536` — a login that fails for a
reason nothing explains. Verified both ways; the compose file says so at
the top.

`docker-compose.sqlite.yml` is the smaller alternative: one container, the
database a file on its volume, no Postgres at all — the shape the Python
tier's self-hosting had. Same commands with `-f docker-compose.sqlite.yml`.
The hash step runs the image's own tool in both, so the one credential
this mode cannot start without needs no toolchain to produce.

Without Docker, the same thing by hand:

```bash
cd backend-rs
cargo run --bin sentinel-hash-password    # prints LOCAL_ADMIN_PASSWORD_HASH
# or, non-interactively: echo -n 'secret' | … --stdin
# Set in the environment: AUTH_PROVIDER=local, APP_SECRET_KEY=<random 32+ bytes>,
# LOCAL_ADMIN_USERNAME, LOCAL_ADMIN_PASSWORD_HASH (from above),
# LOCAL_ADMIN_EMAIL. DATABASE_URL may be left unset: sqlite:///./sentinel.db
cargo run --features sqlite        # or plain `cargo run` with a postgresql:// URL

# Set in frontend/.env: VITE_AUTH_PROVIDER=local (VITE_CLERK_PUBLISHABLE_KEY not needed)
cd frontend && npm run dev
```

On a deployed machine both operator tools are on `PATH`:

```bash
fly ssh console -a sentinel-command -C sentinel-hash-password
fly ssh console -a sentinel-command -C "sentinel-restore-from-cloud --list"
```

**The agent** is a second binary in the same crate, so `cargo test` above
already covers it:

```bash
cd backend-rs
OLLAMA_API_KEY=… SENTINEL_AGENT_KEY=dev-secret AGENT_MODE=poll \
OPENSENTRY_API_BASE=http://localhost:8000 WEBHOOK_VERIFY_SIGNATURE=false \
  cargo run --bin sentinel-agent    # :8080; reads .env in the working directory
```

See Authentication → "Local auth (self-hosted)" below for what this mode does and doesn't enable.

## Configuration

Backend config is loaded from environment variables (see `backend-rs/.env.example`).

**Required (`AUTH_PROVIDER=clerk`, the default):**
- `CLERK_SECRET_KEY` / `CLERK_PUBLISHABLE_KEY` — Clerk auth

**Required (`AUTH_PROVIDER=local`, self-hosted):**
- `APP_SECRET_KEY`, `LOCAL_ADMIN_USERNAME`, `LOCAL_ADMIN_PASSWORD_HASH`, `LOCAL_ADMIN_EMAIL` — see Authentication → "Local auth (self-hosted)"

**Optional:**
- `CLERK_WEBHOOK_SECRET` — Svix signature for Clerk subscription + organizationMembership webhooks
- `DATABASE_URL` — `postgresql://…` / `postgres://…`, or `sqlite:///path`
  (SQLAlchemy's spelling, which the Python tier documented: three slashes
  relative, four absolute). **Unset, it is `sqlite:///./sentinel.db`** —
  the Python tier's default, kept. A `postgresql+psycopg://` URL has its
  driver suffix stripped, so the existing Fly secret works unchanged
  (`config.rs::normalize_database_url`).
  - **Hosted production** runs the shared `sentinel-postgres` cluster — one cluster, one database per service (`sentinel_command`, `sentinel_license`, `sentinel_sync`). Set as a Fly *secret*, not in `fly.toml`, because it carries a password. **The roles are hardened**, which is not Fly's default: `fly postgres attach` creates every role `SUPERUSER`, which would let any one service's credential read and write the others' databases. Each database is instead owned by its own role, `CONNECT` is revoked from `PUBLIC`, and the roles are `NOSUPERUSER` — verified: all six cross-database connection attempts are refused while each service still reaches its own. **Any future `fly postgres attach` recreates a superuser role and must be re-hardened** (see `docs/runbooks/DISASTER_RECOVERY.md`). A pooled `PgPool` (10 connections, 10s acquire timeout), not a fresh connection per request, because the database is across a network.
  - **SQLite** — the self-hosted default, as it was under the Python tier.
    The driver is chosen at BUILD time (`--features sqlite`, `src/db.rs`
    says why), the image ships both builds, and `sentinel-command` replaces
    itself with `sentinel-command-sqlite` for a `sqlite://` URL — so the
    choice is the URL and nothing else. Same pragmas the Python set: WAL,
    `synchronous=NORMAL`, 30 s `busy_timeout`, foreign keys on. The schema
    (`migrations-sqlite/`) is what the Python models produced on SQLite, so
    a `sentinel.db` from the last Python release opens as it is — checked
    by creating one with those models and serving it.
    - Held to the PostgreSQL build by `tests/differential/dialect_*.sh`:
      reads 590, writes 729 (with table contents), MCP 150, WS 20, HLS 49,
      SSE 29, the seven loop bodies, the email worker 21, and a four-way
      cloud-mirror round trip.
    - Writing SQL for both is a short list of rules — `backend-rs/README.md`
      › "Two databases". The two that bite: `CAST()` not `::`, and every
      ORDER BY over a nullable column says `NULLS LAST`/`NULLS FIRST`,
      because the engines sort NULL at opposite ends (a unit test enforces it).
    - Hosted production stays on PostgreSQL. A hosted machine with no
      `DATABASE_URL` would start on an empty file, which is why it is a
      Fly secret that must stay set.
- `FRONTEND_URL` — extra CORS origin (must have scheme, no trailing slash)
- `TRUST_PROXY_HEADERS` — take the client address from `Fly-Client-IP` / `X-Forwarded-For`. Default: on under Fly (`FLY_APP_NAME` set), whose edge sets the first and strips any copy a client sent; **off** elsewhere, where both are the caller's own claim and the TCP peer is used. A self-hoster behind their own reverse proxy turns it on. It decides the rate-limit bucket and the IP in the audit trail.
- `REDIS_URL` — shared rate-limiter storage (the Python used it through slowapi; `ratelimit.rs` uses the same keys, so counters survive the rewrite). Without it, per-process in-memory counters (single-VM safe; multi-VM round-robins around the limit). Currently in production via Upstash on Fly.
- `SEGMENT_CACHE_MAX_PER_CAMERA` — segments cached in memory per camera (default **60**, ~60s — CameraNode emits 1-second segments)
- `SEGMENT_CACHE_MAX_TOTAL_BYTES` — global byte ceiling across all camera caches (default **384 MiB**, in both the Python and the Rust; this line said 2 GiB for a long time and was never true). When exceeded, `hls.rs` evicts oldest segments across ALL cameras until back under cap.
- `SEGMENT_PUSH_MAX_BYTES` — max bytes per pushed segment (default 2 MB)
- `PLAYLIST_PUSH_MAX_BYTES` — max bytes per pushed playlist (default 64 KB)
- `CLEANUP_INTERVAL` — run cache eviction every N playlist updates (default 20)
- ~~`INACTIVE_CAMERA_CLEANUP_HOURS`~~ — **not read.** The Python's daily loop freed caches for cameras offline this long; the 60-second stale sweep in `hls.rs` (`evict_stale_cameras`) drops a camera's segments, playlist and counters a minute after it stops pushing, which leaves the daily pass nothing to do. Setting it has no effect.
- ~~`LOG_RETENTION_DAYS`~~ — **not read, and it never did anything.** Retention is per plan (Free 30 / Pro 90 / Pro Plus 365, in `plans.rs`). The Python took this as a fallback for an org whose plan could not be resolved, but `get_plan_limits` falls back to the free tier's whole dict, so the fallback was unreachable; the port dropped the parameter rather than carry a knob that cannot change an answer.
- `OFFLINE_SWEEP_INTERVAL_SECONDS` — how often to mark stale rows offline (default 30)
**Sentinel AI agent (the gated agent feature):**
- `SENTINEL_AGENT_KEY` — shared secret for the run-queue API (`X-Sentinel-Agent-Key`) and the HMAC on outbound `/wakeup` webhooks. Must match the agent's own `SENTINEL_AGENT_KEY`. ⚠️ **Multi-tenant** — its holder can drain every org's queue. Never give it to a customer; issue a scoped `osa_` key from **MCP → Sentinel Agent Keys** instead. Leaving it unset disables only the first-party agent path — scoped keys keep working, which is what a self-hosted Command Center wants.
- `SENTINEL_AGENT_MCP_KEY` — the agent's bearer for the MCP tool surface. Distinct from the above on the first-party deployment; a scoped `osa_` key authenticates both.
- `SENTINEL_AGENT_WEBHOOK_URL` — where Command Center fires the wakeup. Now an **internal** address: `http://sentinel-command.flycast:8080/wakeup`, the `agent` process group of this same app over 6PN. Hitting a Fly *service* is what auto-starts the stopped agent machine — a bare 6PN connection to a stopped machine just fails — so the webhook both wakes the worker and delivers the work. Unset means no webhook is sent; a self-hosted agent instead polls, so this is only needed for the push topology.
- `SENTINEL_DISPATCH_ENABLED` — kill switch for creating new runs. Turn it off to stop dispatch without touching plans or licences.
- `SENTINEL_GLOBAL_MONTHLY_RUN_CAP` — a fleet-wide ceiling on runs per month, on top of the per-plan caps. Backstop against a runaway loop billing you across every org at once.
- `SENTINEL_LICENSE_SERVICE_URL` / `SENTINEL_SYNC_SERVICE_URL` — the sibling services, defaulting to the hosted `https://sentinel-license.fly.dev` / `https://sentinel-sync.fly.dev` (the compose files pass only `SENTINEL_LICENSE_KEY`, so a self-hoster never sets these). See `docs/runbooks/DISASTER_RECOVERY.md` for how they fit together.
- `API_DOCS_ENABLED` — serve `/api-docs`, `/api-redoc` and `/api/openapi.json`. Default: off on Fly (`FLY_APP_NAME` set), on elsewhere.

**Node versions:**
- `MIN_SUPPORTED_NODE_VERSION` — CameraNodes below this are refused. `LATEST_NODE_VERSION` (above) is the cold-boot fallback for the "update available" check.

- `SENTRY_DSN` — error tracking. In production this is managed by the Fly Sentry extension (`fly ext sentry create -a sentinel-command`) which provisions a sponsored Team plan and auto-injects the secret; you rarely set this by hand. `sentry.rs::init()` is a no-op when the DSN is absent, so local dev needs no extra config. Dashboard: `fly ext sentry dashboard -a sentinel-command`.
  - **It is wired through `tracing`, which makes an `error!` an alert.** ERROR
    becomes a Sentry event, INFO and WARN become breadcrumbs — the Python's
    `LoggingIntegration(level=INFO, event_level=ERROR)`. That is the delivery
    path the disk-critical alert depends on: `loops::check_disk_critical`
    fires one `tracing::error!` and deliberately does NOT go through customer
    notifications. `tower_http::trace`'s own failure log is demoted to a
    breadcrumb, because otherwise every 5xx arrived twice.
  - Query strings are stripped from every event before it leaves the process
    (`sentry.rs::scrub`) — a URL is not PII to the SDK, and the pre-v0.1.65
    CameraNode WebSocket handshake puts `api_key=` in one. Five header names
    are redacted as a second line of defence behind `send_default_pii=false`.
  - `SENTRY_TRACES_SAMPLE_RATE` (default 0.1), `SENTRY_ENVIRONMENT` (else
    `production` when `FLY_APP_NAME` is set, `development` otherwise) and
    `SENTRY_RELEASE` (else `FLY_MACHINE_VERSION`).

**Email (Resend, optional):**
- `EMAIL_ENABLED` — global kill-switch (default `false`). Code can ship with it off; flip to `true` once DNS propagates and a smoke test passes. Worker still runs when off; transport short-circuits with a logged "would have sent" line.
- `RESEND_API_KEY` — Resend transactional API key (`re_…`)
- `RESEND_WEBHOOK_SECRET` — Svix signing secret for the `/api/webhooks/resend` bounce/complaint handler
- `EMAIL_FROM_ADDRESS` — default `notifications@sentinel-command.com` (must be on a Resend-verified sending domain; sentinel-command.com is verified — DKIM + SPF/Return-Path on `send.sentinel-command.com` — sourceboxsentry.com is not). No-reply by design: no Reply-To is set — support is a separate proactive channel (`support@sentinel-command.com`).
- `EMAIL_FROM_NAME` — default `Sentinel by SourceBox`
- `EMAIL_TEMPLATES_DIR` — overrides the compiled-in templates with a
  directory. Unset in production, which is the point: the 46 templates are
  `include_str!`d from `backend-rs/templates/emails/`, so a missing or
  renamed one is a compile error rather than a render failure on the first
  send. Set it only to preview a template edit without rebuilding.
- `EMAIL_WORKER_INTERVAL_SECONDS` — outbox-drain tick interval (default 5)
- `EMAIL_WORKER_BATCH_SIZE` — max rows drained per tick (default 20)
- `EMAIL_MAX_ATTEMPTS` — retries before a row is permanently failed (default 3)

Frontend config: `VITE_AUTH_PROVIDER` (`clerk` default or `local`), `VITE_CLERK_PUBLISHABLE_KEY`, `VITE_API_URL`, `VITE_LOCAL_HLS`.

## Project Structure

```
backend-rs/                       # Command Center AND the agent. One crate, four binaries.
├── src/
│   ├── main.rs                   # entrypoint: pool, migrations, loops, serve
│   ├── app.rs                    # the route table, the SPA fallback, the MCP
│   │                             # mount and its 307. READ THIS FIRST — every
│   │                             # route the service answers is named here.
│   ├── api/                      # one file per router, mirroring the old
│   │   │                         # app/api/ split so the two can be compared
│   │   ├── cameras.rs  groups.rs  nodes.rs  node_register.rs
│   │   ├── hls.rs                # playlist + segment cache + push-segment
│   │   ├── incidents.rs  motion.rs  notifications.rs  sentinel.rs
│   │   ├── integration.rs        # Home Assistant, `osi_` keys
│   │   ├── keys.rs  mcp_activity.rs  audit.rs  stream_logs.rs
│   │   ├── install.rs            # serves scripts/ from SCRIPTS_DIR
│   │   ├── clerk_webhook.rs  resend_webhook.rs  local_auth.rs
│   │   ├── gdpr.rs  health.rs  sentinel_config.rs  well_known.rs
│   │   └── docs.rs               # /api-docs, /api-redoc, the harvested schema
│   ├── mcp/
│   │   ├── server.rs             # rmcp ServerHandler — replaces fastmcp
│   │   ├── tools.rs              # all 23 tools
│   │   ├── scope.rs              # the scope gate, rate limits, tool catalog
│   │   ├── auth.rs  activity.rs  snapshot.rs
│   ├── loops.rs                  # the background sweeps and their bodies:
│   │                             # offline sweep, log cleanup, reaper, motion
│   │                             # digest, disk check, plan reconcile
│   ├── sync.rs                   # the one-way mirror to Sentinel-Sync-Service
│   ├── license.rs                # the self-host licence gate + check-in
│   ├── plans.rs                  # PLAN_LIMITS, caps, grace, viewer hours
│   ├── notifications.rs          # the inbox, the broadcaster, email fan-out
│   ├── email*.rs                 # send, templates, worker, unsubscribe
│   ├── spa.rs                    # the React document and its pass-through list
│   ├── hls.rs                    # the in-memory segment cache (one owner)
│   ├── auth.rs / auth/           # Clerk JWT (V1 + V2) and the local-auth path
│   ├── sentry.rs                 # error tracking; the tracing bridge is what
│   │                             # turns an `error!` into an alert
│   ├── csv_export.rs             # the three ?format=csv exports, streamed
│   └── py*.rs                    # CPython semantics the port has to match
│                                 # exactly: json.dumps spacing, round()
│                                 # half-to-even, float() underscores,
│                                 # fromisoformat, int() coercion, str().
│                                 # Each one exists because a differential
│                                 # case failed on it.
│   ├── agent.rs / agent/         # the Sentinel AI agent: config, server,
│   │                             # processor, run (the loop), llm (rig, and
│   │                             # ALL provider-shaped knowledge), mcp_client
│   │                             # (rmcp), queue, prompts
│   ├── bin/
│   │   ├── agent.rs              # sentinel-agent — the `agent` process group
│   │   ├── hash_password.rs      # sentinel-hash-password
│   │   └── restore_from_cloud.rs # sentinel-restore-from-cloud
├── assets/agent/*.txt            # the agent's five prompts, compiled in
├── assets/openapi.json           # FastAPI's own document, harvested at port
│                                 # time and compiled in. See api/docs.rs.
├── templates/emails/             # the 46 Jinja templates — _layout.html.j2
│                                 # plus 15 kinds x (subject + txt + html).
│                                 # COMPILED IN, like the two above: they
│                                 # were read from disk under backend/app/
│                                 # and the deletion shipped an image with
│                                 # none of them, silently, for one commit.
├── migrations/                   # PostgreSQL schema, embedded at compile time
├── migrations-sqlite/            # SQLite schema — the Python models' own, read
│                                 # back out of sqlite_master. Same 21 tables.
├── src/db.rs                     # which database this build opens; the few
│                                 # helpers that differ (any/list, ILIKE)
├── examples/                     # probe pairs for code with no HTTP surface
└── tests/
    ├── differential/             # how the port was verified. Needs the Python,
    │                             # so it runs against the commit before the
    │                             # deletion — plus the checkers that do not:
    │                             # column_defaults, openapi_drift,
    │                             # ratelimit/auth parity. agent_run.sh is
    │                             # the agent's, with fake_llm.py as the
    │                             # scripted model.
    ├── agent_contract.rs         # the agent's /complete body vs. the handler
    └── *_db.rs                   # integration tests: Postgres when TEST_DATABASE_URL
                                  # is set; always, on a file, under --features sqlite

scripts/                          # served from SCRIPTS_DIR, or run by an operator
├── install.sh  mcp-setup.sh  mcp-setup.ps1
└── backup_db.sh  restore_db.sh   # PostgreSQL only (pg_dump; hence
                                  # postgresql-client-18). SQLite: copy the file

docs/                             # runbooks + ADRs, unchanged by the rewrite
frontend/                         # React 19. pages/SentinelPage.jsx (/sentinel)
                                  # is Sentinel AI's only UI: config, Run now,
                                  # run history. It was lost in July with the
                                  # marketing pages and restored; without it a
                                  # fresh org never runs Sentinel, because the
                                  # config row is created by its first read.
```

## Architecture

### Request flow

```
Browser ──Clerk JWT──→ axum ──SQL──→ PostgreSQL | SQLite
                          ↕
CameraNode ──X-Node-API-Key──→ axum ──RAM──→ in-memory segment cache
          ──WebSocket──────→                   + per-org motion/notification broadcasters
MCP Client ──Bearer osc_…──→ rmcp → mcp::scope gate → tools
```

### Video streaming pipeline

1. CameraNode generates HLS segments via FFmpeg (1-second `.ts` files by default; see `streaming.hls.segment_duration` in CameraNode's `config.yaml`)
2. CameraNode calls `POST /api/cameras/{id}/push-segment?filename=segment_NNNNN.ts` with the raw `.ts` body and `X-Node-API-Key` header
3. Backend stores the bytes in `_segment_cache[camera_id][filename]`, evicting the oldest once `SEGMENT_CACHE_MAX_PER_CAMERA` is exceeded
4. CameraNode calls `POST /api/cameras/{id}/playlist` with the rolling `stream.m3u8` text
5. Backend rewrites playlist segment filenames to relative `segment/<file>` proxy URLs and caches the result in `_playlist_cache`
6. Browser calls `GET /api/cameras/{id}/stream.m3u8` with JWT → served instantly from `_playlist_cache`
7. Browser fetches each segment via `GET /api/cameras/{id}/segment/{filename}` → served from `_segment_cache` in memory
8. Cache eviction sweeps every `CLEANUP_INTERVAL` playlist updates, and a 60-second timer drops every cache for a camera that has stopped pushing — so the advertised inactivity cutoff holds even when no node is pushing at all

### SPA serving

`spa.rs::fallback`, reached only when no route matched:
- `/api`, `/ws`, `/install.`, `/mcp-setup.`, `/downloads/`, `/.well-known/`,
  `/security.txt` → **404 `{"detail": "Not Found"}`**, never the React
  document. A 200 HTML page answering a CameraNode's request for a route
  that no longer exists is the failure this list prevents.
- `POST /mcp` → 307 to `/mcp/`; `POST /mcp/` → the rmcp transport. Both
  behind `mcp::pre_auth` (Content-Length required, 2 MB cap, 600/min).
- `GET /mcp` and every other method on it → the React `McpPage`, because
  Python's SPA middleware was outermost and never consulted a method table
  for that path. `app.rs::served_spa` reproduces it.
- `/assets/*` → `ServeDir` over the built SPA
- Everything else → `index.html` (SPA client-side routing)

A matched path with an unmatched method is axum's 405 — kept because it
computes `Allow` — with FastAPI's `{"detail": "Method Not Allowed"}` body
filled in by one layer (`app.rs::method_not_allowed_body`). `HEAD` on a GET
route is a 405, as FastAPI's `APIRoute` made it. `tests/routing.rs` pins all
of this.

`GET /docs` is owned by the React `DocsPage`; the auto docs live at
`/api-docs` (Swagger) and `/api-redoc` (ReDoc) and are served from the
harvested `assets/openapi.json` — see `api/docs.rs`; the schema is at
`/api/openapi.json`.

## Authentication

### Clerk JWT (browser users)

`Authenticator::authenticate` in `auth.rs`, reached through the `AuthUser`
extractor:
1. Extracts `Authorization: Bearer <token>`, falling back to the Clerk
   session cookie
2. Verifies it locally: RS256 against a cached JWKS. **No Clerk SDK** — the
   Python's SDK call was local verification too, wrapped in `to_thread`
   because it was sync.
3. Extracts `sub` (user_id), `org_id`, and permissions from JWT claims (V1 or V2 format)
4. Returns an `AuthUser` with `is_admin`, `permissions`, etc., and tags the
   per-request Sentry scope with `user_id` / `org_id` / `plan`

**V2 permission decoding** (`auth/claims.rs`):
- `o` claim contains org object with `fpm` (feature permission map) and `per` (permissions)
- `fea` claim contains feature list (e.g. `o:admin,o:cameras`)
- Decoded to `org:{feature}:{permission}` format

**Dependencies:**
- `require_view()` → any authenticated org member (no extra permission check)
- `require_admin()` → Clerk role `org:admin` / `admin`, or `org:cameras:manage_cameras` permission

### Local auth (self-hosted, `AUTH_PROVIDER=local`)

An alternate human-login mode for single-admin, fully self-hosted installs — no
Clerk account, no billing. Everything is unlocked EXCEPT Sentinel AI, which is
licensed separately (see below) since it's the one feature with a real ongoing
LLM cost regardless of who's running the dashboard. `Authenticator::from_config` picks, at startup, between the Clerk path above
and `auth/local.rs`'s local-JWT path; both produce the same `AuthUser` shape,
so every downstream route, `require_view`/`require_admin`/`require_active_billing`,
and the plan-enforcement engine work unmodified regardless of provider.

- **Login**: `POST /api/auth/local/login` (username/password against
  `LOCAL_ADMIN_USERNAME`/`LOCAL_ADMIN_PASSWORD_HASH`, an argon2 hash generated by
  the `sentinel-hash-password` binary) issues a 30-day HS256 JWT
  signed with `APP_SECRET_KEY`. `POST /api/auth/local/refresh` re-signs a
  still-valid token with a renewed expiry — the frontend calls this
  opportunistically so an open tab never actually hits the 30-day wall (this is
  a security-camera dashboard plausibly left open unattended).
- **One fixed admin, one fixed org** (`LOCAL_ORG_ID`, default `"self-host"`) — no
  local User/Organization table, no invite flow, no member management. `org_id`
  is an unconstrained string everywhere in the schema (see Data Models), so this
  just works.
- **Plan**: `resolve_org_plan()`/`effective_plan_for_caps()` in `plans.rs`
  short-circuit to a dedicated `"self_host"` `PLAN_LIMITS` tier the moment
  `AUTH_PROVIDER=local` — before touching `Setting` or Clerk at all. This is the
  choke point for every non-JWT plan lookup too (node registration, MCP,
  Sentinel dispatch), so patching only `get_current_user`'s `AuthUser.plan`
  would have missed all of those.
- **What's disabled in this mode**: `/api/webhooks/clerk` isn't mounted (no
  Clerk account to send webhooks); the hourly Clerk-plan-reconciliation loop
  isn't scheduled; `recipients.rs` returns `[LOCAL_ADMIN_EMAIL]` directly
  instead of querying Clerk org membership.
- **Sentinel AI license gate**: a genuinely separate service (sibling repo
  `Sentinel-License-Service`, not part of this codebase) validates a license
  key an operator configures via `SENTINEL_LICENSE_KEY`. `license.rs`
  checks in every 15 min (a loop in `loops.rs`),
  caching the verdict in the same `Setting` KV pair shape Clerk's billing
  webhook already uses (`sentinel_license_valid`, `sentinel_license_last_ok_at`,
  etc.) — no new table. Fail-open for `SENTINEL_LICENSE_GRACE_HOURS` (72h) if
  the service is unreachable, using the last known-good state; fail closed
  immediately (no grace) the moment the service is reachable and explicitly
  says no. The gate itself is `license.rs::sentinel_blocked_by_license(plan, db)`
  — a single shared predicate layered on top of the existing plan gate at four
  call sites: `can_dispatch_for_kind` and `dispatch_manual_run`
  (`sentinel_dispatch.rs`), `resolve_sentinel_access` (`api/sentinel.rs`),
  and the multi-tenant agent's MCP auth resolver
  (`mcp/auth.rs`), which independently re-checks the
  same plan set and needs the same license check for the same reason.
  `resolve_org_plan()`'s self_host short-circuit is untouched — camera caps,
  viewer-hours, MCP access, everything else self-host unlocks is unaffected.
- **Cloud data-sync tier**: a second, independent entitlement on the same
  licence (`sync_enabled`, returned by License-Service alongside the AI
  verdict), mirroring this install's data to `Sentinel-Sync-Service` (another
  sibling repo). One-way: the local database stays the source of truth and
  the app works with no internet at all. `sync.rs` pushes changed rows
  every 30 min (`loops::spawn_loops`), tracking a per-table high-water
  cursor in the same `Setting` KV table — again no new table.
  - Payload is **raw column values**, not `to_dict()`. That distinction is
    load-bearing: `to_dict()` is a display shape and using it made the mirror
    unrestorable (Camera lost 11 of 21 columns, `SentinelRun` never carried
    `tool_trace`). Exclusions are explicit in `_COLUMN_DENYLIST` —
    `camera_nodes.api_key_hash` and `incident_evidence.data` — and are checked
    *before* the attribute is read, because that blob column is `deferred()`
    and reading it would defeat the deferral.
  - Deletions propagate only for small identity tables (cameras, groups,
    nodes) via a `known_ids` reconciliation. Log/event tables deliberately
    never propagate deletes: local retention prunes them precisely because
    local disk is finite, and the cloud copy is meant to outlive that.
  - Recovery is the `sentinel-restore-from-cloud` binary — see
    `docs/runbooks/DISASTER_RECOVERY.md` for the procedure and for what it
    can't bring back (node API keys, evidence blobs, recordings).
- **Frontend**: `frontend/src/auth/` is a facade — `VITE_AUTH_PROVIDER=clerk`
  (default) re-exports `@clerk/clerk-react`'s hooks/components directly;
  `VITE_AUTH_PROVIDER=local` selects `frontend/src/auth/local.jsx`'s
  implementation instead, which fakes a stable single-org shape so the ~20
  files calling `useAuth()`/`useOrganization()` don't need to know which mode
  is active. `SignInPage`/`SignUpPage`/`PricingPage`/`Layout`'s `UserButton` +
  `OrganizationSwitcher` branch explicitly on `IS_LOCAL_AUTH` since those have
  no equivalent Clerk-hosted UI to fall back on.
- **Not the same login as a paired CameraNode's own local-admin auth**:
  Sentinel-CameraNode's `Local` mode has its own independent password
  (added the same session as this feature) protecting that node's own
  standalone browser dashboard when it's LAN-exposed. The two logins
  share no session, no password, and no trust boundary — an operator
  running both self-hosted has two separate credentials to remember.
  See that repo's `docs/runbooks/local-mode-setup.md` for the
  CameraNode-side detail.

### API key (CameraNode)

CameraNode endpoints validate `X-Node-API-Key`:
1. SHA-256 hash the provided key
2. Match against `api_key_hash` on `CameraNode`
3. Derive `org_id` from the matched node row

### MCP API key

MCP endpoint (`POST /mcp`) validates `Authorization: Bearer osc_<hex>`:
1. SHA-256 hash the raw key
2. Match against `McpApiKey.key_hash` with `revoked=False` AND `kind="mcp"`
3. `ScopeMiddleware` (see below) filters tool discovery + invocation per-key

### Integration API key

`/api/integration/*` endpoints (Home Assistant) validate `Authorization: Bearer osi_<hex>`
via `api/integration.rs`'s key resolver:
1. SHA-256 hash the raw key
2. Match against `McpApiKey.key_hash` with `revoked=False` AND `kind="integration"`
3. Derive `org_id` from the matched row

Integration keys share the `mcp_api_keys` table with MCP keys, split by `kind`
(`mcp` vs `integration`). The `kind` filter is applied to **every** `McpApiKey`
query — both auth paths and both management list/revoke endpoints — so the two
key kinds can never cross surfaces. The resolver takes a connection from the
pool and returns it before the stream starts, so the long-lived motion SSE
doesn't pin one for its lifetime. No plan gate — available on all tiers.

## Data Models

All 20 tables, as `migrations/0001_adopt_production_schema.sql` defines them
and `models.rs` maps them. Every one has `org_id` for tenant isolation EXCEPT `ProcessedWebhook` (global webhook dedup, keyed on Svix msg_id) and `EmailSuppression` (operator-global bounce / complaint list, intentionally cross-tenant so a hard-bounced address stops being mailed for every org).

| Model | Key Fields | Purpose |
|-------|------------|---------|
| `Camera` | `camera_id`, `node_id` (FK), `name`, `status`, `video_codec`, `audio_codec`, `group_id`, `last_seen` | Camera registered by a CameraNode; `effective_status` flips to offline after a 90s heartbeat gap |
| `CameraNode` | `node_id`, `api_key_hash`, `hostname`, `status`, `video_codec`, `audio_codec`, `last_seen`, `key_rotated_at`, `storage_*` | Physical CameraNode device + storage stats from heartbeat (drives `cameranode_disk_low` alert) |
| `CameraGroup` | `name`, `color`, `icon` | User-defined camera grouping |
| `Setting` | `key`, `value` | Per-org key/value store. Used for plan slug, recording config, email toggles, motion email cooldown anchors (`motion_email_cooldown_start:{camera_id}`), CameraNode disk debounce (`cameranode_disk_low_emit_at:{node_id}`), payment past-due flags. |
| `AuditLog` | `event`, `user_id`, `ip_address`, `details`, `(org_id, timestamp)` index | Security audit trail |
| `StreamAccessLog` | `user_id`, `camera_id`, `ip_address`, `user_agent`, `accessed_at`, `(org_id, accessed_at)` index | Stream playback audit |
| `McpApiKey` | `name`, `key_hash`, `kind` (`mcp`/`integration`), `scope_mode`, `scope_tools` (JSON text), `last_used_at`, `revoked` | Both MCP keys (`osc_`, AI agents) AND Home Assistant integration keys (`osi_`) — one table, split by `kind`; **scope_mode** (MCP only): `all` / `readonly` / `custom` |
| `McpActivityLog` | `tool_name`, `key_name`, `status`, `duration_ms`, `args_summary`, `error`, `timestamp`, `(org_id, timestamp)` index | Per-call MCP audit log |
| `Incident` | `title`, `summary`, `report` (markdown), `severity`, `status`, `camera_id`, `created_by`, `resolved_at`, `resolved_by` | AI-generated incident (`open` / `acknowledged` / `resolved` / `dismissed`) |
| `IncidentEvidence` | `incident_id` (FK cascade), `kind` (`snapshot` / `clip` / `observation`), `text`, `camera_id`, `data` (LargeBinary), `data_mime` | Snapshot JPEG, clip (MPEG-TS bytes), or text observation — evidence travels inline with the incident |
| `MotionEvent` | `camera_id`, `node_id`, `score` (0–100), `segment_seq`, `timestamp`, `(org_id, timestamp)` index | Motion detected by CameraNode scene-change analysis |
| `Notification` | `kind`, `audience` (`all` / `admin`), `title`, `body`, `severity`, `link`, `camera_id`, `node_id`, `meta_json` | Unified inbox entry (15 kinds: motion, motion_digest, camera_offline / camera_online, node_offline / node_online, incident_created, mcp_key_created / mcp_key_revoked, cameranode_disk_low, member_added / member_role_changed / member_removed / member_promotion_requested, welcome) |
| `UserNotificationState` | `clerk_user_id` + `org_id` (unique), `last_viewed_at` | Per-user read cursor for the inbox |
| `OrgMonthlyUsage` | `org_id` + `year_month` (unique), `viewer_seconds` | One row per org per calendar month; aggregates live-playback viewer-seconds for viewer-hour cap enforcement |
| `EmailOutbox` | `recipient_email`, `subject`, `body_text`, `body_html`, `kind`, `notification_id` (soft FK), `status` (`pending`/`sending`/`sent`/`failed`/`suppressed`), `attempts`, `resend_message_id`, `(status, created_at)` index | Pending email send queue. Drained by `email_worker_loop` every 5s. Survives process restart. |
| `EmailLog` | `recipient_email`, `kind`, `status`, `resend_message_id`, `error`, `timestamp`, `(org_id, timestamp)` index | Per-org email send/delivery audit. Per-tier retention via `run_log_cleanup`. |
| `EmailSuppression` | `address` (unique, lowercased), `reason`, `source`, `created_at` | Local mirror of Resend's suppression list. Worker checks before every send. **Cross-tenant by design** (no `org_id`) — a hard-bounced address stops getting mail across every org. |
| `ProcessedWebhook` | `svix_msg_id` (unique), `event_type`, `created_at` | Webhook dedup table for both Clerk and Resend (idempotency under Svix retry). **Cross-tenant by design** (no `org_id`). |
| `SentinelConfig` | `org_id` (unique), `enabled`, `motion_enabled`, `incident_opened_enabled`, `motion_cooldown_min`, `schedule_mode` (`always` / `scheduled` / `off`), `schedule_start`/`end` (HH:MM), `active_days` (JSON), `camera_scope` (JSON) | Per-org Sentinel AI configuration. Lazily upserted on first GET via `_ensure_config_row()`. |
| `SentinelRun` | `id` (UUID hex), `org_id`, `triggered_at`, `trigger_type` (`motion` / `incident_opened` / `manual` / `scheduled`), `camera_id`, `outcome` (`pending` / `running` / `incident` / `no_action` / `error`), `severity`, `incident_id` (no FK), `started_at`, `completed_at`, `tool_trace` | One row per Sentinel agent invocation. Drives the Sentinel AI dashboard's run history + the monthly-cap counter. |

Validation constants (`models.rs`):
- `INCIDENT_STATUSES` = `("open", "acknowledged", "resolved", "dismissed")`
- `INCIDENT_SEVERITIES` = `("low", "medium", "high", "critical")`

## API Routes

**Every route is registered in `app.rs`, in one table — read that first.**
There are no router prefixes in the Rust tier: axum takes full paths, so the
prefix a FastAPI router carried is written out at each route. The grouping
below is kept because it is how the endpoints are organised in the code, and
because a reader comparing the two stacks needs the correspondence.

| Was | Is now | Paths |
|------|--------|------|
| `cameras.py` | `api/cameras.rs`, `api/recording.rs`, `api/settings.rs`, `api/timezone.rs`, `api/groups.rs`, `api/audit.rs`, `api/gdpr.rs` | `/api/cameras…`, `/api/camera-groups`, `/api/settings…`, `/api/audit-logs` |
| `nodes.py` | `api/nodes.rs`, `api/node_register.rs`, `api/node_writes.rs` | `/api/nodes…` |
| `hls.py` | `api/hls.rs` (cache in `hls.rs`) | `/api/cameras/{camera_id}/…` |
| `audit.py` | `api/stream_logs.rs` | `/api/audit/stream-logs…` |
| `incidents.py` | `api/incidents.rs` | `/api/incidents…` |
| `mcp_keys.py` | `api/keys.rs` | `/api/mcp/keys`, `/api/mcp/tools` |
| `mcp_activity.py` | `api/mcp_activity.rs` | `/api/mcp/activity/…` |
| `integration.py` | `api/integration.rs` | `/api/integration/…` |
| `motion.py` | `api/motion.rs` | `/api/motion/…` |
| `notifications.py` | `api/notifications.rs` | `/api/notifications/…` |
| `sentinel.py` | `api/sentinel.rs`, `api/sentinel_config.rs` | `/api/sentinel/…` |
| `install.py` | `api/install.rs` | `/install.sh`, `/mcp-setup.*`, `/downloads/…` |
| `ws.py` | `api/ws.rs` (protocol in `ws.rs`) | `/ws/node` |
| `webhooks.py` | `api/clerk_webhook.rs`, `api/resend_webhook.rs` | `/api/webhooks/…` |
| (local auth) | `api/local_auth.rs` | `/api/auth/local/…` |
| (health) | `api/health.rs` | `/api/health`, `/api/health/detailed` |
| (RFC 9116) | `api/well_known.rs` | `/.well-known/security.txt`, `/security.txt` |
| (docs) | `api/docs.rs` | `/api-docs`, `/api-redoc`, `/api/openapi.json` |

### All endpoints

**api/cameras.rs + siblings** (prefix `/api`):
- `GET /cameras` — list cameras (view)
- `GET /cameras/{camera_id}` — get camera (view)
- `POST /cameras/{camera_id}/snapshot` — ask node to capture & store a snapshot locally (view, 30/min)
- `POST /cameras/{camera_id}/recording` — manual start/stop recording on the camera; thin wrapper that flips `continuous_24_7` (admin, 30/min)
- `PATCH /cameras/{camera_id}/recording-settings` — partial-update per-camera recording policy (`continuous_24_7`, `scheduled_recording`, `scheduled_start`, `scheduled_end`).  Per-camera since v0.1.43 — replaced the retired org-level `/settings/recording` endpoint pair (admin, 30/min)
- `POST /cameras/{camera_id}/codec` — CameraNode reports codec after first segment (node API key, 30/min)
- `GET /camera-groups` — list groups (view)
- `POST /camera-groups` — create group (admin, 20/min)
- `DELETE /camera-groups/{group_id}` — delete group; member cameras unassigned (admin, 60/min)
- `PUT /cameras/{camera_id}/group` — assign / unassign a camera's group (admin, 60/min)
- `GET /settings` — all org-level settings (view)
- `POST /settings/timezone` — set the org's IANA timezone for scheduled-recording-window interpretation (admin, 30/min)
- `GET /settings/notifications` — read inbox + email toggle prefs (view)
- `POST /settings/notifications` — update inbox + email toggle prefs (admin, 30/min)
- `GET /settings/motion-ingestion` — read motion-event ingestion toggle (view)
- `POST /settings/motion-ingestion` — toggle motion-event ingestion org-wide (admin, 30/min)
- `GET /audit-logs` — audit logs (admin, 120/min)
- `POST /settings/danger/wipe-logs` — selectively delete stream + MCP activity logs while keeping the org running (admin + **Pro/Pro Plus**, 5/hour).  Operator-convenience feature, *not* a right-to-erasure obligation.
- `POST /settings/danger/full-reset` — GDPR Article 17 right-to-erasure: wipe all nodes/cameras/recordings/snapshots/incidents/logs/settings for the org (admin, **every plan**, 3/hour).  Routes through the shared `app.core.gdpr.delete_org_data` helper so this end-state matches what `organization.deleted` Clerk webhook produces.

**api/nodes.rs** (prefix `/api/nodes`):
- `POST /validate` — validate node_id + API key pair, used by CameraNode setup wizard (10/min)
- `POST /register` — CameraNode registration (API key, 10/min)
- `POST /heartbeat` — CameraNode heartbeat (API key, 60/min)
- `GET /` — list nodes (admin)
- `GET /plan` — current plan, node usage, and limits (view)
- `POST /` — create node (admin, requires active billing + plan capacity, 20/hour)
- `GET /ws-status` — which nodes are WebSocket-connected (admin)
- `POST /self/decommission` — node-initiated factory reset (API key, 10/hour); called from CameraNode's `/wipe confirm` before the local wipe runs so a freshly-wiped box doesn't linger as a stale offline node
- `GET /{node_id}` — get node (admin)
- `DELETE /{node_id}` — delete node (admin; cascades cameras + flushes segment caches, 20/hour)
- `POST /{node_id}/rotate-key` — rotate API key (admin, 5/min)

**api/hls.rs** (prefix `/api/cameras/{camera_id}`):
- `GET /stream.m3u8` — HLS playlist served from cache (JWT)
- `GET /segment/{filename}` — serve cached `.ts` segment from memory (JWT)
- `POST /push-segment?filename=…` — CameraNode pushes `.ts` segment into cache (API key, 1200/min)
- `POST /playlist` — update playlist (API key, 600/min)
- `POST /motion` — motion event delivery (API key, 120/min). HTTP-only post v0.1.61; the pre-v0.1.61 WebSocket-forwarding branch had no producer and was removed.

**api/stream_logs.rs** (prefix `/api`):
- `GET /audit/stream-logs` — stream access logs (admin)
- `GET /audit/stream-logs/stats` — aggregates by camera/user/day (admin)

**api/incidents.rs** (prefix `/api/incidents`):
- `GET /` — list (admin; filters: `status`, `severity`, `camera_id`, `limit`, `offset`)
- `GET /counts` — aggregate counts (admin)
- `GET /{incident_id}` — detail with evidence list (admin)
- `PATCH /{incident_id}` — update status / severity / summary / report (admin)
- `DELETE /{incident_id}` — delete incident + cascade evidence (admin)
- `GET /{incident_id}/evidence/{evidence_id}` — stream snapshot or clip blob (admin)
- `GET /{incident_id}/evidence/{evidence_id}/playlist.m3u8` — synthetic single-segment HLS playlist for in-dashboard clip playback (admin)

**api/keys.rs** (prefix `/api/mcp`):
- `POST /keys` — generate key; JSON body `{name, scope_mode, scope_tools?}` (snake_case; any other field is a 422 rather than ignored, because an ignored `scope_mode` is a full-access key); returns plaintext `osc_...` once (admin + active billing)
- `GET /tools` — live tool catalog with read/write kind (admin)
- `GET /keys` — list MCP keys for the org (admin)
- `DELETE /keys/{key_id}` — revoke (admin)

**api/mcp_activity.rs** (prefix `/api/mcp/activity`):
- `GET /stream` — SSE stream of live MCP tool calls (admin)
- `GET /recent` — recent tool calls (admin)
- `GET /sessions` — session summaries (admin)
- `GET /stats` — aggregated stats by tool / key / time (admin)
- `GET /logs` — filterable MCP tool call log (admin)
- `GET /logs/stats` — summary counts for logs (admin)

**api/integration.rs** (prefix `/api/integration`) — Home Assistant; key mgmt is admin (Clerk JWT), data plane is `osi_` integration-key auth (all tiers):
- `POST /keys` — mint an `osi_` integration key; returns plaintext once (admin)
- `GET /keys` — list integration keys (admin)
- `DELETE /keys/{key_id}` — revoke (admin)
- `GET /cameras` — all cameras across all nodes with LAN-direct `local_url`, snapshot URL, recording state (integration key)
- `GET /cameras/{id}/snapshot` — live JPEG via the node round-trip (integration key)
- `POST /cameras/{id}/recording` — toggle `continuous_24_7`; body `{recording: bool}` (integration key)
- `GET /status` — org rollup: camera/node online counts, per-node disk + version, plan (integration key)
- `GET /motion/stream` — SSE motion feed for HA `binary_sensor`s; separate subscriber pool from the dashboard's `/api/motion/events/stream` (integration key)

**api/motion.rs** (prefix `/api/motion`):
- `GET /events` — list motion events; filters: `camera_id`, `hours`, `limit`, `offset` (view)
- `GET /events/stats` — per-camera aggregates (view)
- `GET /events/stream` — SSE motion feed for dashboard notifications (view)

**api/notifications.rs** (prefix `/api/notifications`):
- `GET /` — paginated inbox, newest first; applies audience filter (view)
- `GET /unread-count` — cheap count for the bell badge (capped at 99) (view)
- `POST /mark-viewed` — bump `last_viewed_at` to now (view)
- `POST /clear-all` — hide everything up to now from the caller's own inbox (a per-user cursor; deletes nothing, other members unaffected) (view)
- `POST /request-admin-promotion` — member-initiated admin-access request; fires the `member_promotion_requested` notification kind to org admins (view)
- `GET /stream` — SSE stream for the bell; audience filter applied server-side (view)
- `GET /email/preferences` — read the org's per-kind email toggles (view)
- `POST /email/preferences` — update the org's per-kind email toggles (admin)
- `GET /email/unsubscribe` — one-click unsubscribe link target.  Validates a signed JWT in the query string, flips the matching email toggle off, returns a confirmation HTML page (no auth required — auth comes from the signed token).

**api/install.rs** (no prefix, no auth):
- `GET /install.sh` — CameraNode installer for Linux/macOS. Windows users install via the MSI from the latest CameraNode GitHub release; the legacy `/install.ps1` route was removed when the MSI shipped.
- `GET /mcp-setup.sh` / `GET /mcp-setup.ps1` — MCP client config helpers (separate from CameraNode install — these configure Claude / Cursor / etc. to talk to this Command Center)

**api/ws.rs** (no prefix):
- `WS /ws/node` — WebSocket channel for CameraNode realtime.  Preferred auth (v0.1.65+): `X-Node-API-Key` + `X-Node-Id` headers on the upgrade request.  Back-compat fallback (pre-v0.1.65 clients): `?api_key=…&node_id=…` query string — still accepted, but the handler logs a deprecation warning on every successful auth so we can sunset the path once the install base has rolled forward.  Headers > query because URLs land in too many log sinks (access logs, Fly platform logs, log-shipper exports) — and in Sentry, which is why `sentry.rs::scrub` strips query strings from every event.
  - Node → Backend: `heartbeat`, `command_result`
  - Backend → Node: `ack`, `command` (`take_snapshot`, `list_snapshots`, `list_recordings`, `wipe_data`), `error`
  - Motion events do **not** flow over WS — they reach Command Center via `POST /api/cameras/{id}/motion`.  The pre-v0.1.61 wire format reserved an `event` / `motion_detected` frame for this path but it was never produced; the unused branch was removed in v0.1.61.

**api/sentinel.rs** (prefix `/api/sentinel`):
- `GET /config` — read the org's Sentinel AI config + plan-aware `monthly_cap` + `plan_gated` flag (view; always 200 — non-eligible orgs get a read-only payload for the upgrade banner)
- `PATCH /config` — partial update of Sentinel config (admin + Pro/Pro Plus; 402 otherwise)
- `GET /runs` — list recent runs + stats (view)
- `GET /runs/{run_id}` — single run with full tool trace (view)
- `POST /runs/manual` — operator "Run now"; skips schedule + scope gates but cap-enforced (admin + Pro/Pro Plus; 429 at cap)
- `GET /runs/pending` — service-to-service.  Agent polls this on wakeup to drain runs across all orgs (FIFO).  Auth: `X-Sentinel-Agent-Key` header.
- `POST /runs/{run_id}/start` — service-to-service.  Agent claims a pending run (`pending → running`).  Idempotent.
- `POST /runs/{run_id}/complete` — service-to-service.  Agent posts terminal outcome (`incident` / `no_action` / `error`) + full tool trace.  Cross-checks `incident_id` belongs to the run's org.  Idempotent on terminal rows.

**api/clerk_webhook.rs + api/resend_webhook.rs** (prefix `/api/webhooks`):
- `POST /clerk` — Clerk subscription + organizationMembership events (Svix-signed with `CLERK_WEBHOOK_SECRET`; refused when it is unset; 120/min)
- `POST /resend` — Resend bounce / complaint / unsubscribe webhooks (Svix signature when `RESEND_WEBHOOK_SECRET` is set); writes to `EmailSuppression` so subsequent sends short-circuit before the API call

**Top-level** (registered in `app.rs`):
- `GET /api/health` — minimal liveness for load balancers: `{"status": "healthy", "version": "2.1.2"}` (no auth)
- `GET /api/health/detailed` — verbose status for status-page polling and on-call diagnostics: `{status, version, uptime_seconds, started_at, time, checks: {database: {status, latency_ms}, hls_cache: {playlists_cached, segment_cameras}, viewer_usage: {pending_writes, status}, sse: {subscriber_orgs, subscriber_total}}}`. Public on purpose — every value is metric-shaped, never an org/camera/user identifier (pinned by a privacy regression test in `api/health.rs`).
- API docs: `/api-docs` (Swagger), `/api-redoc` (ReDoc), OpenAPI at `/api/openapi.json` — FastAPI's own document, harvested and compiled in, held in step with the route table by `tests/differential/openapi_drift.py`. `/docs` is the React `DocsPage`. **Off in production**: unregistered when `FLY_APP_NAME` is set, so all three answer the API 404, as they did under the Python, on everywhere else; `API_DOCS_ENABLED` overrides either way.

## MCP Server

Mounted at `/mcp/` via rmcp's streamable-HTTP transport, stateless and
JSON-framed (`json_response = true`), which is what FastMCP answered when a
client accepted JSON. rmcp's DNS-rebinding guard is **disabled** (`app.rs`): its default allows only a localhost `Host`, which would answer 403 to every real client, and every request here carries a bearer key anyway — `tests/routing.rs` sends a foreign Host to keep it that way. Authenticates with `Authorization: Bearer osc_...` against `McpApiKey.key_hash`. Exposes **23 tools** (16 read + 7 write).

### Scope middleware

The scope gate (`mcp/scope.rs`, applied by `mcp/server.rs`) runs before every `list_tools` and `call_tool` request:

1. Extracts the Bearer token from the request headers
2. SHA-256-hashes the key and looks up the matching `McpApiKey` row
3. Computes the allowed-tool frozenset from `scope_mode` + `scope_tools`
4. Filters `list_tools` responses and returns a tool error on disallowed `call_tool` invocations. A key it does not recognise — none, mistyped, revoked — is refused `list_tools` outright (a JSON-RPC error, not an HTTP 401, which would send clients into OAuth discovery); FastMCP had listed the whole catalog to it

Scope modes:
- `"all"` (default; NULL also treated as "all" for legacy rows) → every tool
- `"readonly"` → intersection with `MCP_READ_TOOLS` (16 tools)
- `"custom"` → intersection of `scope_tools` JSON list with `MCP_ALL_TOOLS` (unknown names silently dropped — can't accidentally enable a new server-side WRITE tool via typo)

### Tool inventory

**Read tools (`MCP_READ_TOOLS`, 16):**

| Tool | Purpose |
|------|---------|
| `list_cameras` | All cameras with status/codec/group |
| `get_camera` | One camera by id |
| `get_stream_url` | Authenticated HLS URL for a camera |
| `view_camera` | Live JPEG from a camera (agent can see it) |
| `watch_camera` | Multi-frame burst (2–10 frames, 1–30s apart) |
| `list_camera_groups` | Camera groups for the org |
| `list_nodes` | CameraNodes + their status |
| `get_node` | One node by id |
| `get_camera_recording_policy` | One camera's recording policy (continuous / scheduled / off) |
| `get_stream_logs` | Stream access audit entries |
| `get_stream_stats` | Aggregated views by camera/user/day |
| `get_system_status` | Org-wide snapshot (cameras on/offline, plan, nodes) |
| `list_incidents` | Previous incidents (filter by status/severity/camera) |
| `get_incident` | Full detail of one incident incl. evidence metadata |
| `get_incident_snapshot` | Fetch a previously attached snapshot JPEG |
| `get_incident_clip` | Metadata about a previously attached clip |

**Write tools (`MCP_WRITE_TOOLS`, 7):**

| Tool | Purpose |
|------|---------|
| `create_incident` | Open a new incident (title, summary, severity) |
| `add_observation` | Append a text observation to an incident |
| `attach_snapshot` | Capture a JPEG and attach it as evidence |
| `attach_clip` | Save the recent live buffer as a video clip (pulls from in-memory HLS cache) |
| `update_incident` | Change status / severity / summary / report body (revisions) |
| `finalize_incident` | Write the markdown report body for the first time |
| `set_camera_recording_policy` | Flip a camera between continuous / scheduled / off (mutually exclusive; HH:MM windows in org timezone) |

## CORS

Configured in `cors.rs`:
```
http://localhost:5173
http://localhost:8000
https://sentinel-command.com
```
Plus `FRONTEND_URL` if set (validated: must have scheme, no trailing slash, no embedded whitespace). All methods and headers allowed; credentials allowed.

## Rate Limiting

`ratelimit.rs` with a tenant-aware key (Redis when `REDIS_URL` is set, else
per-process counters), reproducing slowapi's buckets:
- `POST /api/nodes/validate`, `POST /api/nodes/register` — 10/min
- `POST /api/nodes/heartbeat` — 60/min
- `POST /api/nodes/{id}/rotate-key` — 5/min
- `POST /api/cameras/{id}/codec` — 30/min
- `POST /api/cameras/{id}/push-segment` — 1200/min
- `POST /api/cameras/{id}/playlist` — 600/min
- `POST /api/cameras/{id}/motion` — 120/min
- `POST /api/auth/local/login` — 10/min **per client address only** (`PerMinuteByIp`)

The tenant key reads credentials *unverified* — that is how a node or an
org gets its own bucket. It also means a caller chooses its bucket: a fresh
`X-Node-API-Key`, or a token naming a fresh org, on every request and no
limit is met. Harmless where the credential is checked first and cannot be
guessed; not on the local login, which checks a password a person chose.
That route ignores credentials, and proxy headers count only when
`TRUST_PROXY_HEADERS` says a proxy wrote them.

HLS `GET` paths (`stream.m3u8`, `segment/{file}`) are not per-request rate limited — segment fetches are fast-path with no per-request DB work. They are however metered against the caller's monthly viewer-hour cap (see `Plan Enforcement` → Viewer-hours below): every served segment increments an in-memory counter, and the 429 kicks in when the counter exceeds `max_viewer_hours_per_month * 3600`.


## Webhook Handling

Two endpoints, both Svix-signed (signature verification mandatory in production):

### `POST /api/webhooks/clerk` — Clerk events

- Verifies the Svix signature with `CLERK_WEBHOOK_SECRET`. **Unset, every delivery is refused** (400 "Webhook processing unavailable") — not accepted unsigned, which would let anyone forge `organization.deleted` and wipe an org. Checked against the running service.
- Dedup via `ProcessedWebhook(svix_msg_id)` so retries are idempotent

**Subscription lifecycle:**
- `subscription.{created,updated,active}` — writes `Setting(org_plan)`, updates the Clerk org member limit, and runs `enforce_camera_cap` so a plan change (in either direction) flips the `Camera.disabled_by_plan` flags to match the new cap.
- `subscription.pastDue` / `subscriptionItem.pastDue` — writes `Setting(payment_past_due="true")` and a timestamped `payment_past_due_at`. No camera enforcement at this stage; see the grace-period note below.
- `paymentAttempt.updated` with `status="paid"` — clears both past-due settings and re-runs `enforce_camera_cap`, so cameras suspended during a grace-expired past-due window light back up.
- `subscriptionItem.{canceled,ended}` — reverts to `free_org`, resets member limit, and re-runs `enforce_camera_cap`. Camera rows are preserved (not deleted) so re-subscribe instantly restores streaming.

**Organization membership lifecycle (security audit):**
- `organizationMembership.created` — emits `member_added` notification (audience: admin). Severity is `warning` for promotion-to-admin, `info` for member-tier additions.
- `organizationMembership.updated` — emits `member_role_changed` notification.
- `organizationMembership.deleted` — emits `member_removed` notification.
- All three notifications wrapped in try/except so a notification fault never causes Clerk webhook backpressure.

**Org lifecycle:**
- `organization.deleted` — full org wipe (cameras, nodes, groups, MCP keys, all logs, settings).

### `POST /api/webhooks/resend` — Resend events

- Verifies signature with Svix using `RESEND_WEBHOOK_SECRET`
- Dedup via the same `ProcessedWebhook` table (Svix msg_ids are high-entropy; cross-source collision is astronomical)
- `email.bounced` → insert `EmailSuppression(address, reason='bounce', source='resend_webhook')` so the worker stops sending to that address
- `email.complained` → insert `EmailSuppression(address, reason='complaint', source='resend_webhook')`. User marked our email as spam — protects sender reputation by removing them from the list immediately.

## Plan Enforcement

`plans.rs` owns plan-cap policy. `PLAN_LIMITS` is the source of truth for every per-tier number — camera/node/seat caps (abuse rails), monthly viewer-hour cap (the real tier axis), per-channel SSE concurrency cap, and log retention days. Three tiers: `free_org`, `pro`, `pro_plus`. (An earlier `business` slug was renamed to `pro_plus` during a Clerk-side reorg; the transitional alias was carried briefly and removed once every known org had rolled over — see ADR `docs/adr/0002-viewer-hour-billing.md` for the original tier names.)

Two entry points for plan resolution:

- `resolve_org_plan(db, org_id)` — nominal plan (what Clerk says the org pays for). Fast-path reads `Setting(org_plan)`; falls back to a throttled `clerk.organizations.get_billing_subscription` call for free/missing orgs. Used for the status-bar badge CameraNode shows the operator.
- `effective_plan_for_caps(db, org_id)` — plan to use for *cap enforcement*. Returns `resolve_org_plan` unless the org has been `payment_past_due` for more than `PAYMENT_GRACE_DAYS` (7), in which case it returns `"free_org"`. Used inside `enforce_camera_cap`; keeps the two concerns separate so brief card failures don't punish paying users but long-unpaid accounts don't keep getting Pro service.

### Camera cap

`enforce_camera_cap(db, org_id)` — idempotent. Orders the org's cameras by `created_at ASC`, keeps the oldest N (N = effective plan's `max_cameras`), flags the rest as `disabled_by_plan=True`. Oldest-first is deterministic and preserves long-running cameras with history. On upgrade, flags clear in the same call.

### Viewer-hours (the real tier axis)

`hls.rs` maintains a per-org monthly viewer-second counter. Each successful `GET /segment/{filename}` serve calls `record_viewer_second(org_id)` which increments a thread-safe in-memory dict keyed on `(org_id, "YYYY-MM")`. The `_viewer_usage_flush_loop` background task flushes accumulated deltas to `OrgMonthlyUsage` rows every 60 seconds with one UPSERT per active org, so the hot serve path never touches the database.

Before serving each segment, `get_hls_segment` calls `_warm_cached_viewer_seconds(org_id)` to get the authoritative running total (cached DB value + pending in-memory delta). If that total is ≥ `max_viewer_hours_per_month * 3600`, the route returns HTTP 429 with `Retry-After: 3600` and an upgrade message. The first request for an org in a given process lifetime amortizes a DB read to warm the cache; thereafter it's all in-memory.

The counter is exposed on `GET /api/nodes/plan` as `usage.viewer_hours_used` / `usage.viewer_hours_limit` so the dashboard can render a live gauge.

### SSE caps

Every SSE broadcaster (`MotionBroadcaster`, `NotificationBroadcaster`, `McpActivityTracker`) accepts a per-call `cap` argument. Route handlers look up `get_plan_limits(user.plan)["max_sse_subscribers"]` and pass that; the broadcaster refuses `subscribe()` when the org is at cap and the route turns it into a 429 with the tier-specific cap in the message.

### MCP daily cap

`mcp/scope.rs`'s rate limiter tracks two windows per API key hash: a rolling 60-second window (minute cap) and a rolling 24-hour window (daily cap). `RATE_LIMITS[plan]` supplies both numbers (Pro 30/min × 5000/day, Pro Plus 120/min × 30000/day). The `check()` method returns a `breach` reason string so the caller can generate a "you're spamming" vs "you've been looping for hours" error message.

### Log retention

`loops.rs`'s log-cleanup loop runs daily. It collects distinct `org_id` values from the log tables, resolves each org's plan via `resolve_org_plan`, and deletes each log type older than that org's `log_retention_days`. Free gets 30d, Pro 90d, Pro Plus 365d.

**Triggers** for `enforce_camera_cap`:
1. Webhook: subscription lifecycle events (create/update/cancel/paid).
2. Register (`POST /api/nodes/register`): safety net for any missed webhook. Idempotent so cost is just one indexed query in the steady state.
3. Heartbeat (`POST /api/nodes/heartbeat`): gated on `payment_past_due=="true"`. Drives the time-based grace-expiration transition since no webhook fires for that clock tick.

**Push-segment gate** (`POST /api/cameras/{id}/push-segment`): when `camera.disabled_by_plan` is set, returns **HTTP 402** with a structured `plan_limit_hit` body (plan display name, cap, camera name, upgrade copy). CameraNode treats 402 as non-retryable and surfaces the suspension in its TUI.

**Heartbeat response** also carries `disabled_cameras: list[str]` scoped to the calling node, so CameraNode can skip the upload task entirely for suspended cameras (no 402 flood) and mark those rows `suspended (plan)` in its live dashboard.

## Background Loops

`main.rs` starts the same long-running tasks the Python's lifespan did —
`hls::spawn_loops`, `versions::spawn_refresh_loop`, the email worker and
`loops::spawn_loops`, which decides per auth mode which of the licence
check-in, the data sync and the plan reconcile to schedule:

| Task | Cadence | What it does |
|------|---------|--------------|
| `_log_cleanup_loop` | Every `LOG_CLEANUP_INTERVAL_HOURS` (24h) | Thin scheduler around `run_log_cleanup(db) -> dict` (extracted for direct test coverage). Iterates distinct `org_id` values across all log tables (StreamAccessLog, McpActivityLog, AuditLog, MotionEvent, Notification, EmailLog), resolves each org's plan, and deletes records older than that org's `log_retention_days` (30 / 90 / 365). Also sweeps terminal-state EmailOutbox rows older than 7 days (cross-org; pending/sending NEVER deleted regardless of age). |
| `_offline_sweep_loop` | Every `OFFLINE_SWEEP_INTERVAL_SECONDS` (30s) | Flips nodes/cameras whose `last_seen` is older than 90s from `status='online'` to `'offline'` and emits `Notification` rows + broadcasts SSE events |
| `_viewer_usage_flush_loop` | Every 60s | Flushes pending in-memory viewer-second counters to the `org_monthly_usage` table with one UPSERT per active org. Keeps the hot HLS-serve path O(1) in memory. |
| `_release_cache_refresh_loop` | Every `RELEASE_CACHE_REFRESH_INTERVAL_SECONDS` (600s) | Polls GitHub `/releases/latest` for the CameraNode repo so the heartbeat handler's `update_available` field stays fresh without blocking on GitHub per request. Cold-boot fallback is `LATEST_NODE_VERSION` env var. |
| `_disk_check_loop` | Every `DISK_CHECK_INTERVAL_SECONDS` (300s = 5min) | Polls `/data` disk usage. When ≥95%, fires a single `logger.error()` with structured `extra` fields (Sentry-captured). 6h re-emit debounce per process. **Operator-side only** — does NOT route through customer notifications (multi-tenant violation removed 2026-05-04). |
| `_motion_digest_loop` | Every `MOTION_DIGEST_INTERVAL_SECONDS` (60s) | Drains expired per-camera motion email cooldown anchors (Setting rows keyed `motion_email_cooldown_start:{camera_id}`). For each expired window: counts MotionEvent rows in the window, emits a `motion_digest` notification (which itself enqueues a digest email) if extras > 0, deletes the anchor regardless. Volume cap: 2 emails per cycle per camera. |
| `_sentinel_reaper_loop` | Every `SENTINEL_REAPER_INTERVAL_SECONDS` (300s = 5min) | Sweeps `SentinelRun` rows stuck in `running` for more than `STRANDED_RUN_AGE_MINUTES` (20 min) and marks them `outcome='error'` with a "stranded — agent never finished" note. Catches the rare case where the agent's own wall-clock cleanup wrapper doesn't fire (process crash, network partition); without this the run drawer would show a permanent in-progress spinner. |
| `email_worker_loop` (in `email_worker.rs`, spawned from `main.rs`) | Every `EMAIL_WORKER_INTERVAL_SECONDS` (5s) | Drains EmailOutbox `status='pending'` rows in batches of `EMAIL_WORKER_BATCH_SIZE`. Per row: check suppression list → call `email.send_email()` → mark `sent`/`failed`/`suppressed` → write EmailLog row. Reclaims rows stuck in `sending` for >60s (worker crash). Idempotency-Key on the Resend send protects against duplicate delivery on retry. |

## Key Patterns

**Tenant isolation:** every query filters by `org_id` from the authenticated user/node.

**Error handling:** Two layers. New endpoints return `error.rs::ApiError` for a structured envelope (`{detail: {error, message, ...extras}}`); the frontend's `services/api.js::parseErrorBody` reads `e.message` for toasts and `e.code` for branching. Older endpoints still use bare `HTTPException(detail="...")` and the frontend parser handles both shapes plus the rate-limit handler's top-level `{error, message, ...}` shape, so call sites never see `[object Object]`. Query and body validation failures are normalised into the same 422 envelope by `query.rs`, which reproduces FastAPI's `loc`/`msg`/`ctx` shape down to the order parameters are reported in. The envelope is REST-only — MCP tools at `/mcp/` return a tool error, because the JSON-RPC error shape is fixed by the protocol.

**Database access:** a pooled `db::Pool` on `AppState` — PostgreSQL or SQLite by build, with runtime-checked
queries (`sqlx::query*`) rather than the compile-time macros, so a build needs
no live database.

**In-memory segment cache:** live `.ts` segments live in `hls.rs`'s cache,
keyed camera → filename → (bytes, timestamp). One process owns it. Backend never touches S3 for live video. Recordings and snapshots live on the CameraNode. Incident snapshots + clips are stored inline on `IncidentEvidence.data` (LargeBinary).

**Codec detection:** CameraNode reports codec via `POST /api/cameras/{id}/codec` after the first segment, stored on the Camera and CameraNode rows. It is **not** put in the playlist: `#EXT-X-CODECS` is only valid in a master playlist, so `api/hls.rs` strips any such line from what a node pushes rather than adding one.

**Notification broadcaster:** `notifications.rs`'s broadcaster is a per-process pub/sub — SSE subscribers register per org + admin flag; `emit_camera_transition`, `emit_node_transition`, and motion event handlers write a `Notification` row then broadcast.

**Motion broadcaster:** the motion SSE stream pushes events that arrive on `POST /api/cameras/{id}/motion`.  Motion delivery is HTTP-only since CameraNode v0.1.61; the pre-v0.1.61 WebSocket variant had no producer on the node side and was removed.

**Shared Clerk token:** frontend's `useSharedToken` serialises the Clerk JWT for HLS.js's `xhrSetup` so segment fetches ride on the same auth as API calls.

**First-heartbeat UX:** when the admin creates a node, the dashboard stashes the new `node_id` in `localStorage` and `HeartbeatBanner` starts polling `GET /api/nodes/{node_id}` every few seconds. As soon as `last_seen` is non-null the banner auto-dismisses. Users don't have to refresh — it's a reassurance loop for the 30–60s window where the node is downloading ffmpeg / registering cameras.

**Role-split welcome hero:** `WelcomeHero.jsx` exports two components — `AdminWelcomeHero` shows the "Install a CameraNode → Camera goes live" checklist with CTAs into Settings + the install guide; `MemberWelcomeHero` shows a capability-focused welcome (live monitoring, motion alerts, team workspace) because members can't act on a setup checklist. `DashboardPage` picks the right one based on `is_admin`.

## Setup Scripts

`scripts/mcp-setup.sh` + `mcp-setup.ps1` are served verbatim by `api/install.rs` from `SCRIPTS_DIR` (`/app/scripts` in the image). They:
1. Accept `<api_key> <server_url>` (positional)
2. Detect installed MCP clients (Claude Code, Claude Desktop, Cursor, Windsurf)
3. Prompt the user for which ones to configure
4. Merge an `sentinel` entry into each client's JSON config (creating directories + backing up corrupted files)

**Windows invocation pattern** — `irm … | iex -Args …` **does not work** (`iex` has no `-Args`). Use the scriptblock pattern instead, which is what the dashboard prints:

```powershell
& ([scriptblock]::Create((irm <url>/mcp-setup.ps1))) '<api_key>' '<server_url>'
```

**Bash invocation** — when run via `curl … | bash -s --`, stdin is the piped script, so `read` would hit EOF immediately. The script falls back to `</dev/tty` when stdin isn't a TTY.

## Key Dependencies

- `axum` / `tokio` / `hyper` — the web framework and runtime
- `sqlx` — PostgreSQL, or SQLite under `--features sqlite` (runtime-checked queries, so a build needs no live database), with the migrations embedded at compile time
- `rmcp` — the Model Context Protocol, both ends: the server Command
  Center mounts (replaced `fastmcp`) and the client the agent connects
  with (replaced the Python SDK). Pinned to `3.5` because `"0.9"`
  resolves to a much older crate of the same name. Its client does not
  follow the 307 from `/mcp` to `/mcp/`, which is why the agent's URL
  always ends in the slash.
- `rig-core` (+ `rig-reqwest` for the transport it wraps) — the agent's
  model calls, on three wires: Ollama, Anthropic, OpenAI Chat
  Completions. Replaced LiteLLM. Pre-1.0; see rule 1 above.
- `serde` / `serde_json` — with the `preserve_order` feature, which is
  load-bearing: key order is on the wire and Python's dicts are ordered.
  Note `Map::remove` is a SWAP-remove under it; use `shift_remove`.
- `jsonwebtoken` — Clerk JWT verification (V1 and V2 claim shapes) and the
  signed unsubscribe tokens. No Clerk SDK: auth is local RS256 + JWKS.
- `argon2` — the local-admin password path. Parameters are set explicitly
  to python-argon2's, NOT the crate's weaker default; see
  `bin/hash_password.rs`.
- `reqwest` — outbound HTTP, rustls only (no OpenSSL in the image)
- `minijinja` — email templates, with per-template autoescape
- `tower-http` — static files, tracing, the security-header layer
- `libc` — `statvfs` for the disk probe, and `termios` for the password
  tool's echo suppression, which is why no terminal crate is needed

## Development Notes

- Schema is applied on startup by `sqlx::migrate!`, from `migrations/`
  embedded at compile time. The first migration adopts exactly what
  SQLAlchemy's `create_all()` plus the hand-rolled `sync_schema()` had already
  built in production, taken from `pg_dump --schema-only` rather than
  transcribed. See ADR `docs/adr/0001-sync-schema-vs-alembic.md` for the
  history it replaces.
- Backend serves the React build as static files in production (`spa.rs`)
- Frontend uses HLS.js for video playback with a Clerk JWT injected via `xhrSetup`
- `VITE_LOCAL_HLS=true` bypasses the backend and streams directly from CameraNode on localhost:8080 (for local dev only)
- Tests live beside the code in `backend-rs/src/**` plus `backend-rs/tests/`,
  and run with `cargo test` — no database needed; the DB-gated ones skip
  themselves unless `TEST_DATABASE_URL` is set. The agent's tests are in
  `src/agent/**` and `tests/agent_contract.rs`. How the port was verified, and what replaces the
  differential harnesses now that the Python is gone, is in
  `backend-rs/tests/differential/README.md` ("After the cut").
