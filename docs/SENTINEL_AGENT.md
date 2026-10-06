# Sentinel AI agent

The AI security agent that investigates motion events and files incidents. It is the `sentinel-agent` binary, built from `backend-rs/src/agent/` in the same crate as Command Center, and runs as the **`agent` process group** of the `sentinel-command` Fly app — its own machine, the same image, one deploy.

It used to be a separate repository (`SourceBox-Sentinel`, archived 2026-09-09) deploying to a separate Fly app (`sourcebox-sentinel`, destroyed the same day). If you find a reference to either, it is stale.

It was Python until the rewrite (`backend/app/sentinel_agent/`, with LiteLLM and the Python MCP SDK). The port was verified by running both agents against one Command Center, one database and one scripted model on each of the three provider wires — see [What the port changed](#what-the-port-changed) for the four places it deliberately differs.

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

- **LLM**: Ollama, Anthropic, or anything that speaks OpenAI's Chat Completions, via [rig](https://github.com/0xPlaygrounds/rig). `LLM_MODEL` is the only setting that changes — `ollama_chat/qwen3.5:cloud` today, `anthropic/claude-sonnet-5` for a bring-your-own-key deployment, or `openai/<model>` with `LLM_API_BASE` for a custom or self-hosted endpoint (vLLM, llama.cpp). **This is narrower than it was**: LiteLLM accepted a hundred provider prefixes and the agent now speaks three wires. Any other prefix is refused at startup with the supported list, rather than failing on the first run. **The model must support tool calling *and* image input**: the agent fans out tool calls and feeds camera frames back in, so a text-only model does not degrade, it fails every run.
- **Tools**: Command Center's MCP server — cameras, incidents, evidence — over streamable HTTP. Inventory in `../AGENTS.md` → MCP Server.
- **Server**: axum. No auth on its own state — every accepted request is HMAC-verified against the shared `SENTINEL_AGENT_KEY`.
- **Master of pending work**: Command Center's `sentinel_runs` table. The agent persists nothing; every wakeup re-fetches what's pending.

### Why a separate process group, not a thread

A run holds base64 camera frames for the length of its wall-clock budget, and the web machine's segment cache is already budgeted most of that machine's memory (see the `[env]` comment in `fly.toml`). Sharing one machine is how the OOM killer takes every org's live streams down at once.

Being a separate *app* was never what bought that isolation — a separate process group is.

### Why the machine stays warm

`min_machines_running = 1`, deliberately, even though this worker's shape screams scale-to-zero.

The reason was boot time, and that reason is gone. The Python agent took ~10s to bind its port (the interpreter, the MCP SDK, Sentry and a deferred LiteLLM import), which is longer than Fly's proxy waits for a machine it auto-started, so every wakeup against a stopped machine came back `RemoteDisconnected`. The binary answers `/health` about 40 ms after it is started (measured on a laptop, release build).

It has **not** been switched to scale-to-zero anyway. What failed last time was a race against a proxy limit that is only observable in production, and nothing here has been measured there. The `[[services]]` comment in `fly.toml` says how to try it. Until then, one small machine warm is about $2/month. How the sibling services compare against the proxy's budget is in [ARCHITECTURE.md](ARCHITECTURE.md#deployed-services-flyio).

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
OPENSENTRY_API_BASE=https://your-command-center
sentinel-agent
```

The binary is in the Command Center image (`docker run --env-file agent.env <image> sentinel-agent`) or built with `cargo build --release --bin sentinel-agent` in `backend-rs/`. It reads a `.env` in its working directory, underneath the real environment.

`OPENSENTRY_MCP_AGENT_KEY` defaults to `SENTINEL_AGENT_KEY` when unset, because the issued key authenticates both the run queue and the camera tools.

> Do **not** use the shared `SENTINEL_AGENT_KEY` from the first-party deployment. It is a multi-tenant secret that can act as *any* org via `X-Agent-Org-Override`, and must never be handed to a customer. The `osa_` keys exist precisely so it doesn't have to be.

## Plan tiers

Gated end-to-end (UI, dispatcher, agent MCP auth) on the org's plan. Caps reset on the 1st of each calendar month, UTC. The numbers live in `backend-rs/src/api/sentinel_config.rs::cap_for_plan`.

| Plan | Monthly runs | Note |
| -------- | ------------ | ----------------------------------------- |
| Free | 0 | Sentinel locked; UI shows upgrade banner. |
| Pro | 100 | ~3 / day — casual home use. |
| Pro Plus | 500 | ~16 / day — commercial-shaped use. |
| Self-hosted | 500 | Needs a licence key (`SENTINEL_LICENSE_KEY`). |

At the cap, dispatch pauses for the rest of the month. No overage billing; recordings, motion notifications, dashboard and MCP keep working.

## Reliability layers

A run is bounded at every layer. All bounds are env-tunable.

- **Per-LLM-call timeout** — 120s, around the whole call rather than the HTTP request: a provider that accepts the connection then stalls mid-stream would otherwise hold the machine until the wall clock fires.
- **Per-MCP-tool timeout** — 60s. Stuck tools surface to the LLM as an error result so the model can retry or pivot.
- **Iteration cap** — 10 tool-call rounds per run (`MAX_AGENT_ITERATIONS`). Hitting it → outcome `error`, "investigation incomplete".
- **Wall-clock cap** — 270s per wakeup (`processor.rs::DRAIN_TIMEOUT_SECONDS`). When it fires, the agent finds the in-flight run and posts `complete` with `outcome=error`, so the run ends cleanly instead of sitting in `running`. The figure was chosen under the old standalone app's 300s `kill_timeout`; this app sets none, and the `agent` machine never idles to a stop, so today only a deploy or restart cuts a run short, and the reaper below settles it.
- **CC-side stranded-run reaper** — runs stuck in `running` for >20 minutes are marked `error` by Command Center. Catches the case where the agent crashes before its own cleanup fires.

## Endpoints

| Method | Path | Description |
| ------ | --------- | ------------------------------------------------------------------ |
| `GET` | `/health` | Liveness probe. No auth. |
| `POST` | `/wakeup` | Webhook receiver. Drains pending runs. HMAC-verified. |
| `POST` | `/` | DEV-ONLY drain trigger. Disabled in prod via `WEBHOOK_VERIFY_SIGNATURE=true`. |

## Running it locally

```bash
cd backend-rs

OLLAMA_API_KEY=... \
SENTINEL_AGENT_KEY=dev-secret \
AGENT_MODE=poll \
OPENSENTRY_API_BASE=http://localhost:8000 \
WEBHOOK_VERIFY_SIGNATURE=false \
  cargo run --bin sentinel-agent
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
fly secrets set SENTINEL_AGENT_KEY=$(openssl rand -hex 32) -a sentinel-command
fly secrets set SENTINEL_AGENT_MCP_KEY=$(openssl rand -hex 32) -a sentinel-command
fly secrets set SENTINEL_AGENT_WEBHOOK_URL=http://sentinel-command.flycast:8080/wakeup -a sentinel-command
```

`SENTINEL_AGENT_WEBHOOK_URL` points at the app's own `agent` process group over 6PN. That requires a private IPv6 (`fly ips allocate-v6 --private`) — without it `.flycast` does not resolve and every wakeup fails to connect.

The agent reads Command Center's `SENTINEL_AGENT_MCP_KEY` directly when `OPENSENTRY_MCP_AGENT_KEY` is unset, so the first-party deployment needs no duplicate copy of it under the agent's own name.

Confirm:

```bash
fly ssh console -a sentinel-command -s --process-group agent -C "curl -s localhost:8080/health"
```

## Configuration

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `AGENT_MODE` | `push` | `push` = wait for `/wakeup`. `poll` = ask CC on an interval (works behind NAT). |
| `POLL_INTERVAL_SECONDS` | `30` | Poll mode only. |
| `LLM_MODEL` | falls back to `ollama_chat/{OLLAMA_MODEL}` | `ollama_chat/<model>`, `anthropic/<model>` or `openai/<model>`; a bare `claude-…` or `gpt-…` is inferred. Any other prefix fails at startup. Must support tool calling + image input. |
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
| `OPENSENTRY_MCP_URL` | derived from `OPENSENTRY_API_BASE` + `/mcp/` | Override only if MCP lives elsewhere. The trailing slash matters and is added if missing: Command Center answers `/mcp` with a 307 that the MCP client does not follow. |
| `AGENT_HOST` / `PORT` | `0.0.0.0` / `8080` | Where the agent's own server binds. |
| `WEBHOOK_VERIFY_SIGNATURE` | `true` | Hard-disable HMAC for local dev. Always on in prod. |
| `SENTRY_DSN` | unset | Error tracking. No-op when unset. |
| `SENTRY_ENVIRONMENT` | `production` | Set it (for example, to `development`) on a local run, or its errors are reported as production's. |
| `SENTRY_TRACES_SAMPLE_RATE` | `0.1` | Performance sampling. |

## Project structure

```
backend-rs/src/agent/
  config.rs          Environment + .env, validated at startup
  server.rs          axum app, /wakeup HMAC handler, /health, dev /, poll loop
  processor.rs       Drain loop: list pending → start → run → complete
  run.rs             LLM ↔ MCP tool conversation for one run
  prompts.rs         Trigger-specific system prompts (text in assets/agent/)
  llm.rs             rig wrapper + ALL provider-shaped message handling
  mcp_client.rs      MCP client (rmcp, streamable HTTP), tool dispatch
  queue.rs           HTTP client back to Command Center
backend-rs/src/bin/agent.rs      Entry point: sentinel-agent
```

`llm.rs` owns every provider-shaped detail on purpose. Ollama and OpenAI-shaped APIs disagree structurally, not cosmetically — tool results key off `tool_call_id` rather than `tool_name`, arguments arrive as a JSON *string* rather than an object, and images cannot ride on a `tool` message at all (they need a `user` message with a `data:` URI). The loop builds messages only through `llm.rs`'s helpers, so changing providers again touches one file. Every failure mode in that translation is silent: a wrongly-shaped message doesn't raise, it just means the model never saw the camera frame — which is why it is checked on the wire, by `tests/differential/agent_run.sh`, and not only by unit tests.

## What the port changed

Checked with `backend-rs/tests/differential/agent_run.sh`: 20 scenarios on the Ollama wire, 15 on OpenAI, 12 on Anthropic, comparing what the model is sent, the run rows, and the incidents and evidence filed. Identical, except for four decisions the harness names rather than hides:

- **Frames follow a turn's tool results instead of interleaving with them.** The Python put a `user` message between two tool results of one assistant turn, which OpenAI and Anthropic do not accept (`PYTHON_BUGS.md` #19).
- **A provider 5xx is one attempt on every wire.** Under LiteLLM the OpenAI SDK retried it by default and Ollama — the production wire — never did. The run ends `error`; nothing retries it.
- **Unparseable tool arguments are replayed to the model as `{}`.** Both agents call the tool with no arguments; the Python then showed the model its own broken string again.
- **Fewer providers.** Three wires, not LiteLLM's catalogue. See Architecture.

One thing the port surfaced about the Python: under the `mcp` 2.x the lockfile had resolved to, it could not open an MCP connection at all (`PYTHON_BUGS.md` #18).

## Multi-tenancy model

ONE deployed agent serves every org. Per-call scoping happens at the MCP layer via `X-Agent-Org-Override` — the processor sets it to each run's `org_id` before connecting, and the server treats that as authoritative (rejecting if the org isn't entitled or has Sentinel disabled).

Per-run isolation comes from how the loop is built, not from per-org credentials:

- Fresh `McpClient` per run, torn down when it ends
- Fresh `messages` array per run (`run_agent` holds no state)
- Per-run override header from `run.org_id`
- Activity log writes one row per tool call, attributed to the override org

Trust depends on which key the agent holds. The **first-party** deployment uses two shared secrets and has cross-org access — compromise means act-as-any-org. A **self-hosted** agent holds an `osa_` key scoped to one org by its database row, so compromise is bounded and revocable from the UI. Mitigations: Fly secrets only, audit log per call, per-org MCP rate limiting, rotate by changing both sides.

## Contract with Command Center

`agent/queue.rs::complete_body` builds the JSON that `post_run_complete` reads. `backend-rs/tests/agent_contract.rs` pins the two together — worth having because the failure is silent: the handler ignores unknown keys, so renaming a field on either side would make every run record zero tool calls with no 422, no error, and no log line.

## Limitations

- **No retry of failed webhook delivery.** If the wakeup POST fails, the run sits pending until the next trigger fires another wakeup, which also drains the prior one. Much less likely now the agent machine stays warm, but there is still no dead-letter queue. Stranded runs (claimed but never completed) *are* reaped.
- **No cron sweeps.** `trigger_type=scheduled` is mapped in the prompt set but CC doesn't emit them yet.
- **Sequential per wakeup.** Multiple pending runs process one after another inside a single wakeup.
- **One drain at a time, per process.** A wakeup that arrives while a drain is running is answered immediately and does not start a second one; a run queued after that drain listed waits for the next wakeup or poll, exactly as a failed delivery does. Two agent *machines* would still both be able to claim a run — `/start` is idempotent, so that is wasted tool calls rather than corruption — which is one reason there is one.
