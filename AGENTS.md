# AGENTS.md

Sentinel Command Center — cloud dashboard for managing and viewing security cameras under the **Sentinel by SourceBox** product brand. FastAPI backend + React 19 frontend with Clerk authentication. Live video is streamed through an in-memory segment cache — **no Tigris, no S3, no presigned URLs in the live path**.

> **Brand-history note for grep-discoverability:** the product has carried three names — `OpenSentry` (early), `SourceBox Sentry` (mid), and `Sentinel by SourceBox` (current, from May 2026 onward). The `Sentinel AI` name is reserved specifically for the AI-agent feature. Both GitHub repos were renamed in May 2026: Command Center `OpenSentry-Command` → `Sentinel-Command`, and CloudNode `opensentry-cloud-node` → `Sentinel-CameraNode` (note the deliberate "CameraNode" — the repo name now describes the artifact more literally, while the binary, install paths, and product UI keep saying "CloudNode"). GitHub auto-redirects the old URLs, so any hardcoded reference in a release artifact / cached doc / external bookmark continues to resolve. Identifiers preserved verbatim across the entire rebrand (do **not** rename these without a migration plan): the binary name `sourcebox-sentry-cloudnode`, the env-var prefix `SOURCEBOX_SENTRY_*`, the Windows install path `C:\ProgramData\SourceBoxSentry\`, the AES key-derivation domain string `opensentry-cloudnode-machine-id-v2` (see CloudNode `database.rs::KEY_DOMAIN_V2`), and the production hostname `opensentry-command.fly.dev` (tied to the Fly app, decoupled from the repo rename).

## Build & Run

**Prerequisites:** Python ≥ 3.12 (enforced by `backend/pyproject.toml`), Node 18+, `uv` for Python dependency management.

```bash
# Backend
cd backend
uv sync
uv run python start.py              # http://localhost:8000

# Tests
cd backend
uv run pytest

# Frontend
cd frontend
npm install
npm run dev                          # http://localhost:5173
npm run build                        # Production build → backend/static/
```

## Configuration

Backend config is loaded from environment variables (see `backend/.env.example`).

**Required:**
- `CLERK_SECRET_KEY` / `CLERK_PUBLISHABLE_KEY` — Clerk auth

**Optional:**
- `CLERK_WEBHOOK_SECRET` — Svix signature for Clerk subscription + organizationMembership webhooks
- `DATABASE_URL` — defaults to `sqlite:///./opensentry.db`. Production uses `sqlite:////data/opensentry.db` on a Fly volume — single-machine deploy, NullPool, WAL, busy_timeout=5000 (see `app/core/database.py`).
- `FRONTEND_URL` — extra CORS origin (must have scheme, no trailing slash)
- `REDIS_URL` — slowapi rate-limiter shared storage. Without it, per-process in-memory counters (single-VM safe; multi-VM round-robins around the limit). Currently in production via Upstash on Fly.
- `SEGMENT_CACHE_MAX_PER_CAMERA` — segments cached in memory per camera (default **60**, ~60s — CloudNode emits 1-second segments)
- `SEGMENT_CACHE_MAX_TOTAL_BYTES` — global byte ceiling across all camera caches (default 2 GiB). When exceeded, `hls.py` evicts oldest segments across ALL cameras until back under cap.
- `SEGMENT_PUSH_MAX_BYTES` — max bytes per pushed segment (default 2 MB)
- `PLAYLIST_PUSH_MAX_BYTES` — max bytes per pushed playlist (default 64 KB)
- `CLEANUP_INTERVAL` — run cache eviction every N playlist updates (default 20)
- `INACTIVE_CAMERA_CLEANUP_HOURS` — free caches for cameras offline this long (default 24)
- `LOG_RETENTION_DAYS` — stream + MCP + audit + motion + notification + email log retention (default 90; per-tier override via plan slug — Free 30 / Pro 90 / Pro Plus 365)
- `OFFLINE_SWEEP_INTERVAL_SECONDS` — how often to mark stale rows offline (default 30)
- `SENTRY_DSN` — error tracking. In production this is managed by the Fly Sentry extension (`fly ext sentry create -a opensentry-command`) which provisions a sponsored Team plan and auto-injects the secret; you rarely set this by hand. `app/core/sentry.py::init_sentry()` is a no-op when the DSN is absent, so local dev needs no extra config. Dashboard: `fly ext sentry dashboard -a opensentry-command`.

**Email (Resend, optional):**
- `EMAIL_ENABLED` — global kill-switch (default `false`). Code can ship with it off; flip to `true` once DNS propagates and a smoke test passes. Worker still runs when off; transport short-circuits with a logged "would have sent" line.
- `RESEND_API_KEY` — Resend transactional API key (`re_…`)
- `RESEND_WEBHOOK_SECRET` — Svix signing secret for the `/api/webhooks/resend` bounce/complaint handler
- `EMAIL_FROM_ADDRESS` — default `notifications@sourceboxsentry.com` (must be on a verified Resend domain)
- `EMAIL_FROM_NAME` — default `Sentinel by SourceBox`
- `EMAIL_WORKER_INTERVAL_SECONDS` — outbox-drain tick interval (default 5)
- `EMAIL_WORKER_BATCH_SIZE` — max rows drained per tick (default 20)
- `EMAIL_MAX_ATTEMPTS` — retries before a row is permanently failed (default 3)

Frontend config: `VITE_CLERK_PUBLISHABLE_KEY`, `VITE_API_URL`, `VITE_LOCAL_HLS`.

## Project Structure

```
backend/
├── app/
│   ├── main.py                   # FastAPI app, CORS, SPA middleware, rate limiting,
│   │                             # lifespan startup (8 background loops: log-cleanup,
│   │                             # offline-sweep, viewer-usage flush, release-cache
│   │                             # refresh, email-worker, disk-check, motion-digest,
│   │                             # sentinel-reaper), MCP mount, disk-check + motion-
│   │                             # digest loop bodies
│   ├── templates/emails/         # 46 Jinja2 email templates — _layout.html.j2 +
│   │                             # 15 kinds (camera offline/online, node offline/
│   │                             # online, incident_created, mcp_key_created/revoked,
│   │                             # cloudnode_disk_low, member_added/role_changed/
│   │                             # removed/promotion_requested, motion, motion_digest,
│   │                             # welcome) × 3 files (subject.txt + body.txt + body.html)
│   ├── api/
│   │   ├── cameras.py            # Cameras, groups, settings, audit logs, danger zone
│   │   ├── nodes.py              # CloudNode register/heartbeat/CRUD, plan info,
│   │   │                         # cloudnode_disk_low alert helper (per-node debounce
│   │   │                         # via Setting key colon-suffix pattern)
│   │   ├── hls.py                # HLS playlist + segment memory cache + push-segment
│   │   │                         # + HTTP motion fallback + global byte-cap eviction
│   │   ├── audit.py              # Stream access logs + stats
│   │   ├── incidents.py          # AI-generated incident reports (CRUD + evidence blobs)
│   │   ├── mcp_keys.py           # MCP API key management + tool catalog + audit
│   │   │                         # notifications (mcp_key_created/revoked)
│   │   ├── mcp_activity.py       # MCP activity logs, stats, SSE stream
│   │   ├── motion.py             # Motion event queries, stats, SSE stream
│   │   ├── notifications.py      # Notification inbox, unread count, SSE, broadcaster,
│   │   │                         # email kind map, email cooldown gate (motion v1.1),
│   │   │                         # email prefs endpoints, signed unsubscribe
│   │   ├── install.py            # CloudNode + MCP setup script endpoints
│   │   ├── ws.py                 # CloudNode WebSocket channel
│   │   └── webhooks.py           # Clerk subscription + organizationMembership +
│   │                             # Resend bounce/complaint webhook handlers
│   ├── mcp/
│   │   └── server.py             # FastMCP server + 23 tools + ScopeMiddleware
│   ├── core/
│   │   ├── audit.py              # Audit-log writer (error-swallowing pattern)
│   │   ├── auth.py               # Clerk JWT validation (V1 + V2 permissions), dependencies
│   │   ├── config.py             # Environment loading (Config class)
│   │   ├── clerk.py              # Clerk SDK init
│   │   ├── database.py           # SQLAlchemy engine + session factory + Base
│   │   ├── email.py              # Resend SDK touchpoint (single send entry; honors
│   │   │                         # EMAIL_ENABLED kill-switch + suppression list)
│   │   ├── email_templates.py    # Jinja2 renderer (per-template autoescape selection;
│   │   │                         # .html.j2 escapes, .txt.j2 doesn't)
│   │   ├── email_unsubscribe.py  # Signed JWT for one-click footer unsubscribe links
│   │   ├── email_worker.py       # EmailOutbox drain loop + retry + reclaim-stuck-sending
│   │   ├── errors.py             # ApiError class — structured 4xx/5xx envelope
│   │   ├── limiter.py            # slowapi Limiter instance (tenant-aware key)
│   │   ├── migrations.py         # sync_schema (column adder), drop_orphan_tables,
│   │   │                         # sanitize_existing_codecs — stand-in for Alembic
│   │   ├── plans.py              # PLAN_LIMITS, effective_plan_for_caps, grace period
│   │   ├── recipients.py         # Clerk org member lookup + 5-min TTL cache + audience
│   │   │                         # filter (admin / all) + suppression-list exclusion
│   │   ├── release_cache.py      # GitHub /releases/latest cache for CloudNode
│   │   │                         # update_available signal
│   │   └── sentry.py             # Sentry SDK init (no-op when SENTRY_DSN unset)
│   ├── models/models.py          # 18 ORM models (see Data Models below)
│   └── schemas/schemas.py        # Pydantic request/response schemas incl. McpKeyCreate
├── scripts/
│   ├── install.sh                # CloudNode installer for Linux/macOS (served by install.py).
│   │                               # Windows installs via the MSI from the latest CloudNode
│   │                               # GitHub release, not a script — see CloudNodeSetup docs.
│   └── mcp-setup.sh / .ps1       # MCP client config helpers (Claude Code / Desktop / Cursor / Windsurf)
├── tests/                        # pytest — security, MCP scoping, motion, notifications, offline sweep,
│                                 # billing/grace, ApiError envelope, drop_orphan_tables migration
├── start.py                      # Uvicorn entrypoint (0.0.0.0:8000, reload=True)
├── pyproject.toml                # Includes [tool.uv] constraint-dependencies pinning
│                                 # python-multipart and authlib past Dependabot moderates
└── .env.example

docs/                             # Supplementary docs that don't belong in README/AGENTS
├── README.md                     # Index of runbooks + ADRs
└── adr/
    ├── 0001-sync-schema-vs-alembic.md
    └── 0002-viewer-hour-billing.md

frontend/
├── tests/                        # vitest + @testing-library/react + happy-dom
│   ├── setup.js                  # @testing-library/jest-dom matchers + cleanup
│   ├── sanity.test.js            # runner + DOM + matcher wiring smoke
│   ├── services/api.test.js      # fetchWithAuth shape contract (4 wire shapes)
│   ├── components/               # DocsDiagrams, UpgradeModal, EmptyState
│   └── pages/DocsPage.test.jsx   # split structural smoke (every section id renders)
└── src/
    ├── pages/
    │   ├── LandingPage.jsx           # Public landing page
    │   ├── DashboardPage.jsx         # Camera grid with status cards + controls
    │   ├── SettingsPage.jsx          # Nodes, groups, recording, danger zone
    │   ├── McpPage.jsx               # MCP keys (scope picker) + activity (live SSE)
    │   ├── IncidentsPage.jsx         # AI- and human-filed incident reports + create flow
    │   ├── AdminPage.jsx             # Stream logs, MCP activity, audit trail
    │   ├── PricingPage.jsx           # Public pricing tiers
    │   ├── SecurityPage.jsx          # Public privacy + security claims page (/security)
    │   ├── SentinelPage.jsx          # Sentinel agent dashboard — config (triggers,
    │   │                              #   schedule, cooldown, scope), run history,
    │   │                              #   manual "Run now"
    │   ├── LegalPage.jsx             # /legal/:page — Terms, Privacy, etc.
    │   ├── DocsPage.jsx              # /docs — slim composition shell that renders 19 sections
    │   ├── docs/                     # one file per <section> on /docs (extracted from the
    │   │                             # 1,747-line monolith); shared state lives in
    │   │                             # docs/context.jsx (DocsProvider + OsTabs + useDocs)
    │   ├── SignInPage.jsx / SignUpPage.jsx
    │   └── TestHlsPage.jsx           # Admin-only HLS debug view
    ├── components/
    │   ├── HlsPlayer.jsx             # HLS.js player with Clerk JWT xhrSetup
    │   ├── CameraCard.jsx            # Live thumbnail + status + actions
    │   ├── IncidentReportModal.jsx   # Markdown + evidence viewer
    │   ├── NotificationBell.jsx     # Unread badge + inbox popover (SSE-fed)
    │   ├── AddNodeModal.jsx          # Node creation flow (shows one-time API key)
    │   ├── KeyRotationModal.jsx     # Rotate node API key
    │   ├── UpgradeModal.jsx          # Paywall prompt (plan gating)
    │   ├── HeartbeatBanner.jsx       # "Waiting for first heartbeat" banner shown
    │   │                             # after node creation; polls /api/nodes/{id}
    │   │                             # until it sees a last_seen, persists its
    │   │                             # dismissed state in localStorage
    │   ├── WelcomeHero.jsx           # Dashboard empty-state hero — exports
    │   │                             # AdminWelcomeHero (3-step "set up your first
    │   │                             # camera" checklist) and MemberWelcomeHero
    │   │                             # (capability-focused welcome for non-admins)
    │   ├── Layout.jsx / PublicLayout.jsx
    │   ├── LandingNav.jsx / LandingFooter.jsx
    │   ├── ToastContainer.jsx / LoadingSpinner.jsx
    │   ├── DocsDiagrams.jsx         # 8 inline-SVG diagrams embedded on /docs
    │   │                             # (System Architecture / HLS Pipeline / Motion FSM /
    │   │                             # Config Precedence / Incident Lifecycle / MCP Workflow /
    │   │                             # Security Model rings / Dashboard IA tree)
    │   └── EmptyState.jsx
    ├── hooks/
    │   ├── useNotifications.jsx      # SSE inbox + unread count
    │   ├── useMotionAlerts.jsx       # Motion SSE + toast fan-out
    │   ├── usePlanInfo.jsx           # Plan info + node quotas
    │   ├── useSharedToken.jsx        # Shared Clerk token provider (HLS + fetch)
    │   └── useToasts.jsx
    └── services/api.js               # Typed client for every backend endpoint
```

## Architecture

### Request flow

```
Browser ──Clerk JWT──→ FastAPI ──SQL──→ SQLite / PostgreSQL
                          ↕
CloudNode ──X-Node-API-Key──→ FastAPI ──RAM──→ in-memory segment cache
          ──WebSocket──────→                     + per-org motion/notification broadcasters
MCP Client ──Bearer osc_…──→ FastMCP → ScopeMiddleware → tools
```

### Video streaming pipeline

1. CloudNode generates HLS segments via FFmpeg (1-second `.ts` files by default; see `streaming.hls.segment_duration` in CloudNode's `config.yaml`)
2. CloudNode calls `POST /api/cameras/{id}/push-segment?filename=segment_NNNNN.ts` with the raw `.ts` body and `X-Node-API-Key` header
3. Backend stores the bytes in `_segment_cache[camera_id][filename]`, evicting the oldest once `SEGMENT_CACHE_MAX_PER_CAMERA` is exceeded
4. CloudNode calls `POST /api/cameras/{id}/playlist` with the rolling `stream.m3u8` text
5. Backend rewrites playlist segment filenames to relative `segment/<file>` proxy URLs and caches the result in `_playlist_cache`
6. Browser calls `GET /api/cameras/{id}/stream.m3u8` with JWT → served instantly from `_playlist_cache`
7. Browser fetches each segment via `GET /api/cameras/{id}/segment/{filename}` → served from `_segment_cache` in memory
8. Cache eviction sweeps every `CLEANUP_INTERVAL` playlist updates; the daily cleanup loop flushes caches for cameras offline >`INACTIVE_CAMERA_CLEANUP_HOURS`

### SPA serving

`main.py` SPA middleware:
- `/api/*`, `/ws/*`, `/install.*`, `/mcp-setup.*` → FastAPI handlers
- `POST /mcp` → FastMCP ASGI app (streamable HTTP)
- `GET /mcp` → React `McpPage` (dashboard route)
- `/assets/*` → static files from React build
- Everything else → `index.html` (SPA client-side routing)

`GET /docs` is owned by the React `DocsPage`; FastAPI's auto docs live at `/api-docs` (Swagger) and `/api-redoc` (ReDoc); the OpenAPI schema is at `/api/openapi.json`.

## Authentication

### Clerk JWT (browser users)

`get_current_user()` dependency in `core/auth.py`:
1. Extracts `Authorization: Bearer <token>` header
2. Authenticates with Clerk SDK
3. Extracts `sub` (user_id), `org_id`, and permissions from JWT claims (V1 or V2 format)
4. Returns an `AuthUser` object with `is_admin`, `permissions`, etc.

**V2 permission decoding** (`decode_v2_permissions()`):
- `o` claim contains org object with `fpm` (feature permission map) and `per` (permissions)
- `fea` claim contains feature list (e.g. `o:admin,o:cameras`)
- Decoded to `org:{feature}:{permission}` format

**Dependencies:**
- `require_view()` → any authenticated org member (no extra permission check)
- `require_admin()` → Clerk role `org:admin` / `admin`, or `org:cameras:manage_cameras` permission

### API key (CloudNode)

CloudNode endpoints validate `X-Node-API-Key`:
1. SHA-256 hash the provided key
2. Match against `api_key_hash` on `CameraNode`
3. Derive `org_id` from the matched node row

### MCP API key

MCP endpoint (`POST /mcp`) validates `Authorization: Bearer osc_<hex>`:
1. SHA-256 hash the raw key
2. Match against `McpApiKey.key_hash` with `revoked=False`
3. `ScopeMiddleware` (see below) filters tool discovery + invocation per-key

## Data Models

All 20 models in `backend/app/models/models.py`. Every model has `org_id` for tenant isolation EXCEPT `ProcessedWebhook` (global webhook dedup, keyed on Svix msg_id) and `EmailSuppression` (operator-global bounce / complaint list, intentionally cross-tenant so a hard-bounced address stops being mailed for every org).

| Model | Key Fields | Purpose |
|-------|------------|---------|
| `Camera` | `camera_id`, `node_id` (FK), `name`, `status`, `video_codec`, `audio_codec`, `group_id`, `last_seen` | Camera registered by a CloudNode; `effective_status` flips to offline after a 90s heartbeat gap |
| `CameraNode` | `node_id`, `api_key_hash`, `hostname`, `status`, `video_codec`, `audio_codec`, `last_seen`, `key_rotated_at`, `storage_*` | Physical CloudNode device + storage stats from heartbeat (drives `cloudnode_disk_low` alert) |
| `CameraGroup` | `name`, `color`, `icon` | User-defined camera grouping |
| `Setting` | `key`, `value` | Per-org key/value store. Used for plan slug, recording config, email toggles, motion email cooldown anchors (`motion_email_cooldown_start:{camera_id}`), CloudNode disk debounce (`cloudnode_disk_low_emit_at:{node_id}`), payment past-due flags. |
| `AuditLog` | `event`, `user_id`, `ip_address`, `details`, `(org_id, timestamp)` index | Security audit trail |
| `StreamAccessLog` | `user_id`, `camera_id`, `ip_address`, `user_agent`, `accessed_at`, `(org_id, accessed_at)` index | Stream playback audit |
| `McpApiKey` | `name`, `key_hash`, `scope_mode`, `scope_tools` (JSON text), `last_used_at`, `revoked` | MCP API keys — **scope_mode**: `all` / `readonly` / `custom` |
| `McpActivityLog` | `tool_name`, `key_name`, `status`, `duration_ms`, `args_summary`, `error`, `timestamp`, `(org_id, timestamp)` index | Per-call MCP audit log |
| `Incident` | `title`, `summary`, `report` (markdown), `severity`, `status`, `camera_id`, `created_by`, `resolved_at`, `resolved_by` | AI-generated incident (`open` / `acknowledged` / `resolved` / `dismissed`) |
| `IncidentEvidence` | `incident_id` (FK cascade), `kind` (`snapshot` / `clip` / `observation`), `text`, `camera_id`, `data` (LargeBinary), `data_mime` | Snapshot JPEG, clip (MPEG-TS bytes), or text observation — evidence travels inline with the incident |
| `MotionEvent` | `camera_id`, `node_id`, `score` (0–100), `segment_seq`, `timestamp`, `(org_id, timestamp)` index | Motion detected by CloudNode scene-change analysis |
| `Notification` | `kind`, `audience` (`all` / `admin`), `title`, `body`, `severity`, `link`, `camera_id`, `node_id`, `meta_json` | Unified inbox entry (15 kinds: motion, motion_digest, camera_offline / camera_online, node_offline / node_online, incident_created, mcp_key_created / mcp_key_revoked, cloudnode_disk_low, member_added / member_role_changed / member_removed / member_promotion_requested, welcome) |
| `UserNotificationState` | `clerk_user_id` + `org_id` (unique), `last_viewed_at` | Per-user read cursor for the inbox |
| `OrgMonthlyUsage` | `org_id` + `year_month` (unique), `viewer_seconds` | One row per org per calendar month; aggregates live-playback viewer-seconds for viewer-hour cap enforcement |
| `EmailOutbox` | `recipient_email`, `subject`, `body_text`, `body_html`, `kind`, `notification_id` (soft FK), `status` (`pending`/`sending`/`sent`/`failed`/`suppressed`), `attempts`, `resend_message_id`, `(status, created_at)` index | Pending email send queue. Drained by `email_worker_loop` every 5s. Survives process restart. |
| `EmailLog` | `recipient_email`, `kind`, `status`, `resend_message_id`, `error`, `timestamp`, `(org_id, timestamp)` index | Per-org email send/delivery audit. Per-tier retention via `run_log_cleanup`. |
| `EmailSuppression` | `address` (unique, lowercased), `reason`, `source`, `created_at` | Local mirror of Resend's suppression list. Worker checks before every send. **Cross-tenant by design** (no `org_id`) — a hard-bounced address stops getting mail across every org. |
| `ProcessedWebhook` | `svix_msg_id` (unique), `event_type`, `created_at` | Webhook dedup table for both Clerk and Resend (idempotency under Svix retry). **Cross-tenant by design** (no `org_id`). |
| `SentinelConfig` | `org_id` (unique), `enabled`, `motion_enabled`, `incident_opened_enabled`, `motion_cooldown_min`, `schedule_mode` (`always` / `scheduled` / `off`), `schedule_start`/`end` (HH:MM), `active_days` (JSON), `camera_scope` (JSON) | Per-org Sentinel AI configuration. Lazily upserted on first GET via `_ensure_config_row()`. |
| `SentinelRun` | `id` (UUID hex), `org_id`, `triggered_at`, `trigger_type` (`motion` / `incident_opened` / `manual` / `scheduled`), `camera_id`, `outcome` (`pending` / `running` / `incident` / `no_action` / `error`), `severity`, `incident_id` (no FK), `started_at`, `completed_at`, `tool_trace` | One row per Sentinel agent invocation. Drives the Sentinel AI dashboard's run history + the monthly-cap counter. |

Validation constants (also in `models.py`):
- `INCIDENT_STATUSES` = `("open", "acknowledged", "resolved", "dismissed")`
- `INCIDENT_SEVERITIES` = `("low", "medium", "high", "critical")`

## API Routes

### Router prefixes

| File | Prefix | Tags |
|------|--------|------|
| `cameras.py` | `/api` | api |
| `nodes.py` | `/api/nodes` | nodes |
| `hls.py` | `/api/cameras/{camera_id}` | streaming |
| `audit.py` | `/api` | audit |
| `incidents.py` | `/api/incidents` | incidents |
| `mcp_keys.py` | `/api/mcp` | mcp |
| `mcp_activity.py` | `/api/mcp/activity` | mcp-activity |
| `motion.py` | `/api/motion` | motion |
| `notifications.py` | `/api/notifications` | notifications |
| `sentinel.py` | `/api/sentinel` | sentinel |
| `install.py` | (none) | installation |
| `ws.py` | (none) | ws |
| `webhooks.py` | `/api/webhooks` | webhooks |

### All endpoints

**cameras.py** (prefix `/api`):
- `GET /cameras` — list cameras (view)
- `GET /cameras/{camera_id}` — get camera (view)
- `POST /cameras/{camera_id}/snapshot` — ask node to capture & store a snapshot locally (view, 30/min)
- `POST /cameras/{camera_id}/recording` — manual start/stop recording on the camera; thin wrapper that flips `continuous_24_7` (admin, 30/min)
- `PATCH /cameras/{camera_id}/recording-settings` — partial-update per-camera recording policy (`continuous_24_7`, `scheduled_recording`, `scheduled_start`, `scheduled_end`).  Per-camera since v0.1.43 — replaced the retired org-level `/settings/recording` endpoint pair (admin, 30/min)
- `POST /cameras/{camera_id}/codec` — CloudNode reports codec after first segment (node API key, 30/min)
- `GET /camera-groups` — list groups (view)
- `POST /camera-groups` — create group (admin, 20/min)
- `DELETE /camera-groups/{group_id}` — delete group; member cameras unassigned (admin, 60/min)
- `PUT /cameras/{camera_id}/group` — assign / unassign a camera's group (admin, 60/min)
- `GET /settings` — all org-level settings (view)
- `POST /settings/timezone` — set the org's IANA timezone for scheduled-recording-window interpretation (admin, 30/min)
- `GET /settings/notifications` — read inbox + email toggle prefs (view)
- `POST /settings/notifications` — update inbox + email toggle prefs (admin, 30/min)
- `GET /settings/motion-ingestion` — read motion-event ingestion toggle (admin)
- `POST /settings/motion-ingestion` — toggle motion-event ingestion org-wide (admin, 30/min)
- `GET /audit-logs` — audit logs (admin, 120/min)
- `POST /settings/danger/wipe-logs` — selectively delete stream + MCP activity logs while keeping the org running (admin + **Pro/Pro Plus**, 5/hour).  Operator-convenience feature, *not* a right-to-erasure obligation.
- `POST /settings/danger/full-reset` — GDPR Article 17 right-to-erasure: wipe all nodes/cameras/recordings/snapshots/incidents/logs/settings for the org (admin, **every plan**, 3/hour).  Routes through the shared `app.core.gdpr.delete_org_data` helper so this end-state matches what `organization.deleted` Clerk webhook produces.

**nodes.py** (prefix `/api/nodes`):
- `POST /validate` — validate node_id + API key pair, used by CloudNode setup wizard (10/min)
- `POST /register` — CloudNode registration (API key, 10/min)
- `POST /heartbeat` — CloudNode heartbeat (API key, 60/min)
- `GET /` — list nodes (admin)
- `GET /plan` — current plan, node usage, and limits (view)
- `POST /` — create node (admin, requires active billing + plan capacity, 20/hour)
- `GET /ws-status` — which nodes are WebSocket-connected (admin)
- `POST /self/decommission` — node-initiated factory reset (API key, 10/hour); called from CloudNode's `/wipe confirm` before the local wipe runs so a freshly-wiped box doesn't linger as a stale offline node
- `GET /{node_id}` — get node (admin)
- `DELETE /{node_id}` — delete node (admin; cascades cameras + flushes segment caches, 20/hour)
- `POST /{node_id}/rotate-key` — rotate API key (admin, 5/min)

**hls.py** (prefix `/api/cameras/{camera_id}`):
- `GET /stream.m3u8` — HLS playlist served from cache (JWT)
- `GET /segment/{filename}` — serve cached `.ts` segment from memory (JWT)
- `POST /push-segment?filename=…` — CloudNode pushes `.ts` segment into cache (API key, 1200/min)
- `POST /playlist` — update playlist (API key, 600/min)
- `POST /motion` — motion event delivery (API key, 120/min). HTTP-only post v0.1.61; the pre-v0.1.61 WebSocket-forwarding branch had no producer and was removed.

**audit.py** (prefix `/api`):
- `GET /audit/stream-logs` — stream access logs (admin)
- `GET /audit/stream-logs/stats` — aggregates by camera/user/day (admin)

**incidents.py** (prefix `/api/incidents`):
- `GET /` — list (admin; filters: `status`, `severity`, `camera_id`, `limit`, `offset`)
- `GET /counts` — aggregate counts (admin)
- `GET /{incident_id}` — detail with evidence list (admin)
- `PATCH /{incident_id}` — update status / severity / summary / report (admin)
- `DELETE /{incident_id}` — delete incident + cascade evidence (admin)
- `GET /{incident_id}/evidence/{evidence_id}` — stream snapshot or clip blob (admin)
- `GET /{incident_id}/evidence/{evidence_id}/playlist.m3u8` — synthetic single-segment HLS playlist for in-dashboard clip playback (admin)

**mcp_keys.py** (prefix `/api/mcp`):
- `POST /keys` — generate key; JSON body `{name, scopeMode, scopeTools?}`; returns plaintext `osc_...` once (admin + active billing)
- `GET /tools` — live tool catalog with read/write kind (admin)
- `GET /keys` — list MCP keys for the org (admin)
- `DELETE /keys/{key_id}` — revoke (admin)

**mcp_activity.py** (prefix `/api/mcp/activity`):
- `GET /stream` — SSE stream of live MCP tool calls (admin)
- `GET /recent` — recent tool calls (admin)
- `GET /sessions` — session summaries (admin)
- `GET /stats` — aggregated stats by tool / key / time (admin)
- `GET /logs` — filterable MCP tool call log (admin)
- `GET /logs/stats` — summary counts for logs (admin)

**motion.py** (prefix `/api/motion`):
- `GET /events` — list motion events; filters: `camera_id`, `hours`, `limit`, `offset` (view)
- `GET /events/stats` — per-camera aggregates (view)
- `GET /events/stream` — SSE motion feed for dashboard notifications (view)

**notifications.py** (prefix `/api/notifications`):
- `GET /` — paginated inbox, newest first; applies audience filter (view)
- `GET /unread-count` — cheap count for the bell badge (capped at 99) (view)
- `POST /mark-viewed` — bump `last_viewed_at` to now (view)
- `POST /clear-all` — empty the inbox for this org (admin)
- `POST /request-admin-promotion` — member-initiated admin-access request; fires the `member_promotion_requested` notification kind to org admins (view)
- `GET /stream` — SSE stream for the bell; audience filter applied server-side (view)
- `GET /email/preferences` — read the org's per-kind email toggles (view)
- `POST /email/preferences` — update the org's per-kind email toggles (admin)
- `GET /email/unsubscribe` — one-click unsubscribe link target.  Validates a signed JWT in the query string, flips the matching email toggle off, returns a confirmation HTML page (no auth required — auth comes from the signed token).

**install.py** (no prefix, no auth):
- `GET /install.sh` — CloudNode installer for Linux/macOS. Windows users install via the MSI from the latest CloudNode GitHub release; the legacy `/install.ps1` route was removed when the MSI shipped.
- `GET /mcp-setup.sh` / `GET /mcp-setup.ps1` — MCP client config helpers (separate from CloudNode install — these configure Claude / Cursor / etc. to talk to this Command Center)

**ws.py** (no prefix):
- `WS /ws/node` — WebSocket channel for CloudNode realtime (API key in query)
  - Node → Backend: `heartbeat`, `command_result`
  - Backend → Node: `ack`, `command` (`take_snapshot`, `list_snapshots`, `list_recordings`, `wipe_data`), `error`
  - Motion events do **not** flow over WS — they reach Command Center via `POST /api/cameras/{id}/motion`.  The pre-v0.1.61 wire format reserved an `event` / `motion_detected` frame for this path but it was never produced; the unused branch was removed in v0.1.61.

**sentinel.py** (prefix `/api/sentinel`):
- `GET /config` — read the org's Sentinel AI config + plan-aware `monthly_cap` + `plan_gated` flag (view; always 200 — non-eligible orgs get a read-only payload for the upgrade banner)
- `PATCH /config` — partial update of Sentinel config (admin + Pro/Pro Plus; 402 otherwise)
- `GET /runs` — list recent runs + stats (admin)
- `GET /runs/{run_id}` — single run with full tool trace (admin)
- `POST /runs/manual` — operator "Run now"; skips schedule + scope gates but cap-enforced (admin + Pro/Pro Plus; 429 at cap)
- `GET /runs/pending` — service-to-service.  Agent polls this on wakeup to drain runs across all orgs (FIFO).  Auth: `X-Sentinel-Agent-Key` header.
- `POST /runs/{run_id}/start` — service-to-service.  Agent claims a pending run (`pending → running`).  Idempotent.
- `POST /runs/{run_id}/complete` — service-to-service.  Agent posts terminal outcome (`incident` / `no_action` / `error`) + full tool trace.  Cross-checks `incident_id` belongs to the run's org.  Idempotent on terminal rows.

**webhooks.py** (prefix `/api/webhooks`):
- `POST /clerk` — Clerk subscription + organizationMembership events (Svix signature when `CLERK_WEBHOOK_SECRET` is set; 120/min)
- `POST /resend` — Resend bounce / complaint / unsubscribe webhooks (Svix signature when `RESEND_WEBHOOK_SECRET` is set); writes to `EmailSuppression` so subsequent sends short-circuit before the API call

**Top-level** (`main.py`):
- `GET /api/health` — minimal liveness for load balancers: `{"status": "healthy", "version": "2.1.1"}` (no auth)
- `GET /api/health/detailed` — verbose status for status-page polling and on-call diagnostics: `{status, version, uptime_seconds, started_at, time, checks: {database: {status, latency_ms}, hls_cache: {playlists_cached, segment_cameras}, viewer_usage: {pending_writes, status}, sse: {subscriber_orgs, subscriber_total}}}`. Public on purpose — every value is metric-shaped, never an org/camera/user identifier (pinned by a privacy regression test in `tests/test_health.py`).
- FastAPI docs: `/api-docs` (Swagger), `/api-redoc` (ReDoc), OpenAPI at `/api/openapi.json`. `/docs` is the React `DocsPage`.

## MCP Server

Mounted at `/mcp` via FastMCP streamable HTTP. Authenticates with `Authorization: Bearer osc_...` against `McpApiKey.key_hash`. Exposes **23 tools** (16 read + 7 write).

### Scope middleware

`ScopeMiddleware` (in `app/mcp/server.py`) runs before every `list_tools` and `call_tool` request:

1. Extracts the Bearer token from request headers via `get_http_headers()`
2. SHA-256-hashes the key and looks up the matching `McpApiKey` row
3. Computes the allowed-tool frozenset from `scope_mode` + `scope_tools`
4. Filters `list_tools` responses and raises `ToolError` on disallowed `call_tool` invocations

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
| `list_nodes` | CloudNodes + their status |
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

Configured in `main.py`:
```python
cors_origins = [
    "http://localhost:5173",
    "http://localhost:8000",
    "https://opensentry-command.fly.dev",
]
```
Plus `FRONTEND_URL` if set (validated: must have scheme, no trailing slash, no embedded whitespace). All methods and headers allowed; credentials allowed.

## Rate Limiting

`slowapi` with a tenant-aware key:
- `POST /api/nodes/validate`, `POST /api/nodes/register` — 10/min
- `POST /api/nodes/heartbeat` — 60/min
- `POST /api/nodes/{id}/rotate-key` — 5/min
- `POST /api/cameras/{id}/codec` — 30/min
- `POST /api/cameras/{id}/push-segment` — 1200/min
- `POST /api/cameras/{id}/playlist` — 600/min
- `POST /api/cameras/{id}/motion` — 120/min

HLS `GET` paths (`stream.m3u8`, `segment/{file}`) are not per-request rate limited — segment fetches are fast-path with no per-request DB work. They are however metered against the caller's monthly viewer-hour cap (see `Plan Enforcement` → Viewer-hours below): every served segment increments an in-memory counter, and the 429 kicks in when the counter exceeds `max_viewer_hours_per_month * 3600`.


## Webhook Handling

Two endpoints, both Svix-signed (signature verification mandatory in production):

### `POST /api/webhooks/clerk` — Clerk events

- Verifies signature with Svix when `CLERK_WEBHOOK_SECRET` is set; accepts unsigned JSON otherwise (dev mode)
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

`app/core/plans.py` owns plan-cap policy. `PLAN_LIMITS` is the source of truth for every per-tier number — camera/node/seat caps (abuse rails), monthly viewer-hour cap (the real tier axis), per-channel SSE concurrency cap, and log retention days. Three tiers: `free_org`, `pro`, `pro_plus`. (An earlier `business` slug was renamed to `pro_plus` during a Clerk-side reorg; the transitional alias was carried briefly and removed once every known org had rolled over — see ADR `docs/adr/0002-viewer-hour-billing.md` for the original tier names.)

Two entry points for plan resolution:

- `resolve_org_plan(db, org_id)` — nominal plan (what Clerk says the org pays for). Fast-path reads `Setting(org_plan)`; falls back to a throttled `clerk.organizations.get_billing_subscription` call for free/missing orgs. Used for the status-bar badge CloudNode shows the operator.
- `effective_plan_for_caps(db, org_id)` — plan to use for *cap enforcement*. Returns `resolve_org_plan` unless the org has been `payment_past_due` for more than `PAYMENT_GRACE_DAYS` (7), in which case it returns `"free_org"`. Used inside `enforce_camera_cap`; keeps the two concerns separate so brief card failures don't punish paying users but long-unpaid accounts don't keep getting Pro service.

### Camera cap

`enforce_camera_cap(db, org_id)` — idempotent. Orders the org's cameras by `created_at ASC`, keeps the oldest N (N = effective plan's `max_cameras`), flags the rest as `disabled_by_plan=True`. Oldest-first is deterministic and preserves long-running cameras with history. On upgrade, flags clear in the same call.

### Viewer-hours (the real tier axis)

`app/api/hls.py` maintains a per-org monthly viewer-second counter. Each successful `GET /segment/{filename}` serve calls `record_viewer_second(org_id)` which increments a thread-safe in-memory dict keyed on `(org_id, "YYYY-MM")`. The `_viewer_usage_flush_loop` background task flushes accumulated deltas to `OrgMonthlyUsage` rows every 60 seconds with one UPSERT per active org, so the hot serve path never touches SQLite.

Before serving each segment, `get_hls_segment` calls `_warm_cached_viewer_seconds(org_id)` to get the authoritative running total (cached DB value + pending in-memory delta). If that total is ≥ `max_viewer_hours_per_month * 3600`, the route returns HTTP 429 with `Retry-After: 3600` and an upgrade message. The first request for an org in a given process lifetime amortizes a DB read to warm the cache; thereafter it's all in-memory.

The counter is exposed on `GET /api/nodes/plan` as `usage.viewer_hours_used` / `usage.viewer_hours_limit` so the dashboard can render a live gauge.

### SSE caps

Every SSE broadcaster (`MotionBroadcaster`, `NotificationBroadcaster`, `McpActivityTracker`) accepts a per-call `cap` argument. Route handlers look up `get_plan_limits(user.plan)["max_sse_subscribers"]` and pass that; the broadcaster refuses `subscribe()` when the org is at cap and the route turns it into a 429 with the tier-specific cap in the message.

### MCP daily cap

`app/mcp/server.py::_RateLimiter` tracks two windows per API key hash: a rolling 60-second window (minute cap) and a rolling 24-hour window (daily cap). `RATE_LIMITS[plan]` supplies both numbers (Pro 30/min × 5000/day, Pro Plus 120/min × 30000/day). The `check()` method returns a `breach` reason string so the caller can generate a "you're spamming" vs "you've been looping for hours" error message.

### Log retention

`_log_cleanup_loop` in `main.py` runs daily. It collects distinct `org_id` values from the log tables, resolves each org's plan via `resolve_org_plan`, and deletes each log type older than that org's `log_retention_days`. Free gets 30d, Pro 90d, Pro Plus 365d.

**Triggers** for `enforce_camera_cap`:
1. Webhook: subscription lifecycle events (create/update/cancel/paid).
2. Register (`POST /api/nodes/register`): safety net for any missed webhook. Idempotent so cost is just one indexed query in the steady state.
3. Heartbeat (`POST /api/nodes/heartbeat`): gated on `payment_past_due=="true"`. Drives the time-based grace-expiration transition since no webhook fires for that clock tick.

**Push-segment gate** (`POST /api/cameras/{id}/push-segment`): when `camera.disabled_by_plan` is set, returns **HTTP 402** with a structured `plan_limit_hit` body (plan display name, cap, camera name, upgrade copy). CloudNode treats 402 as non-retryable and surfaces the suspension in its TUI.

**Heartbeat response** also carries `disabled_cameras: list[str]` scoped to the calling node, so CloudNode can skip the upload task entirely for suspended cameras (no 402 flood) and mark those rows `suspended (plan)` in its live dashboard.

## Background Loops

`main.py` starts EIGHT long-running tasks on lifespan startup:

| Task | Cadence | What it does |
|------|---------|--------------|
| `_log_cleanup_loop` | Every `LOG_CLEANUP_INTERVAL_HOURS` (24h) | Thin scheduler around `run_log_cleanup(db) -> dict` (extracted for direct test coverage). Iterates distinct `org_id` values across all log tables (StreamAccessLog, McpActivityLog, AuditLog, MotionEvent, Notification, EmailLog), resolves each org's plan, and deletes records older than that org's `log_retention_days` (30 / 90 / 365). Also sweeps terminal-state EmailOutbox rows older than 7 days (cross-org; pending/sending NEVER deleted regardless of age). |
| `_offline_sweep_loop` | Every `OFFLINE_SWEEP_INTERVAL_SECONDS` (30s) | Flips nodes/cameras whose `last_seen` is older than 90s from `status='online'` to `'offline'` and emits `Notification` rows + broadcasts SSE events |
| `_viewer_usage_flush_loop` | Every 60s | Flushes pending in-memory viewer-second counters to the `org_monthly_usage` table with one UPSERT per active org. Keeps the hot HLS-serve path O(1) in memory. |
| `_release_cache_refresh_loop` | Every `RELEASE_CACHE_REFRESH_INTERVAL_SECONDS` (600s) | Polls GitHub `/releases/latest` for the CloudNode repo so the heartbeat handler's `update_available` field stays fresh without blocking on GitHub per request. Cold-boot fallback is `LATEST_NODE_VERSION` env var. |
| `_disk_check_loop` | Every `DISK_CHECK_INTERVAL_SECONDS` (300s = 5min) | Polls `/data` disk usage. When ≥95%, fires a single `logger.error()` with structured `extra` fields (Sentry-captured). 6h re-emit debounce per process. **Operator-side only** — does NOT route through customer notifications (multi-tenant violation removed 2026-05-04). |
| `_motion_digest_loop` | Every `MOTION_DIGEST_INTERVAL_SECONDS` (60s) | Drains expired per-camera motion email cooldown anchors (Setting rows keyed `motion_email_cooldown_start:{camera_id}`). For each expired window: counts MotionEvent rows in the window, emits a `motion_digest` notification (which itself enqueues a digest email) if extras > 0, deletes the anchor regardless. Volume cap: 2 emails per cycle per camera. |
| `_sentinel_reaper_loop` | Every `SENTINEL_REAPER_INTERVAL_SECONDS` (300s = 5min) | Sweeps `SentinelRun` rows stuck in `running` for more than `STRANDED_RUN_AGE_MINUTES` (20 min) and marks them `outcome='error'` with a "stranded — agent never finished" note. Catches the rare case where the agent's own wall-clock cleanup wrapper doesn't fire (process crash, network partition); without this the run drawer would show a permanent in-progress spinner. |
| `email_worker_loop` (in `app/core/email_worker.py`, spawned from `main.py` lifespan) | Every `EMAIL_WORKER_INTERVAL_SECONDS` (5s) | Drains EmailOutbox `status='pending'` rows in batches of `EMAIL_WORKER_BATCH_SIZE`. Per row: check suppression list → call `email.send_email()` → mark `sent`/`failed`/`suppressed` → write EmailLog row. Reclaims rows stuck in `sending` for >60s (worker crash). Idempotency-Key on the Resend send protects against duplicate delivery on retry. |

## Key Patterns

**Tenant isolation:** every query filters by `org_id` from the authenticated user/node.

**Error handling:** Two layers. New endpoints raise `app.core.errors.ApiError(status, code, message, **extras)` for a structured envelope (`{detail: {error, message, ...extras}}`); the frontend's `services/api.js::parseErrorBody` reads `e.message` for toasts and `e.code` for branching. Older endpoints still use bare `HTTPException(detail="...")` and the frontend parser handles both shapes plus the rate-limit handler's top-level `{error, message, ...}` shape, so call sites never see `[object Object]`. Pydantic 422s are normalised through the same envelope by a `RequestValidationError` handler in `main.py`. ADR docstring on `ApiError` warns: REST-only — MCP tools at `/mcp` use `ToolError` (the JSON-RPC error envelope is fixed by the protocol).

**Database sessions:** `get_db()` dependency yields a SQLAlchemy session per request.

**In-memory segment cache:** live `.ts` segments live in `_segment_cache` (a `dict[camera_id, dict[filename, (bytes, ts)]]`) inside `hls.py`. Backend never touches S3 for live video. Recordings and snapshots live on the CloudNode. Incident snapshots + clips are stored inline on `IncidentEvidence.data` (LargeBinary).

**Codec detection:** CloudNode reports codec via `POST /api/cameras/{id}/codec` after the first segment. Stored on the Camera row and injected into HLS playlists as `#EXT-X-CODECS`.

**Notification broadcaster:** `notification_broadcaster` (in `notifications.py`) is a per-process pub/sub — SSE subscribers register per org + admin flag; `emit_camera_transition`, `emit_node_transition`, and motion event handlers write a `Notification` row then broadcast.

**Motion broadcaster:** the motion SSE stream pushes events that arrive on `POST /api/cameras/{id}/motion`.  Motion delivery is HTTP-only since CloudNode v0.1.61; the pre-v0.1.61 WebSocket variant had no producer on the node side and was removed.

**Shared Clerk token:** frontend's `useSharedToken` serialises the Clerk JWT for HLS.js's `xhrSetup` so segment fetches ride on the same auth as API calls.

**First-heartbeat UX:** when the admin creates a node, the dashboard stashes the new `node_id` in `localStorage` and `HeartbeatBanner` starts polling `GET /api/nodes/{node_id}` every few seconds. As soon as `last_seen` is non-null the banner auto-dismisses. Users don't have to refresh — it's a reassurance loop for the 30–60s window where the node is downloading ffmpeg / registering cameras.

**Role-split welcome hero:** `WelcomeHero.jsx` exports two components — `AdminWelcomeHero` shows the "Install a CloudNode → Camera goes live" checklist with CTAs into Settings + the install guide; `MemberWelcomeHero` shows a capability-focused welcome (live monitoring, motion alerts, team workspace) because members can't act on a setup checklist. `DashboardPage` picks the right one based on `is_admin`.

## Setup Scripts

`backend/scripts/mcp-setup.sh` + `mcp-setup.ps1` are served verbatim from `install.py`. They:
1. Accept `<api_key> <server_url>` (positional)
2. Detect installed MCP clients (Claude Code, Claude Desktop, Cursor, Windsurf)
3. Prompt the user for which ones to configure
4. Merge an `opensentry` entry into each client's JSON config (creating directories + backing up corrupted files)

**Windows invocation pattern** — `irm … | iex -Args …` **does not work** (`iex` has no `-Args`). Use the scriptblock pattern instead, which is what the dashboard prints:

```powershell
& ([scriptblock]::Create((irm <url>/mcp-setup.ps1))) '<api_key>' '<server_url>'
```

**Bash invocation** — when run via `curl … | bash -s --`, stdin is the piped script, so `read` would hit EOF immediately. The script falls back to `</dev/tty` when stdin isn't a TTY.

## Key Dependencies

- `fastapi` / `uvicorn` — Web framework and ASGI server
- `fastmcp` — Model Context Protocol server (streamable HTTP)
- `sqlalchemy` — ORM (SQLite both dev + prod; production runs on a Fly volume)
- `pydantic` — Request/response validation
- `clerk-backend-api` — Clerk authentication
- `pyjwt` — JWT token handling (V2 permission decoding + signed unsubscribe tokens)
- `jinja2` — Email template rendering (`backend/app/templates/emails/`)
- `resend` — Resend SDK for transactional email
- `slowapi` — Rate limiting (Redis-backed via `REDIS_URL` in production; in-memory fallback)
- `httpx` — HTTP client
- `svix` — Webhook signature verification (Clerk + Resend share the library)
- `sentry-sdk` — Error tracking (no-op when `SENTRY_DSN` is unset)
- `python-dotenv` — Environment variable loading

## Development Notes

- Database tables auto-created on startup via `Base.metadata.create_all()`
- Backend serves the React build as static files in production (SPA middleware in `main.py`)
- Frontend uses HLS.js for video playback with a Clerk JWT injected via `xhrSetup`
- `VITE_LOCAL_HLS=true` bypasses the backend and streams directly from CloudNode on localhost:8080 (for local dev only)
- Tests live in `backend/tests/` and run with `uv run pytest`; scope middleware has dedicated coverage (`test_mcp_keys.py`)
