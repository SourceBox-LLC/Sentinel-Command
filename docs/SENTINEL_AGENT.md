# Sentinel AI agent

The AI security agent that investigates motion events and files incidents. It lives in this repo at `backend/app/sentinel_agent/` and runs as the **`agent` process group** of the `sentinel-command` Fly app — its own machine, the same image, one deploy.

It used to be a separate repository (`SourceBox-Sentinel`, archived 2026-09-09) deploying to a separate Fly app (`sourcebox-sentinel`, destroyed the same day). If you find a reference to either, it is stale.

```
Command Center (process "app")        Sentinel AI agent (process "agent")
──────────────────────────────       ────────────────────────────────────
notification fires                    ┌─ POST /wakeup  (HMAC-signed)
  → pending sentinel_run              │     ↓ verify HMAC
  → fire-and-forget webhook ──────────┘     ↓ fetch pending runs from CC
     over .flycast (6PN, internal)          for each run:
                                              POST /api/sentinel/runs/{id}/start
                                              run agent loop (LLM ↔ MCP tools)
                                              POST /api/sentinel/runs/{id}/complete
                                            return 200
```

## Architecture

- **LLM**: any provider, via [LiteLLM](https://docs.litellm.ai/). `LLM_MODEL` is the only setting that changes — `ollama_chat/qwen3.5:cloud` today, `anthropic/claude-sonnet-5` for a bring-your-own-key deployment, or an OpenAI-compatible endpoint for a custom model. **Must support tool calling *and* image input**: the agent fans out tool calls and feeds camera frames back in, so a text-only model does not degrade, it fails every run.
- **Tools**: Command Center's MCP server — cameras, incidents, evidence — over streamable HTTP. Inventory in `../AGENTS.md` → MCP Server.
- **Server**: Starlette + uvicorn. No auth on its own state — every accepted request is HMAC-verified against the shared `SENTINEL_AGENT_KEY`.
- **Master of pending work**: Command Center's `sentinel_runs` table. The agent persists nothing; every wakeup re-fetches what's pending.

### Why a separate process group, not a thread

A run holds base64 camera frames for the length of its wall-clock budget, and the web machine's segment cache is already budgeted most of that machine's memory (see the `[env]` comment in `fly.toml`). Sharing one machine is how the OOM killer takes every org's live streams down at once.

Being a separate *app* was never what bought that isolation — a separate process group is.

### Why the machine stays warm

`min_machines_running = 1`, deliberately, even though this worker's shape screams scale-to-zero.

This process takes ~10s to bind its port (Python + the MCP SDK + Sentry + a deferred LiteLLM import), which is longer than Fly's proxy waits for a machine it auto-started. Every wakeup against a stopped machine came back `RemoteDisconnected`. It was ~7s before LiteLLM — always marginal, and the migration only exposed it.

Keeping one small machine warm costs about $2/month and removes cold starts from the wakeup path instead of racing them. The proxy's actual budget, and how the sibling services compare against it, are in [ARCHITECTURE.md](ARCHITECTURE.md#deployed-services-flyio); the deployment reasoning is in the `[[services]]` comment in `fly.toml`.

## Push or poll

Selected by `AGENT_MODE`.

**`push` (default)** — Command Center calls `POST /wakeup` and the agent drains. Inside the Fly app this is an internal call over `.flycast`, so nothing is publicly addressable.

**`poll`** — the agent asks CC for pending runs every `POLL_INTERVAL_SECONDS`. **No inbound connectivity required**, so it runs fine behind NAT on a home or office network. This is the mode for an agent you host yourself, and it's the same outbound-only shape CameraNode already uses.

Both modes share one drain implementation and one concurrency guard, so they cannot diverge; `/wakeup` stays mounted in poll mode as a way to force an immediate drain.

## Running the agent yourself

You do **not** need a separate install — a self-hosted Command Center already contains the agent. Running it separately is only for putting it on different hardware.

Generate a key in Command Center under **MCP → Sentinel Agent Keys**. It starts with `osa_` and is scoped to your organization alone.

```bash
AGENT_MODE=poll
SENTINEL_AGENT_KEY=osa_...
LLM_API_KEY=...            # or OLLAMA_API_KEY for the default provider
python -m app.sentinel_agent
```

`OPENSENTRY_MCP_AGENT_KEY` defaults to `SENTINEL_AGENT_KEY` when unset, because the issued key authenticates both the run queue and the camera tools.

> Do **not** use the shared `SENTINEL_AGENT_KEY` from the first-party deployment. It is a multi-tenant secret that can act as *any* org via `X-Agent-Org-Override`, and must never be handed to a customer. The `osa_` keys exist precisely so it doesn't have to be.

## Plan tiers

Gated end-to-end (UI, dispatcher, agent MCP auth) on the org's plan. Caps reset on the 1st of each calendar month, UTC.

| Plan | Monthly runs | Note |
| -------- | ------------ | ----------------------------------------- |
| Free | 0 | Sentinel locked; UI shows upgrade banner. |
| Pro | 100 | ~3 / day — casual home use. |
| Pro Plus | 500 | ~16 / day — commercial-shaped use. |
| Self-hosted | unlimited | Unlocked by a license key — see the License Service. |

At the cap, dispatch pauses for the rest of the month. No overage billing; recordings, motion notifications, dashboard and MCP keep working.

## Reliability layers

A run is bounded at every layer. All bounds are env-tunable.

- **Per-LLM-call timeout** — 120s. `asyncio.wait_for` around the LiteLLM call. Wrapped despite LiteLLM taking its own `timeout` because that one bounds the HTTP request, not the whole call: a provider that accepts the connection then stalls mid-stream would otherwise hold the machine until the wall clock fires.
- **Per-MCP-tool timeout** — 60s. Stuck tools surface to the LLM as an error result so the model can retry or pivot.
- **Iteration cap** — 10 tool-call rounds per run (`MAX_AGENT_ITERATIONS`). Hitting it → outcome `error`, "investigation incomplete".
- **Wall-clock cap** — 270s per wakeup, under Fly's 300s `kill_timeout` so cleanup runs before SIGKILL. `process_with_timeout` catches the timeout, identifies the in-flight run, and best-effort POSTs `complete` with `outcome=error` so the run lands terminal instead of stranding in `running`.
- **CC-side stranded-run reaper** — runs stuck in `running` for >20 minutes are marked `error` by Command Center. Catches the case where the agent crashes before its own cleanup fires.

## Endpoints

| Method | Path | Description |
| ------ | --------- | ------------------------------------------------------------------ |
| `GET` | `/health` | Liveness probe. No auth. |
| `POST` | `/wakeup` | Webhook receiver. Drains pending runs. HMAC-verified. |
| `POST` | `/` | DEV-ONLY drain trigger. Disabled in prod via `WEBHOOK_VERIFY_SIGNATURE=true`. |

## Running it locally

The agent resolves against `backend/`'s dependency set — there is no separate project to install.

```bash
cd backend
uv sync --extra dev

OLLAMA_API_KEY=... \
SENTINEL_AGENT_KEY=dev-secret \
AGENT_MODE=poll \
OPENSENTRY_API_BASE=http://localhost:8000 \
WEBHOOK_VERIFY_SIGNATURE=false \
  uv run python -m app.sentinel_agent
```

With verification off you can force a drain:

```bash
curl -X POST http://localhost:8080/
```

## Deployment

There is nothing agent-specific to deploy. `.github/workflows/deploy.yml` builds one image and `flyctl deploy` reconciles both process groups; `[processes]` in `fly.toml` gives each its command.

Secrets live on the `sentinel-command` app and are shared by both groups:

```bash
fly secrets set OLLAMA_API_KEY=... -a sentinel-command
fly secrets set SENTINEL_AGENT_KEY=$(python -c "import secrets; print(secrets.token_hex(32))") -a sentinel-command
fly secrets set SENTINEL_AGENT_MCP_KEY=$(python -c "import secrets; print(secrets.token_hex(32))") -a sentinel-command
fly secrets set SENTINEL_AGENT_WEBHOOK_URL=http://sentinel-command.flycast:8080/wakeup -a sentinel-command
```

`SENTINEL_AGENT_WEBHOOK_URL` points at the app's own `agent` process group over 6PN. That requires a private IPv6 (`fly ips allocate-v6 --private`) — without it `.flycast` does not resolve and every wakeup fails to connect.

The agent reads Command Center's `SENTINEL_AGENT_MCP_KEY` directly (via an `AliasChoices` on `opensentry_mcp_agent_key`), so the first-party deployment needs no duplicate copy of it under the agent's own name.

Confirm:

```bash
fly ssh console -a sentinel-command -s --process-group agent -C "curl -s localhost:8080/health"
```

## Configuration

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `AGENT_MODE` | `push` | `push` = wait for `/wakeup`. `poll` = ask CC on an interval (works behind NAT). |
| `POLL_INTERVAL_SECONDS` | `30` | Poll mode only. |
| `LLM_MODEL` | falls back to `ollama_chat/{OLLAMA_MODEL}` | LiteLLM model string. The only setting needed to change provider. Must support tool calling + image input. |
| `LLM_API_KEY` | falls back to `OLLAMA_API_KEY` | Provider credential. |
| `LLM_API_BASE` | falls back to `OLLAMA_HOST` for the Ollama path | Override only for self-hosted or proxied endpoints. |
| `OLLAMA_API_KEY` | — | Default provider's key. Required unless `LLM_API_KEY` is set; boot fails with neither. |
| `OLLAMA_HOST` | `https://ollama.com` | Default provider endpoint. |
| `OLLAMA_MODEL` | `qwen3.5:cloud` | Model id used to build `LLM_MODEL` when it is unset. |
| `MAX_AGENT_ITERATIONS` | `10` | Max tool-call loops per run. |
| `MAX_TOKENS` | `2048` | Max output tokens per LLM response. |
| `LLM_CALL_TIMEOUT_SECONDS` | `120` | Per-LLM-call timeout. |
| `MCP_TOOL_TIMEOUT_SECONDS` | `60` | Per-MCP-tool-call timeout. |
| `OPENSENTRY_API_BASE` | `https://sentinel-command.fly.dev` | Command Center base URL. `OPENSENTRY_MCP_URL` is derived from it. |
| `SENTINEL_AGENT_KEY` | required | Run-lifecycle callback secret. Must match CC. |
| `OPENSENTRY_MCP_AGENT_KEY` | `SENTINEL_AGENT_MCP_KEY`, then `SENTINEL_AGENT_KEY` | Multi-tenant MCP bearer. Self-hosted: leave unset. |
| `OPENSENTRY_MCP_URL` | derived from `OPENSENTRY_API_BASE` + `/mcp` | Override only if MCP lives elsewhere. |
| `WEBHOOK_VERIFY_SIGNATURE` | `true` | Hard-disable HMAC for local dev. Always on in prod. |
| `SENTRY_DSN` | unset | Error tracking. No-op when unset. |
| `SENTRY_ENVIRONMENT` | `production` | Keeps local runs out of prod. |
| `SENTRY_TRACES_SAMPLE_RATE` | `0.1` | Performance sampling. |

## Project structure

```
backend/app/sentinel_agent/
  __main__.py        Entry point: python -m app.sentinel_agent
  main.py            Starlette app, /wakeup HMAC handler, /health, dev /
  processor.py       Drain loop: list pending → start → run → complete
  agent.py           LLM ↔ MCP tool conversation for one run
  prompts.py         Trigger-specific system prompts (motion / manual / …)
  llm.py             LiteLLM wrapper + ALL provider-shaped message translation
  mcp_client.py      MCP client (streamable HTTP), tool dispatch
  sentinel_client.py HTTP client back to Command Center
  config.py          Pydantic Settings from env
```

`llm.py` owns every provider-shaped detail on purpose. Ollama and OpenAI-shaped APIs disagree structurally, not cosmetically — tool results key off `tool_call_id` rather than `tool_name`, arguments arrive as a JSON *string* rather than a dict, and images cannot ride on a `tool` message at all (they need a `user` message with a `data:` URI). The agent loop builds messages only through `llm.py`'s helpers, so changing providers again touches one file. `backend/tests/test_agent_llm_provider.py` pins that translation, because every failure mode in it is silent: a wrongly-shaped message doesn't raise, it just means the model never saw the camera frame.

## Multi-tenancy model

ONE deployed agent serves every org. Per-call scoping happens at the MCP layer via `X-Agent-Org-Override` — the processor sets it to each run's `org_id` before connecting, and the server treats that as authoritative (rejecting if the org isn't entitled or has Sentinel disabled).

Per-run isolation comes from how the loop is built, not from per-org credentials:

- Fresh `MCPClientManager` per run
- Fresh `messages` array per run (`Agent.run` is stateless)
- Per-run override header from `run.org_id`
- Activity log writes one row per tool call, attributed to the override org

Trust depends on which key the agent holds. The **first-party** deployment uses two shared secrets and has cross-org access — compromise means act-as-any-org. A **self-hosted** agent holds an `osa_` key scoped to one org by its database row, so compromise is bounded and revocable from the UI. Mitigations: Fly secrets only, audit log per call, per-org MCP rate limiting, rotate by changing both sides.

## Contract with Command Center

`sentinel_client.py` hand-builds the JSON that CC's `RunCompleteBody` validates. `backend/tests/test_agent_contract.py` pins the two together — worth having because the failure is silent: pydantic ignores unknown keys, so renaming a field on the CC side would make every run record zero tool calls with no 422, no exception, and no log line.

## Limitations

- **No retry of failed webhook delivery.** If the wakeup POST fails, the run sits pending until the next trigger fires another wakeup, which also drains the prior one. Much less likely now the agent machine stays warm, but there is still no dead-letter queue. Stranded runs (claimed but never completed) *are* reaped.
- **No cron sweeps.** `trigger_type=scheduled` is mapped in the prompt set but CC doesn't emit them yet.
- **Sequential per wakeup.** Multiple pending runs process one after another inside a single wakeup.
- **Concurrent wakeups can claim the same run.** Two deliveries can both call `/start` on the same row. CC's `start` is idempotent so it isn't a data-integrity issue — just wasted tool calls and a small duplicate-incident risk. An `asyncio.Lock` around `process_with_timeout` is the fix when concurrent wakeups become real.
