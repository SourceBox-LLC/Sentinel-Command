<p align="center">
  <h1 align="center">Sentinel Command Center</h1>
  <p align="center">
    Cloud dashboard for managing and viewing your security cameras in real time.
    <br />
    <a href="https://opensentry-command.fly.dev">Live App</a>
    &middot;
    <a href="#quick-start">Quick Start</a>
    &middot;
    <a href="#api-reference">API Reference</a>
    &middot;
    <a href="https://github.com/SourceBox-LLC/OpenSentry-CloudNode">CloudNode</a>
  </p>
</p>

<p align="center">
  <a href="https://www.gnu.org/licenses/agpl-3.0"><img src="https://img.shields.io/badge/License-AGPL_v3-blue.svg" alt="License: AGPL v3"></a>
  <a href="https://www.python.org/"><img src="https://img.shields.io/badge/Backend-FastAPI_2.1-009688.svg" alt="FastAPI"></a>
  <a href="https://react.dev/"><img src="https://img.shields.io/badge/Frontend-React_19-61DAFB.svg" alt="React"></a>
</p>

---

Sentinel Command Center is the cloud hub for the Sentinel ecosystem (Sentinel by SourceBox). It receives live HLS video streams from [CloudNode](https://github.com/SourceBox-LLC/OpenSentry-CloudNode) devices on your local network, caches segments in memory, and proxies them to any browser. Authentication and multi-tenant isolation are handled by Clerk.

**What it does:**
- Receives live HLS video from CloudNode devices and proxies it to the browser via a same-origin in-memory cache — no object store, no presigned URLs
- Manages camera nodes and groups, with per-org Clerk authentication
- Multi-tenant with organization-based access control (V2 JWT permissions)
- Motion detection events from CloudNodes with per-camera aggregates and a live SSE feed
- Unified notification inbox for motion, camera/node status transitions, MCP key audit, member audit, CloudNode disk warnings, AI-agent incidents, and errors
- **Email notifications** via Resend — opt-in per-org per-kind, with per-camera cooldown + digest mode for high-volume motion events (14 notification kinds gated by 7 setting toggles; plus a hidden `welcome` kind that has no UI toggle)
- Audit logging for stream access, admin actions, and MCP tool calls
- MCP server exposing 23 tools (16 read, 7 write) so AI clients can view cameras, file incident reports with snapshots and short video clips, and read back past investigations — with per-key scoping (all / readonly / custom allow-list)
- **Sentinel AI agent** — webhook-driven serverless agent ([separate repo](https://github.com/SourceBox-LLC/SourceBox-Sentinel)) that auto-investigates motion events and incident_opened notifications using a vision-capable LLM ↔ MCP tool loop. Pro: 100 runs/month. Pro Plus: 500 runs/month. Per-org scoping via signed override header — one deployed agent serves every org with no cross-tenant state.

---

> **Looking for the product?** Sign up at the [live app](https://opensentry-command.fly.dev) — we host Sentinel by SourceBox as a SaaS, you don't run any servers. The repo is public for trust + transparency: every claim on `/security` and `/docs` points at the file that implements it.
>
> The Quick Start below is for **engineers who want to clone the repo and run it locally** — for code review, contributing fixes, or auditing what we run. It is **not** the install path for end users; users sign up at the link above. The CloudNode camera daemon (the piece that genuinely runs on customer hardware) is in a [separate repo](https://github.com/SourceBox-LLC/OpenSentry-CloudNode) — its own README walks through installation.

---

## Quick Start

### Prerequisites

- **Python** 3.12+ (backend declares `requires-python = ">=3.12"` in `pyproject.toml`)
- **Node.js** 18+
- **uv** ([Python package manager](https://docs.astral.sh/uv/))

### 1. Backend

```bash
cd backend
cp .env.example .env    # Edit with your Clerk keys
uv sync
uv run python start.py
```

API available at `http://localhost:8000`

### 2. Frontend

```bash
cd frontend
cp .env.example .env    # Set VITE_CLERK_PUBLISHABLE_KEY
npm install
npm run dev
```

App available at `http://localhost:5173`

### 3. Connect a CloudNode

Create a camera node from the Settings page, then run [CloudNode](https://github.com/SourceBox-LLC/OpenSentry-CloudNode) with those credentials. Cameras auto-register when the CloudNode first connects.

### 4. Connect an MCP client (optional)

From the MCP page, generate an API key and pick a scope (all / readonly / custom). The page prints a one-line installer that configures Claude Code, Claude Desktop, Cursor, or Windsurf in place:

```bash
# Linux / macOS
curl -fsSL https://opensentry-command.fly.dev/mcp-setup.sh | bash -s -- <api_key> <mcp_url>

# Windows (PowerShell)
& ([scriptblock]::Create((irm https://opensentry-command.fly.dev/mcp-setup.ps1))) '<api_key>' '<mcp_url>'
```

The scripts detect which clients you already have and merge an `opensentry` entry into each one's MCP config.

---

## Architecture

```
   CloudNode (Rust)                  Command Center                    Browser
  ┌──────────────┐            ┌───────────────────────┐         ┌──────────────┐
  │ USB Camera   │            │  FastAPI Backend      │         │  React 19    │
  │      ↓       │            │                       │         │              │
  │ FFmpeg (HLS) │──push─────→│  In-memory segment    │←─GET───→│  HLS.js      │
  │              │  segments  │  cache (~60 segs/cam) │  proxy  │  (video)     │
  │              │──register─→│  SQLite (Fly volume)  │  URLs   │              │
  │              │──heartbeat→│  Clerk Auth           │←─JWT───→│  Clerk Auth  │
  │              │──WS events │  FastMCP (/mcp)       │←──SSE───│  Motion feed │
  │              │            │  Resend (email out)   │         │              │
  └──────────────┘            └───────────────────────┘         └──────────────┘
```

**Video pipeline:** CloudNode transcodes USB camera video into HLS segments and pushes each `.ts` file directly to the backend via `POST /api/cameras/{id}/push-segment`. The backend caches segments in memory (60 per camera by default, ~60s buffer) and serves them through the same-origin proxy at `GET /api/cameras/{id}/segment/{file}`. The rewritten playlist contains relative segment URLs, so the browser's Clerk JWT auth header is automatically attached. No S3, no presigned URLs, no third-party storage in the live path. A global byte ceiling (`SEGMENT_CACHE_MAX_TOTAL_BYTES`, default 2 GiB) bounds total cache size across all cameras and evicts oldest-first when exceeded.

**Authentication:** Clerk handles user sign-up, login, and organization management. The backend validates JWT tokens (V1 and V2 permission formats) and extracts organization-scoped permissions. CloudNodes authenticate with API keys (SHA-256 hashed in the database) passed via `X-Node-API-Key`. MCP clients authenticate with `Authorization: Bearer osc_...` keys (also hashed).

**Storage:** Live segments live in the backend's in-memory cache; they expire automatically once `SEGMENT_CACHE_MAX_PER_CAMERA` is exceeded. Recordings and snapshots live on the CloudNode itself. **SQLite** is the production database (single-machine deploy on a Fly volume at `/data`); `DATABASE_URL` defaults to `sqlite:///./opensentry.db` for local dev. Incident snapshots and clips are stored inline on `IncidentEvidence.data` (LargeBinary) — evidence travels with the incident.

**Email:** Operator-critical notifications (camera offline, CloudNode offline, AI-agent incident, MCP key audit, CloudNode disk warning, member audit, motion w/ digest) flow through `EmailOutbox` → background worker → Resend transactional API. Per-org per-kind opt-in toggles (all default ON except motion, which defaults OFF). See `app/core/email_worker.py` and the `_motion_digest_loop` background task.

**Real-time:** CloudNodes maintain a WebSocket channel (`/ws/node`) used for commands, status, and motion events. The dashboard subscribes to SSE feeds for motion events (`/api/motion/events/stream`), notifications (`/api/notifications/stream`), and MCP activity (`/api/mcp/activity/stream`).

**Sentinel agent:** When a motion or incident_opened notification fires for a Pro/Pro Plus org with Sentinel configured, Command Center inserts a row into a `sentinel_runs` queue and POSTs an HMAC-signed wakeup webhook to the [Sentinel agent](https://github.com/SourceBox-LLC/SourceBox-Sentinel) on Fly.io. The agent boots, drains all pending runs across all orgs (per-call org scoping via `X-Agent-Org-Override` header against `SENTINEL_AGENT_MCP_KEY`), runs the LLM↔MCP loop for each, posts results back via `/api/sentinel/runs/{id}/complete`, then auto-stops. One deployed agent serves every org; per-run isolation comes from fresh MCP client + fresh messages array per run.

---

## Configuration

### Backend environment variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| **Auth** | | | |
| `CLERK_SECRET_KEY` | Yes | | Clerk backend API key |
| `CLERK_PUBLISHABLE_KEY` | Yes | | Clerk frontend key |
| `CLERK_WEBHOOK_SECRET` | No | | Svix signature secret for Clerk webhooks |
| **Storage** | | | |
| `DATABASE_URL` | No | `sqlite:///./opensentry.db` | SQLAlchemy connection string. Production uses `sqlite:////data/opensentry.db` on a Fly volume. |
| `FRONTEND_URL` | No | `http://localhost:5173` | Extra CORS origin (must include scheme, no trailing slash) |
| `REDIS_URL` | No | | Rate-limiter shared storage. Without it, slowapi falls back to per-process in-memory counters (fine for single-VM; required for multi-VM) |
| **HLS cache** | | | |
| `SEGMENT_CACHE_MAX_PER_CAMERA` | No | `60` | Segments cached in memory per camera (~1s each, ~60s buffer) |
| `SEGMENT_CACHE_MAX_TOTAL_BYTES` | No | `2147483648` | Hard byte ceiling on the SUM of all camera caches (2 GiB) — global eviction kicks in when exceeded |
| `SEGMENT_PUSH_MAX_BYTES` | No | `2097152` | Max bytes per pushed segment (2 MB) |
| `PLAYLIST_PUSH_MAX_BYTES` | No | `65536` | Max bytes per pushed playlist (64 KB) |
| `CLEANUP_INTERVAL` | No | `20` | Run cache eviction every N playlist updates |
| `INACTIVE_CAMERA_CLEANUP_HOURS` | No | `24` | Free caches for cameras offline this long |
| **Logs + sweeps** | | | |
| `LOG_RETENTION_DAYS` | No | `90` | Stream, MCP, audit, motion, notification, and email log retention (per-tier override via plan slug — Free 30, Pro 90, Pro Plus 365) |
| `OFFLINE_SWEEP_INTERVAL_SECONDS` | No | `30` | How often to flip stale `online` rows to `offline` |
| **Sentry (optional)** | | | |
| `SENTRY_DSN` | No | | Project DSN. In production this is auto-injected by Fly's Sentry extension (`fly ext sentry create`). Init is a no-op when unset. |
| `SENTRY_TRACES_SAMPLE_RATE` | No | `0.1` | Trace sample rate (10% keeps us inside Sentry's free-tier event budget) |
| **Email (Resend, optional)** | | | |
| `EMAIL_ENABLED` | No | `false` | Global kill-switch. Code can ship with it off; flip to `true` once DNS propagates and a smoke test passes. |
| `RESEND_API_KEY` | No | | Resend transactional API key (`re_…`) |
| `RESEND_WEBHOOK_SECRET` | No | | Svix signing secret for the bounce/complaint webhook (`whsec_…`) |
| `EMAIL_FROM_ADDRESS` | No | `notifications@sourceboxsentry.com` | Sender address (must be on a verified Resend domain) |
| `EMAIL_FROM_NAME` | No | `Sentinel by SourceBox` | Display name in the From header |
| `EMAIL_WORKER_INTERVAL_SECONDS` | No | `5` | Outbox-drain tick interval |
| `EMAIL_WORKER_BATCH_SIZE` | No | `20` | Max rows drained per tick (kept under Resend's 10 req/sec default) |
| `EMAIL_MAX_ATTEMPTS` | No | `3` | Retries before a row is marked `failed` permanently |
| **CloudNode version compatibility** | | | |
| `MIN_SUPPORTED_NODE_VERSION` | No | `0.1.0` | Reject older CloudNode register/heartbeat with HTTP 426 |
| `LATEST_NODE_VERSION` | No | `0.1.26` | Disaster fallback for `update_available`; runtime polls GitHub /releases/latest in normal operation |

### Frontend environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `VITE_CLERK_PUBLISHABLE_KEY` | Yes | Clerk publishable key |
| `VITE_API_URL` | No | Backend URL (default: `http://localhost:8000`) |
| `VITE_LOCAL_HLS` | No | Set `true` to stream directly from CloudNode on localhost |

---

## API Reference

### Cameras

| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| GET | `/api/cameras` | User | List all cameras |
| GET | `/api/cameras/{camera_id}` | User | Get camera details |
| POST | `/api/cameras/{camera_id}/snapshot` | User | Ask the node to capture a snapshot locally |
| POST | `/api/cameras/{camera_id}/recording` | User | Start/stop recording on the node |
| POST | `/api/cameras/{camera_id}/codec` | Node | Report video/audio codec (called by CloudNode, 30/min) |

### Camera Groups

| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| GET | `/api/camera-groups` | User | List groups |
| POST | `/api/camera-groups` | Admin | Create group |
| DELETE | `/api/camera-groups/{group_id}` | Admin | Delete group |
| PUT | `/api/cameras/{camera_id}/group` | Admin | Assign camera to a group |

### Camera Nodes

| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| GET | `/api/nodes` | Admin | List all nodes |
| POST | `/api/nodes` | Admin | Create a node (requires active billing + plan capacity) |
| GET | `/api/nodes/{node_id}` | Admin | Get node details |
| DELETE | `/api/nodes/{node_id}` | Admin | Delete node (cascades cameras + segment caches) |
| POST | `/api/nodes/{node_id}/rotate-key` | Admin | Rotate API key (5/min) |
| GET | `/api/nodes/plan` | User | Current plan, camera/node/viewer-hour usage, tier limits, grace-period countdown |
| GET | `/api/nodes/ws-status` | Admin | Which org nodes are currently WebSocket-connected |
| POST | `/api/nodes/validate` | None | Validate a `(node_id, api_key)` pair (used by CloudNode setup wizard, 10/min) |
| POST | `/api/nodes/register` | Node | CloudNode registration (10/min) |
| POST | `/api/nodes/heartbeat` | Node | CloudNode heartbeat (60/min) |

### HLS Streaming

| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| GET | `/api/cameras/{camera_id}/stream.m3u8` | User | HLS playlist (cached, with relative segment URLs) |
| GET | `/api/cameras/{camera_id}/segment/{file}` | User | Serve cached `.ts` segment from memory |
| POST | `/api/cameras/{camera_id}/push-segment` | Node | Push a `.ts` segment into the cache (1200/min) |
| POST | `/api/cameras/{camera_id}/playlist` | Node | Update playlist (600/min) |
| POST | `/api/cameras/{camera_id}/motion` | Node | HTTP fallback for motion events when WebSocket is offline (120/min) |

### Settings

| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| GET | `/api/settings` | User | All settings (notifications + timezone; recording is per-camera) |
| POST | `/api/settings/notifications` | Admin | Update notification toggles |
| POST | `/api/settings/timezone` | Admin | Set the org's IANA timezone (drives scheduled-recording windows) |
| PATCH | `/api/cameras/{camera_id}/recording-settings` | Admin | Per-camera recording policy (continuous_24_7 / scheduled_recording, mutually exclusive) |
| POST | `/api/cameras/{camera_id}/recording` | Admin | Manual record button — thin wrapper that flips `continuous_24_7` |
| POST | `/api/settings/danger/wipe-logs` | Admin | Permanently delete all stream + MCP + audit logs (Pro/Pro Plus only) |
| POST | `/api/settings/danger/full-reset` | Admin | Wipe all nodes, cameras, logs, and settings for the org (Pro/Pro Plus only) |

### Audit

| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| GET | `/api/audit-logs` | Admin | List audit logs |
| GET | `/api/audit/stream-logs` | Admin | Stream access logs |
| GET | `/api/audit/stream-logs/stats` | Admin | Stream access stats grouped by camera/user/day |

### Incident Reports

Agents author incidents via the MCP write tools below; admins review them from the dashboard's Incidents view.

| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| GET | `/api/incidents` | Admin | List incidents (filter by `status`, `severity`, `camera_id`) |
| GET | `/api/incidents/counts` | Admin | Aggregate counts (open, open critical, open high, total) |
| GET | `/api/incidents/{incident_id}` | Admin | Incident detail + all evidence metadata |
| PATCH | `/api/incidents/{incident_id}` | Admin | Acknowledge / resolve / dismiss / edit |
| DELETE | `/api/incidents/{incident_id}` | Admin | Delete incident (cascades to evidence blobs) |
| GET | `/api/incidents/{incident_id}/evidence/{evidence_id}` | Admin | Stream a snapshot or clip blob |
| GET | `/api/incidents/{incident_id}/evidence/{evidence_id}/playlist.m3u8` | Admin | Synthetic single-segment HLS playlist for in-dashboard clip playback |

### Motion Events

| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| GET | `/api/motion/events` | User | List motion events (filter: `camera_id`, `hours`, `limit`, `offset`) |
| GET | `/api/motion/events/stats` | User | Per-camera aggregates: event count, peak score, latest |
| GET | `/api/motion/events/stream` | User | SSE stream — real-time motion alerts for the dashboard |

### Notifications

| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| GET | `/api/notifications` | User | Paginated inbox (motion, camera/node online/offline, MCP key audit, member audit, CloudNode disk warnings, AI-agent incidents, motion digests, errors) |
| GET | `/api/notifications/unread-count` | User | Unread badge count (capped at 99) |
| POST | `/api/notifications/mark-viewed` | User | Mark the whole inbox viewed |
| GET | `/api/notifications/stream` | User | SSE stream for live bell updates |
| GET | `/api/notifications/email/preferences` | User | Returns per-org email toggle state + global kill-switch flag |
| POST | `/api/notifications/email/preferences` | Admin | Update per-org per-kind email opt-in toggles (audited) |
| GET | `/api/notifications/email/unsubscribe?t=…` | None (token in URL) | One-click unsubscribe from email footers (signed JWT, rate-limited 60/min) |

### Sentinel (AI agent config + run history)

Per-org config + run lifecycle for the [Sentinel agent](https://github.com/SourceBox-LLC/SourceBox-Sentinel). Admin endpoints surface plan-gated config and recent runs to the dashboard; agent-side endpoints (`/runs/pending`, `/start`, `/complete`) are gated by the shared `SENTINEL_AGENT_KEY` header for service-to-service callbacks.

| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| GET | `/api/sentinel/config` | User | Get the org's Sentinel config + plan-gated flag + plan-aware monthly cap (always 200; non-Pro/Pro-Plus orgs get a read-only payload) |
| PATCH | `/api/sentinel/config` | Admin (Pro+) | Partial update — toggles, schedule, scope, motion cooldown |
| GET | `/api/sentinel/runs` | Admin | List recent runs with stats (runs today/this month/incidents filed/pending, plan-aware monthly cap) |
| GET | `/api/sentinel/runs/{run_id}` | Admin | Single run with full tool trace |
| POST | `/api/sentinel/runs/manual` | Admin (Pro+) | Operator "Run now" — bypasses schedule + scope, still cap-enforced |
| GET | `/api/sentinel/runs/pending` | Sentinel Agent | Drain pending runs (FIFO across all orgs) |
| POST | `/api/sentinel/runs/{run_id}/start` | Sentinel Agent | Claim a pending run — transitions to `running` |
| POST | `/api/sentinel/runs/{run_id}/complete` | Sentinel Agent | Post terminal outcome (`incident` / `no_action` / `error`) + tool trace |

### MCP (for AI clients)

Streamable HTTP MCP server exposing **23 tools** (16 read + 7 write). Requires a Pro or Pro Plus plan + an API key generated from the dashboard. Each key has a scope (`all` / `readonly` / `custom`) enforced server-side by a middleware layer — agents never see or can invoke tools the key isn't scoped for.

| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| POST | `/mcp` | MCP Key | Streamable HTTP MCP endpoint (`Authorization: Bearer osc_...`) |
| GET | `/api/mcp/keys` | Admin | List MCP API keys |
| POST | `/api/mcp/keys` | Admin | Generate a new key (shown once; body: `{name, scopeMode, scopeTools?}`) |
| DELETE | `/api/mcp/keys/{key_id}` | Admin | Revoke a key |
| GET | `/api/mcp/tools` | Admin | Live tool catalog (name, description, read/write kind) |
| GET | `/api/mcp/activity/stream` | Admin | SSE stream of live MCP tool calls |
| GET | `/api/mcp/activity/recent` | Admin | Recent MCP tool calls |
| GET | `/api/mcp/activity/sessions` | Admin | MCP session summaries |
| GET | `/api/mcp/activity/stats` | Admin | Aggregated stats by tool / key / time |
| GET | `/api/mcp/activity/logs` | Admin | MCP tool call activity log (filterable) |
| GET | `/api/mcp/activity/logs/stats` | Admin | Summary counts for MCP logs |

See [AGENTS.md](AGENTS.md) for the full per-tool list.

### Installers (no auth)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/install.sh` | CloudNode installer for Linux/macOS (Windows = MSI from GitHub Releases) |
| GET | `/mcp-setup.sh` / `/mcp-setup.ps1` | MCP client setup helpers (configures Claude/Cursor/etc. — unrelated to CloudNode install) |

### System

| Method | Endpoint | Auth | Description |
|--------|----------|------|-------------|
| GET | `/api/health` | None | Liveness check (cheap; for load balancers) |
| GET | `/api/health/detailed` | None | Verbose status: DB latency, HLS cache depth, viewer-usage queue, SSE subscribers, disk usage, Resend transport state, email outbox depth. Public on purpose — every value is metric-shaped, never an org/camera/user identifier (pinned by a privacy regression test). |
| POST | `/api/webhooks/clerk` | Webhook | Clerk subscription + organization membership events (Svix-signed) |
| POST | `/api/webhooks/resend` | Webhook | Resend bounce/complaint events → suppression list (Svix-signed) |
| WS | `/ws/node` | Node (query) | CloudNode real-time channel (heartbeat, commands, motion) |

**Auth types:** `User` = Clerk JWT, `Admin` = Clerk JWT with admin permission, `Node` = `X-Node-API-Key` header, `MCP Key` = `Authorization: Bearer osc_...`.

---

## Permissions

Access control uses Clerk organizations with V1 or V2 JWT permission claims:

| Check | Grants |
|-------|--------|
| Clerk role `org:admin` / `admin` | Full access (nodes, groups, settings, audit logs, incidents, MCP keys) |
| `org:cameras:manage_cameras` permission | Manage cameras and nodes (alternative admin path) |
| Any authenticated org member | View cameras, streams, motion, notifications |

Admin access is required for node management, group management, settings, audit logs, incident review, MCP key management, and danger-zone operations. All authenticated org members can view cameras, streams, motion events, and their notification inbox.

---

## Data Models

All 18 ORM models live in `backend/app/models/models.py`; every row is scoped by `org_id` (except `ProcessedWebhook`, which is global webhook dedup).

| Model | Purpose |
|-------|---------|
| `Camera` | Camera device registered by a CloudNode; tracks codec, status, group |
| `CameraNode` | Physical CloudNode device; holds `api_key_hash` + codec info |
| `CameraGroup` | User-defined grouping (name, color, icon) |
| `Setting` | Per-org key/value store (recording config, plan slug, email toggles, motion cooldown anchors, etc.) |
| `AuditLog` | Admin / security-relevant audit trail |
| `StreamAccessLog` | Per-stream playback audit (user, IP, UA) |
| `McpApiKey` | Hashed MCP API key + scope (`scope_mode`, `scope_tools`) |
| `McpActivityLog` | Per-call MCP audit entry (tool, status, duration, args summary) |
| `Incident` | AI-generated incident report (open / acknowledged / resolved / dismissed) |
| `IncidentEvidence` | Inline snapshot, clip (MPEG-TS blob), or text observation attached to an incident |
| `MotionEvent` | Motion detection event reported by a node (`score`, `segment_seq`, timestamp) |
| `Notification` | Unified inbox entry (motion, motion_digest, camera/node transitions, MCP key audit, member audit, CloudNode disk warnings, AI-agent incidents, errors) |
| `UserNotificationState` | Per-`(clerk_user_id, org_id)` read cursor (`last_viewed_at`) |
| `OrgMonthlyUsage` | Per-`(org_id, year_month)` viewer-second counter — populated by `_viewer_usage_flush_loop` for billing-cap enforcement |
| `EmailOutbox` | Pending email send queue, drained by `email_worker_loop`. Survives process restart so a crash never drops a "camera offline" email. |
| `EmailLog` | Per-org email send/delivery audit (kind, status, message_id, error). Mirrors `AuditLog` shape. |
| `EmailSuppression` | Local mirror of Resend's suppression list (bounce / complaint / manual unsubscribe). Worker checks this before every send. |
| `ProcessedWebhook` | Svix-msg-id dedup table for both Clerk and Resend webhook handlers (idempotency guarantee under retry). |

---

## Project Structure

```
backend/
├── app/
│   ├── main.py                  # FastAPI app, CORS, SPA middleware, lifespan
│   │                            # (spawns 7 background loops: log cleanup, offline
│   │                            # sweep, viewer-usage flush, release-cache refresh,
│   │                            # disk check, motion digest, email worker)
│   ├── templates/emails/        # 22 Jinja2 templates — _layout.html.j2 + 7 kinds
│   │                            # (camera offline/recovered, node offline/recovered,
│   │                            # incident_created, MCP key created/revoked,
│   │                            # cloudnode_disk_low, member_added/role_changed/
│   │                            # removed, motion, motion_digest) × 3 files each
│   ├── api/
│   │   ├── cameras.py           # Cameras, groups, settings, audit logs, danger zone
│   │   ├── nodes.py             # CloudNode registration, heartbeat, CRUD, plan info,
│   │   │                        # cloudnode_disk_low alert helper
│   │   ├── hls.py               # HLS playlist + in-memory segment cache + push-segment + HTTP motion fallback
│   │   ├── audit.py             # Stream access logging
│   │   ├── incidents.py         # AI-generated incident reports (CRUD + evidence blobs)
│   │   ├── mcp_keys.py          # MCP key management + tool catalog
│   │   ├── mcp_activity.py      # MCP tool call logs, stats, SSE stream
│   │   ├── motion.py            # Motion events: queries, stats, SSE stream
│   │   ├── notifications.py     # Notification inbox + email kind map + email
│   │   │                        # cooldown gate + email prefs endpoints +
│   │   │                        # signed unsubscribe link
│   │   ├── install.py           # CloudNode + MCP setup scripts
│   │   ├── ws.py                # WebSocket channel (heartbeat, commands, motion events)
│   │   └── webhooks.py          # Clerk + Resend webhook handlers
│   ├── mcp/
│   │   └── server.py            # FastMCP server + 23 tools + ScopeMiddleware
│   ├── core/
│   │   ├── audit.py             # Audit-log writer (error-swallowing pattern)
│   │   ├── auth.py              # Clerk JWT validation (V1 + V2), dependencies
│   │   ├── config.py            # Environment loading (Config class)
│   │   ├── clerk.py             # Clerk SDK initialization
│   │   ├── database.py          # SQLAlchemy engine + session factory
│   │   ├── email.py             # Resend SDK touchpoint (single send entry)
│   │   ├── email_templates.py   # Jinja2 renderer (per-kind autoescape selection)
│   │   ├── email_unsubscribe.py # Signed JWT for one-click footer unsubscribe
│   │   ├── email_worker.py      # EmailOutbox drain loop + retry logic
│   │   ├── errors.py            # ApiError class — structured 4xx/5xx envelope
│   │   ├── limiter.py           # slowapi Limiter (tenant-aware key)
│   │   ├── migrations.py        # sync_schema column adder (stand-in for Alembic)
│   │   ├── plans.py             # PLAN_LIMITS, effective_plan_for_caps, grace
│   │   ├── recipients.py        # Clerk org member lookup + 5-min TTL cache
│   │   ├── release_cache.py     # GitHub /releases/latest cache for CloudNode update_available
│   │   └── sentry.py            # Sentry SDK init (no-op when SENTRY_DSN unset)
│   ├── models/models.py         # 18 ORM models (see table above)
│   └── schemas/schemas.py       # Pydantic request/response schemas
├── scripts/
│   ├── install.sh               # CloudNode installer for Linux/macOS (Windows = MSI)
│   └── mcp-setup.sh / .ps1      # MCP client config helpers (Claude / Cursor / etc.)
├── tests/                       # pytest — 450+ tests across security, MCP scoping,
│                                # motion, notifications, email transport, email
│                                # worker, motion email cooldown, motion digest loop,
│                                # offline sweep, billing/grace, Resend webhook
├── .env.example
├── pyproject.toml
└── start.py                     # Uvicorn entry point

frontend/
└── src/
    ├── pages/
    │   ├── LandingPage.jsx         # Public landing page
    │   ├── DashboardPage.jsx       # Camera grid, status, controls
    │   ├── SettingsPage.jsx        # Node + group + recording + danger zone
    │   ├── McpPage.jsx             # MCP keys + scope picker + agent activity (live SSE)
    │   ├── IncidentsPage.jsx       # AI- and human-filed incident reports + create flow
    │   ├── AdminPage.jsx           # Stream logs, MCP activity, audit trail
    │   ├── PricingPage.jsx         # Public pricing
    │   ├── SentinelPage.jsx        # Sentinel agent dashboard — config (triggers, schedule,
    │   │                           #   cooldown, scope), run history, manual "Run now"
    │   ├── LegalPage.jsx           # Terms, privacy, etc. (`/legal/:page`)
    │   ├── DocsPage.jsx            # In-app documentation (owns `/docs`)
    │   ├── SignInPage.jsx / SignUpPage.jsx
    │   └── TestHlsPage.jsx         # Admin-only HLS debug page
    ├── components/                 # HlsPlayer, CameraCard, IncidentReportModal,
    │                               # NotificationBell, KeyRotationModal, AddNodeModal,
    │                               # UpgradeModal, ToastContainer, Layout,
    │                               # HeartbeatBanner (first-heartbeat polling after
    │                               #   node creation, localStorage-backed),
    │                               # WelcomeHero (Admin / Member empty-state heroes),
    │                               # EmptyState, PublicLayout,
    │                               # LandingNav, LandingFooter, LoadingSpinner
    ├── hooks/                      # useNotifications, useMotionAlerts, usePlanInfo,
    │                               # useSharedToken, useToasts
    └── services/api.js             # Typed client for every backend endpoint
```

---

## Development

### Backend

```bash
cd backend
uv sync
uv run python start.py          # :8000 with auto-reload
uv run pytest                   # Runs the test suite
```

### Frontend

```bash
cd frontend
npm install
npm run dev                      # :5173
npm run build                    # Production build → backend/static/
```

### Database

SQLite for both development and production. Local dev defaults to `opensentry.db` in the backend directory; production uses `sqlite:////data/opensentry.db` on a Fly volume mounted at `/data`. Tables auto-create on startup via `Base.metadata.create_all()`; column drift is handled by the in-process `sync_schema` migration in `app/core/migrations.py` (see `docs/adr/0001-sync-schema-vs-alembic.md` for why we don't use Alembic).

---

## Deployment

Deployed on [Fly.io](https://fly.io) via GitHub Actions:

1. Frontend is built and copied to `backend/static/` by CI
2. FastAPI serves the React bundle; SPA middleware routes non-API requests to `index.html`
3. Live video segments are cached in the backend's process memory — no external storage
4. Clerk handles authentication (no user database needed)

**CI flow** (since 2026-05-04): the deploy workflow builds the Docker image locally on the GitHub runner via `docker/build-push-action` and pushes directly to `registry.fly.io` — no Fly remote builder, no depot.dev. `fly machine update --image <tag>` then rolls the live machine to the new image. See `.github/workflows/deploy.yml` for the long comment block tracing the three builder regressions that led here.

Memory sizing: each camera uses ~15 MB of cache (`SEGMENT_CACHE_MAX_PER_CAMERA=60 × ~250 KB per segment`), capped globally at `SEGMENT_CACHE_MAX_TOTAL_BYTES` (2 GiB default). A 4 GiB Fly instance comfortably handles ~150 active cameras. Bump `[[vm]] memory_mb` in `fly.toml` if you need more.

Production URL: [opensentry-command.fly.dev](https://opensentry-command.fly.dev)

---

## Troubleshooting

### Live video never shows up in the dashboard

Symptom: the camera appears in the grid but the tile stays black, or the HLS player loops the buffering spinner.

Check, in order:

1. **CloudNode heartbeat is arriving.** Visit `/settings`, find the node, confirm "Last seen" updates every ~30s. If it doesn't, the node never registered — check CloudNode logs for a `register` failure.
2. **Segments are being pushed.** In the browser devtools Network tab, look for `GET /api/cameras/{id}/segment/...` returning `200`. If they 404, the CloudNode isn't pushing — check `POST /api/cameras/{id}/push-segment` on the CloudNode side.
3. **The playlist is fresh.** `GET /api/cameras/{id}/stream.m3u8` — if the `#EXTINF` segment list is empty or the `segment/...` URLs are stale, the CloudNode's playlist upload stalled.
4. **The browser can decode the codec.** Admin-only `/test-hls` (the `TestHlsPage`) shows the raw SPS-derived codec string. If it's missing, the CloudNode's libx264 / hardware encoder wrote a non-conforming SPS — update the CloudNode to the latest release (see [CloudNode releases](https://github.com/SourceBox-LLC/opensentry-cloud-node/releases)) and restart it.

The companion runbook in the CloudNode repo (`docs/runbooks/video-not-showing.md`) walks through this from the node's side.

### "Your plan doesn't allow another node"

You're at the plan's node limit. `GET /api/nodes/plan` returns `{ nodes_used, nodes_limit }`. Upgrade from the Pricing page or delete an unused node from Settings.

### Motion events don't appear

- Motion reporting is controlled by the CloudNode's `motion.enabled` config — if it's off, no events will ever arrive.
- The dashboard subscribes to `/api/motion/events/stream` (SSE). If your deployment is behind a proxy that buffers responses, SSE may never flush — make sure proxy-buffering is disabled for `/api/*/stream`.
- The CloudNode's HTTP motion fallback is capped at 120/min per node (WebSocket is the primary channel and isn't rate-limited). Check `app/api/motion.py` and the per-route table in `AGENTS.md` if you need to tune this.

### MCP tools don't show up in my agent

- Make sure the agent is on Pro or Pro Plus — MCP access is plan-gated at the organization layer (see `app.core.auth` / `get_mcp_plan_info`).
- The installer scripts only patch configs for clients that already exist on the machine. If you installed Cursor *after* running `mcp-setup.sh`, re-run the installer.
- `GET /api/mcp/activity/stream` is the fastest way to confirm the agent is hitting your backend at all — if you see calls but `403`s, the key's `scope_mode` doesn't cover the tool the agent invoked.

---

## License

[AGPL-3.0](LICENSE) — source-available. The code is public for trust + transparency: customers can audit the implementation behind the security and privacy claims on the live site, and the file-level pointers from `/security` make every claim verifiable. Command Center is operated by SourceBox LLC as a SaaS — running it yourself is allowed under AGPL but isn't the intended use case, and AGPL §13 obligates anyone who modifies it and offers a network-accessible version to publish their changes.

This project is **not currently accepting external code contributions**. Bug reports and feature requests via [Issues](https://github.com/SourceBox-LLC/Sentinel-Command/issues) and [Discussions](https://github.com/SourceBox-LLC/Sentinel-Command/discussions) are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

---

<p align="center">
  <a href="https://opensentry-command.fly.dev">Sentinel Command Center</a>
  &middot;
  <a href="https://github.com/SourceBox-LLC/OpenSentry-CloudNode">CloudNode</a>
  &middot;
  Made by <a href="https://github.com/SourceBox-LLC">SourceBox LLC</a>
</p>
