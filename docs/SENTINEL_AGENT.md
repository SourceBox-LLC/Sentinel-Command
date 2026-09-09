# SourceBox Sentinel

Serverless AI security agent for [Sentinel Command Center](https://github.com/SourceBox-LLC/Sentinel-Command). Sleeps when idle; wakes on Command Center webhook; processes pending agent runs across every org; goes back to sleep. Vision-capable, time-bounded at every layer, multi-tenant via signed override header.

```
Command Center                  SourceBox Sentinel (Fly.io, auto-stop)
─────────────────              ──────────────────────────────────────
notification fires              ┌─ POST /wakeup  (HMAC-signed)
  → pending sentinel_run        │     ↓ verify HMAC
  → fire-and-forget webhook ────┘     ↓ fetch pending runs from CC
                                       for each run:
                                         POST /api/sentinel/runs/{id}/start
                                         run agent loop (LLM ↔ MCP tools)
                                         POST /api/sentinel/runs/{id}/complete
                                       return 200 — machine stays warm (see fly.toml)
```

## Architecture

- **LLM**: Ollama Cloud (default `qwen3.5:cloud` — vision + tools, 256K context) via the official `ollama` Python client. Vision-capable model required: the agent calls `view_camera` which returns a JPEG.
- **Tools**: Command Center's MCP server (23 tools — list/view/watch cameras, create/finalize incidents, attach evidence, etc.) over streamable HTTP transport.
- **Server**: Bare Starlette + uvicorn. No auth on its own state — every accepted request is HMAC-verified against the shared `SENTINEL_AGENT_KEY`.
- **Master of pending work**: Command Center's `sentinel_runs` table. The agent does not persist state; every wakeup re-fetches what's pending.

## Push or poll

The agent finds work two ways, selected by `AGENT_MODE`.

**`push` (default)** — Command Center calls `POST /wakeup` and the agent
drains. Requires CC to reach the agent *inbound*, so the agent has to be
publicly addressable. This is how the SourceBox-hosted agent runs, and
it is what makes Fly's auto-stop pay off: idle costs nothing.

**`poll`** — the agent asks CC for pending runs every
`POLL_INTERVAL_SECONDS`. **No inbound connectivity required**, so it runs
fine behind NAT on a home or office network. This is the mode for an
agent you host yourself, and it is the same shape CameraNode already
uses to talk to Command Center: outbound only.

Both modes share one drain implementation and one concurrency guard, so
they cannot diverge; `/wakeup` stays mounted in poll mode as a way to
force an immediate drain.

> **Don't set `poll` on a Fly deployment.** Polling keeps the machine
> awake, which defeats `auto_stop_machines` and bills you for idle time.
> Poll is for deployments you run yourself.

This is what makes mixed setups work — a self-hosted agent against the
cloud Command Center, or an entirely local stack.

## Running this agent yourself

Generate a key in Command Center under **MCP → Sentinel Agent Keys**. It
starts with `osa_` and is scoped to your organization alone: an agent
using it can never see another customer's cameras.

Then set two things on the agent:

```
AGENT_MODE=poll
SENTINEL_AGENT_KEY=osa_...
```

That's it — `OPENSENTRY_MCP_AGENT_KEY` defaults to `SENTINEL_AGENT_KEY`
when unset, because the issued key authenticates both the run queue and
the camera tools.

> Do **not** use the shared `SENTINEL_AGENT_KEY` from the first-party
> deployment. It is a multi-tenant secret that can act as *any* org via
> `X-Agent-Org-Override`, and must never be handed to a customer. The
> `osa_` keys exist precisely so it doesn't have to be.

## Plan tiers

Sentinel is gated end-to-end (UI, dispatcher, agent MCP auth) on the org's plan. Caps reset on the 1st of each calendar month in UTC.

| Plan      | Monthly runs | Note                                      |
|-----------|--------------|-------------------------------------------|
| Free      | 0            | Sentinel locked; UI shows upgrade banner. |
| Pro       | 100          | ~3 / day — casual home use.               |
| Pro Plus  | 500          | ~16 / day — commercial-shaped use.        |

When an org hits the cap, dispatch pauses for the rest of the month. There's no overage billing; the rest of SourceBox Sentry (recordings, motion notifications, dashboard, MCP) keeps working as normal.

## Reliability layers

A single run is bounded at every layer to prevent runaway loops or hung calls. All bounds are env-tunable via the matching `*_TIMEOUT_SECONDS` / `MAX_*` settings.

- **Per-LLM-call timeout** — 120s default. `asyncio.wait_for` around the Ollama chat call. Hung calls return clean errors instead of holding the machine alive.
- **Per-MCP-tool timeout** — 60s default. Stuck tools surface to the LLM as an error result so the model can retry or pivot.
- **Iteration cap** — 10 tool-call rounds per run (`MAX_AGENT_ITERATIONS`). Hits the cap → outcome is `error` with "investigation incomplete".
- **Wall-clock cap** — 270s per wakeup (kept under Fly's 300s `kill_timeout` so the cleanup runs before SIGKILL). The `process_with_timeout` wrapper catches `asyncio.TimeoutError`, identifies the in-flight run via a mutable `in_flight` handle, and best-effort POSTs `/api/sentinel/runs/{id}/complete` with `outcome=error` so the run lands terminal on Command Center instead of stranding in `running` state.
- **CC-side stranded-run reaper** — runs stuck in `running` for more than 20 minutes are marked `error` automatically by Command Center. Catches the rare case where the agent process crashes before its own cleanup wrapper fires.

## Endpoints

| Method | Path       | Description                                                                            |
|--------|------------|----------------------------------------------------------------------------------------|
| `GET`  | `/health`  | Liveness probe (keys-configured-or-not flags). No auth.                                |
| `POST` | `/wakeup`  | Webhook receiver. Drains pending runs. HMAC-verified.                                  |
| `POST` | `/`        | DEV-ONLY drain trigger. Disabled in production via `WEBHOOK_VERIFY_SIGNATURE=true`.    |

## Setup

### Local

```bash
cp .env.example .env
# Fill in OLLAMA_API_KEY, SENTINEL_AGENT_KEY, OPENSENTRY_MCP_AGENT_KEY
pip install -r requirements.txt
WEBHOOK_VERIFY_SIGNATURE=false uvicorn app.main:app --reload
```

In another terminal:

```bash
# Trigger a drain (skips HMAC because verify is off):
curl -X POST http://localhost:8080/
```

### Fly.io (production)

Two shared secrets, both have to match Command Center:

```bash
# Run-lifecycle callback secret (X-Sentinel-Agent-Key + HMAC):
fly secrets set SENTINEL_AGENT_KEY=$(python -c "import secrets; print(secrets.token_hex(32))")

# Multi-tenant MCP bearer (per-call org via X-Agent-Org-Override):
fly secrets set OPENSENTRY_MCP_AGENT_KEY=$(python -c "import secrets; print(secrets.token_hex(32))")

fly secrets set OLLAMA_API_KEY=...
fly deploy
```

Confirm:

```bash
fly ssh console -a sentinel-command -s --process-group agent -C "curl -s localhost:8080/health"
```

### CI/CD (GitHub Actions)

Pushes to `master` auto-deploy via `.github/workflows/deploy.yml`. To
enable on a fresh checkout:

```bash
# Generate a deploy-scoped Fly token for this app:
# Deploys come from Command Center's own workflow now — no separate token.

# Add it to the GitHub repo as a secret named FLY_API_TOKEN
# (Settings → Secrets and variables → Actions → New repository secret)
```

After that, every push to `master` runs a syntax check then
`flyctl deploy --remote-only`. Manual re-runs available via the
Actions tab → "Deploy to Fly.io" → "Run workflow".

## Wiring with Command Center

On the Command Center side, set the matching pair of secrets:

```bash
fly secrets set SENTINEL_AGENT_KEY=<same value as agent>
fly secrets set SENTINEL_AGENT_MCP_KEY=<same value as agent>
fly secrets set SENTINEL_AGENT_WEBHOOK_URL=http://sentinel-command.flycast:8080/wakeup -a sentinel-command
fly deploy
```

When a notification fires (motion / incident_created) AND the org has Sentinel configured AND the dispatch gate clears (camera in scope, schedule allows, under cap), Command Center inserts a pending `sentinel_runs` row and POSTs the wakeup webhook fire-and-forget. Sentinel wakes, drains all pending runs across all orgs, sleeps.

## Configuration

| Variable                       | Default                                     | Purpose                                                |
|--------------------------------|---------------------------------------------|--------------------------------------------------------|
| `AGENT_MODE`                   | `push`                                      | `push` = wait for CC's `/wakeup` webhook. `poll` = ask CC for work on an interval (works behind NAT). |
| `POLL_INTERVAL_SECONDS`        | `30`                                        | Poll mode only. Seconds between checks for pending runs. |
| `OLLAMA_API_KEY`               | required                                    | Ollama Cloud API key.                                  |
| `OLLAMA_HOST`                  | `https://ollama.com`                        | Ollama API endpoint.                                   |
| `OLLAMA_MODEL`                 | `qwen3.5:cloud`                             | Model id. Must be vision-capable (view_camera returns JPEGs). |
| `MAX_AGENT_ITERATIONS`         | `10`                                        | Max tool-call loops per run.                           |
| `MAX_TOKENS`                   | `2048`                                      | Max output tokens per LLM response.                    |
| `LLM_CALL_TIMEOUT_SECONDS`     | `120`                                       | Per-LLM-call timeout — wraps the Ollama chat call in `asyncio.wait_for` so hung connections return clean errors instead of holding the machine alive. |
| `MCP_TOOL_TIMEOUT_SECONDS`     | `60`                                        | Per-MCP-tool-call timeout. Stuck tools surface to the LLM as error results so the model can retry or pivot. |
| `OPENSENTRY_API_BASE`          | `https://sentinel-command.fly.dev`          | Command Center base URL. `OPENSENTRY_MCP_URL` is derived from it. |
| `SENTINEL_AGENT_KEY`           | required                                    | Run-lifecycle callback secret. Match CC.               |
| `OPENSENTRY_MCP_AGENT_KEY`     | falls back to `SENTINEL_AGENT_KEY`          | Multi-tenant MCP bearer. Set explicitly ONLY for the first-party deployment, where it differs. Self-hosted: leave unset. |
| `OPENSENTRY_MCP_URL`           | derived from `OPENSENTRY_API_BASE` + `/mcp` | Override only if MCP lives elsewhere.                  |
| `WEBHOOK_VERIFY_SIGNATURE`     | `true`                                      | Hard-disable HMAC for local dev. Always on in prod.    |
| `SENTRY_DSN`                   | unset                                       | Error tracking. No-op when unset. With `min_machines_running=0` the agent sleeps between wakeups, so a run that errors every time is otherwise invisible. |
| `SENTRY_ENVIRONMENT`           | `production`                                | Tags events so a local run doesn't pollute prod.       |
| `SENTRY_TRACES_SAMPLE_RATE`    | `0.1`                                       | Performance sampling.                                  |

## Project structure

```
app/
  main.py            Starlette app, /wakeup HMAC handler, /health, dev /
  processor.py       Drain loop: list pending → start → run → complete
  agent.py           LLM ↔ MCP tool conversation for one run
  prompts.py         Trigger-specific system prompts (motion / manual / …)
  llm.py             Ollama AsyncClient wrapper
  mcp_client.py      MCP client (streamable HTTP), tool dispatch
  sentinel_client.py HTTP client back to Command Center
  config.py          Pydantic Settings from env
```

## Multi-tenancy model

ONE deployed agent serves every org. Per-call org scoping happens at the MCP layer via the `X-Agent-Org-Override` header — the processor sets it to each run's `org_id` before connecting MCP, and the server uses that as the authoritative scope (rejecting if the org isn't on Pro / Pro Plus or has Sentinel disabled).

Per-run isolation comes from how the loop is built, not from per-org credentials:

- Fresh `MCPClientManager` per run (built and torn down inside `_process_one_run`)
- Fresh `messages` array per run (Agent.run is stateless)
- Per-run override header from `run.org_id` (set when CC dispatched the run)
- Activity log writes one row per tool call, attributed to the override org

Trust model depends on which key the agent holds. The **first-party** deployment uses two shared secrets (`SENTINEL_AGENT_KEY` + `OPENSENTRY_MCP_AGENT_KEY`) and has cross-org access — compromise = act-as-any-org. A **self-hosted** agent holds an `osa_` key scoped to one org by its Command Center database row, so a compromise is bounded to that org and is revocable from the UI. Mitigations: Fly secrets only, audit log per call, per-org rate limiting on the MCP server side, rotate by changing the secrets on both sides.

## Limitations

- **No retry of failed webhook delivery**: if Command Center's wakeup POST fails (Sentinel cold-starting too slowly, network blip), the run sits pending. The next trigger that fires another wakeup also drains the prior pending. No dead-letter queue yet — but stranded runs (those that *did* claim but never completed) are reaped automatically; only the dispatch-side webhook miss is still uncaught.
- **No cron sweeps**: `trigger_type=scheduled` is mapped in the prompt set but Command Center doesn't yet emit them.
- **Sequential per wakeup**: multiple pending runs are processed one after another inside a single wakeup. Async fan-out is a future optimization.
- **Concurrent wakeups can claim the same run**: two `/wakeup` deliveries to the same machine can both call `/start` on the same row. Command Center's `start` endpoint is idempotent so it's not a data integrity issue, just wasted tool calls + a small risk of duplicate incidents. An `asyncio.Lock` around `process_with_timeout` is the planned fix when concurrent wakeups become real.
