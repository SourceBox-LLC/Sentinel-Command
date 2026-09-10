# Sentinel system architecture

How the whole system fits together — every repository, every deployed service, and the paths between them. Start here if you're new, or if you need to know where something lives before changing it.

`README.md` is setup and reference for this repo. `AGENTS.md` is the deep architecture reference for Command Center's internals. **This file is the level above both**: the system, not the codebase.

*Verified against the running infrastructure on 2026-09-09.*

## The pieces

### Repositories

| Repo | What it is | Language |
| ---- | ---------- | -------- |
| **Sentinel-Command** (this one) | Command Center: API, dashboard, MCP server, and the Sentinel AI agent | Python + React |
| **Sentinel-CameraNode** | The binary customers install on the machine their cameras are on | Rust |
| **Sentinel-License-Service** | Validates self-hosted licence keys | Python |
| **Sentinel-Sync-Service** | Optional cloud mirror for self-hosted installs | Python |
| **Sentinel Home Assistant** | Home Assistant integration | Python |

`SourceBox-Sentinel` was the AI agent's repository until 2026-09-09. It is **archived, not deleted** — the agent's pre-move commit history exists only there, because the move was squash-merged.

### Deployed services (Fly.io)

Four apps. Command Center runs **two process groups from one image** — Fly gives one image per app, and process groups differ only by command.

| App | Process | Memory | Runs | Purpose |
| --- | ------- | ------ | ---- | ------- |
| `sentinel-command` | `app` | 1 GB | always | API, SPA, MCP server, in-memory video cache, 8 background loops |
| `sentinel-command` | `agent` | 512 MB | always | Sentinel AI agent |
| `sentinel-license` | `app` | 256 MB | **scales to zero** | Licence validation |
| `sentinel-sync` | `app` | 256 MB | **scales to zero** | One-way mirror receiver |
| `sentinel-postgres` | `app` | 512 MB | always | One cluster, three databases |

**Why two of these scale to zero and two don't.** This is a cross-service comparison, so it lives here rather than in any one service's docs.

Fly's proxy waits **~8s** for an auto-started machine to bind its port. That single number decides it:

| Service | Boot | Verdict |
| ------- | ---- | ------- |
| `sentinel-sync` | ~3s | clears it |
| `sentinel-license` | ~4s | clears it |
| `sentinel-command` / `agent` | ~10s | **misses it** — stays warm |

Boot time alone isn't sufficient; a missed call also has to be cheap. It is for both sleepers — a failed licence check-in falls into a grace window measured in days, and a failed sync push simply retries next cycle with the operator's local database still authoritative. Command Center's web tier serves live video and never sleeps.

## The signal path — camera to browser

This is the part most often assumed to work differently. There is **no object storage anywhere in the live path** — no S3, no Tigris, no presigned URLs. Segments live in Command Center's process memory and are evicted as they age out.

1. **CameraNode** cuts HLS segments with FFmpeg — 1-second `.ts` files by default.
2. **CameraNode pushes** them: `POST /api/cameras/{id}/push-segment` with the raw body and an `X-Node-API-Key` header. The node dials *out*, so no inbound port opens on the customer's network.
3. **Command Center** stores bytes in `_segment_cache[camera_id][filename]`, evicting oldest past `SEGMENT_CACHE_MAX_PER_CAMERA`.
4. **CameraNode pushes the playlist** separately: `POST /api/cameras/{id}/playlist`.
5. **Command Center rewrites** the playlist's segment filenames to relative `segment/<file>` proxy URLs, so the browser learns nothing about the node's own addressing.
6. **Browser** plays it as ordinary HLS. A camera that stops heartbeating flips to `offline` via the sweep loop.

The cache is bounded, and its ceiling is **coupled to the machine's memory** — raise one without the other and the kernel OOM-killer takes every org's streams down at once, well before the cache's own eviction can help. Both numbers, and why they move together, are in the `[env]` comment in `fly.toml`.

## The AI agent

Command Center owns the queue; the agent is a worker draining it. That single decision is what lets the same code run hosted or on someone else's hardware.

- **Push** (hosted default) — CC fires an HMAC-signed wakeup at `http://sentinel-command.flycast:8080/wakeup`, internal over 6PN.
- **Poll** — the agent asks CC for pending runs on an interval. No inbound connectivity, so it works behind NAT. Same shape CameraNode uses.

A run: claim via `POST /runs/{id}/start` → investigate through MCP tools → report via `POST /runs/{id}/complete` with `incident`, `no_action`, or `error`. Bounded at every layer — per-call, per-tool, iteration count, and wall clock — with a CC-side reaper for runs that strand anyway.

The model is a config string (`LLM_MODEL`, via LiteLLM). **Changing it on the hosted deployment moves customer camera imagery to a different processor** — see `legal/SUB_PROCESSORS.md` before you do.

Full detail: [SENTINEL_AGENT.md](SENTINEL_AGENT.md).

## Who can talk to it

There is no single "API key". Every class of caller has its own credential, scope, and revocation path.

| Credential | Used by | Scope |
| ---------- | ------- | ----- |
| Clerk JWT | Browser users (hosted) | Per-user, per-org, role-aware |
| Local auth | Browser users (self-hosted) | Single admin, `AUTH_PROVIDER=local` |
| Node API key | CameraNodes | One node; hashed at rest, rotatable |
| MCP API key | Claude and other MCP clients | Org + `readonly` or full; daily cap |
| Integration key | Home Assistant | Org-wide camera read |
| `osa_` agent key | Sentinel AI agent | Per-org, issued in the dashboard |

A `readonly` MCP key is intersected with the read-tool set **in middleware**, so scope is enforced before a tool runs rather than inside each one. Tool inventory and the read/write split: `../AGENTS.md` → MCP Server.

## Data

**Hosted — Postgres.** Three databases on the single `sentinel-postgres` cluster: `sentinel_command`, `sentinel_license`, `sentinel_sync`. Isolation is by *role*, not by cluster — each service's role owns exactly one database, `CONNECT` is revoked from `PUBLIC`, and none is a superuser. One cluster rather than three was a deliberate cost decision; roles supply the isolation separate clusters would have charged for.

**Self-hosted — SQLite.** The same codebase. This is why the backend suite is parametrised across **both dialects** and a failure in either blocks the deploy.

Schema changes run through a `sync_schema()` sweep on every boot rather than Alembic — see [ADR 0001](adr/0001-sync-schema-vs-alembic.md).

Backups: nightly `pg_dump` for `sentinel_command` and `sentinel_license`, with restores rehearsed rather than assumed. `sentinel_sync` deliberately has none — it holds a mirror whose source of truth is the operator's local SQLite. See [DISASTER_RECOVERY.md](runbooks/DISASTER_RECOVERY.md).

## Two ways to run it

| | Hosted | Self-hosted |
| --- | ------ | ----------- |
| Auth | Clerk | Local admin |
| Database | Postgres | SQLite |
| Cameras, recording, motion, MCP | Plan-limited | **Free and unrestricted** |
| Sentinel AI | Plan-limited | Unlocked by licence key |
| Agent | `agent` process group | Bundled; `python -m app.sentinel_agent` |

Only Sentinel AI is gated for self-hosters, because it is the one feature with an ongoing per-run cost. Everything else ships unlocked.

Plans are enforced on five axes: camera cap, **viewer-hours** (the real tier axis — see [ADR 0002](adr/0002-viewer-hour-billing.md)), SSE caps, MCP daily cap, and log retention.

## How code ships

`.github/workflows/deploy.yml` builds one image and `flyctl deploy` reconciles both of Command Center's process groups. License and Sync each deploy from their own repo's `Test & Deploy` workflow.

**Path filtering is asymmetric, and that is load-bearing.** `push` is filtered (docs and Markdown only); `pull_request` is **never** filtered. `master` requires three status checks, and GitHub reports *no status at all* for a workflow a path filter skipped — so a filtered PR trigger would hang every PR that missed it, presenting as a stuck check rather than a config error.

**Known gap:** GitHub does not trigger `on: push` workflows for commits pushed with `GITHUB_TOKEN`, so an auto-merged Dependabot PR lands on `master` **without deploying**. CodeQL still goes green on those commits, which hides it convincingly. Closing it properly needs a PAT; meanwhile `deploy.yml` carries a `workflow_dispatch` trigger. This is why no repo here uses an auto-merge workflow except Command Center.

## Where to go next

- [SENTINEL_AGENT.md](SENTINEL_AGENT.md) — the AI agent in depth
- [runbooks/ON_CALL.md](runbooks/ON_CALL.md) — "the app is broken"
- [runbooks/DISASTER_RECOVERY.md](runbooks/DISASTER_RECOVERY.md) — "the data is gone"
- [LAUNCH_HANDOFF.md](LAUNCH_HANDOFF.md) — what's left before paying customers
- [adr/](adr/) — why non-obvious decisions were made
- `../AGENTS.md` — Command Center's internals, in detail
