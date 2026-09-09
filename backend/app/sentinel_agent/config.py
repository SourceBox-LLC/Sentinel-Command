"""
SourceBox Sentinel — runtime configuration.

The agent is a serverless Fly.io app that wakes on incoming webhook
from Command Center, processes pending sentinel_runs, and idles back
to sleep. All knobs come from env vars; defaults are dev-friendly.

Multi-tenant: ONE deployed agent serves every org. Per-call org
scoping happens at the MCP layer via the ``X-Agent-Org-Override``
header. The agent doesn't hold per-org credentials; it holds a
single shared MCP secret (``OPENSENTRY_MCP_AGENT_KEY``) and tells
the MCP server which org each tool call is on behalf of.
"""

from pydantic import AliasChoices, Field, field_validator, model_validator
from pydantic_settings import BaseSettings


class Settings(BaseSettings):
    # ── LLM (via LiteLLM) ────────────────────────────────────────────
    # LLM_MODEL is a LiteLLM model string and is the ONLY setting needed
    # to change provider:
    #
    #   ollama_chat/qwen3.5:cloud        Ollama Cloud (today's default)
    #   anthropic/claude-sonnet-5        bring-your-own-key
    #   openai/gpt-5.1
    #   openai/sentinel-1                a future SourceBox model behind
    #                                    an OpenAI-compatible endpoint
    #
    # Leave it unset and the OLLAMA_* settings below are used instead —
    # which is what keeps every existing deployment working untouched.
    #
    # Whatever you point this at MUST support tool calling and image
    # input. The agent loop is built on both: it fans out tool calls and
    # feeds camera frames back in. A text-only model does not degrade
    # here, it fails every run.
    llm_model: str = ""
    llm_api_key: str = ""
    llm_api_base: str = ""

    # ── LLM (Ollama Cloud) — the default provider ────────────────────
    # No longer required: a deployment can configure LLM_* instead. The
    # validator below still refuses to boot with no credential at all,
    # which is what this field's missing-by-default used to enforce.
    ollama_api_key: str = ""
    ollama_host: str = "https://ollama.com"
    # Vision-capable + tool-calling.  qwen3.5:cloud has 256K context,
    # which matters for the multi-turn tool-use loop where the
    # messages array grows as the agent calls list_cameras → view_camera
    # → create_incident etc.  Don't switch to a text-only model here:
    # view_camera returns a JPEG and the LLM has to ingest it.
    # Override via OLLAMA_MODEL env var to test other vision variants
    # (gemini-3-flash-preview:cloud, kimi-k2.6:cloud, etc.).
    ollama_model: str = "qwen3.5:cloud"
    max_tokens: int = 2048

    # Hard cap on tool-call iterations per run.  10 is generous for
    # the surveillance flow (list / view / create / attach / observe /
    # finalize is 6); raises bound on runaway loops.
    max_agent_iterations: int = 10

    # Per-call timeouts.  Without these, a hung Ollama or MCP call
    # holds the Fly machine alive (= billing) until the wall-clock
    # timeout in process_with_timeout fires — at which point the
    # in-flight run is stranded in `running` state on CC.  Both bounds
    # are intentionally generous — the LLM has to think over a JPEG
    # plus a tool catalogue, and watch_camera with multiple frames
    # legitimately takes 10-20 s.
    llm_call_timeout_seconds: float = 120.0
    mcp_tool_timeout_seconds: float = 60.0

    # ── Command Center (this is the master of pending runs) ─────────
    # Base URL of the Command Center API. Examples:
    #   https://sentinel-command.fly.dev     (production)
    #   http://host.docker.internal:8000     (local dev — Docker on Mac/Win)
    #
    # This was `opensentry-command.fly.dev` until 2026-09-07 — a leftover
    # from the OpenSentry -> Sentinel rename. That host no longer
    # resolves, and because opensentry_mcp_url is DERIVED from this value
    # (see below), a stale base takes the agent's entire toolset down with
    # it, not just the run fetch. Verified from the agent's own machine:
    # the old host fails with URLError, this one returns 200.
    opensentry_api_base: str = "https://sentinel-command.fly.dev"

    # Run-lifecycle callback secret.  Sent in X-Sentinel-Agent-Key
    # header on /api/sentinel/runs/* callbacks AND used as the HMAC
    # secret for verifying inbound /wakeup webhooks from CC.  Must
    # match SENTINEL_AGENT_KEY on the Command Center side.
    sentinel_agent_key: str = ""

    # ── MCP server ──────────────────────────────────────────────────
    # The bearer the agent presents as ``Authorization: Bearer <key>``,
    # alongside an ``X-Agent-Org-Override`` header naming the org each
    # call is on behalf of.
    #
    # First-party (multi-tenant) deployment: a single shared secret
    # serving every org, matching SENTINEL_AGENT_MCP_KEY on the Command
    # Center side. Set it explicitly — it differs from
    # sentinel_agent_key there.
    #
    # Self-hosted: leave this UNSET. It falls back to sentinel_agent_key
    # (see _default_mcp_key_to_agent_key below), because Command Center
    # issues one scoped ``osa_`` key that authenticates both surfaces.
    # The override header is still sent and is accepted as long as it
    # names the key's own org.
    opensentry_mcp_url: str = ""  # derived from opensentry_api_base if blank
    # Reads OPENSENTRY_MCP_AGENT_KEY first, then Command Center's own
    # SENTINEL_AGENT_MCP_KEY. The second alias exists because the agent
    # now runs as a process group of the sentinel-command app and shares
    # its environment — without it the hosted agent would need a
    # duplicate copy of a secret that is already right there, under a
    # different name.
    #
    # This matters more than tidiness: the first-party deployment uses
    # two genuinely DIFFERENT shared secrets (run-queue vs MCP), so the
    # self-hosted fallback below would quietly hand the MCP surface the
    # wrong key — an agent that fetches work and then fails every tool
    # call, which is exactly the failure that fallback was written to
    # prevent for self-hosters.
    opensentry_mcp_agent_key: str = Field(
        "",
        validation_alias=AliasChoices(
            "OPENSENTRY_MCP_AGENT_KEY", "SENTINEL_AGENT_MCP_KEY"
        ),
    )

    # ── Webhook signature behaviour ──────────────────────────────────
    # When False, /wakeup skips HMAC verification — useful for local
    # development with curl. ALWAYS true in production.
    webhook_verify_signature: bool = True

    # ── Error tracking (Sentry) ──────────────────────────────────────
    # With min_machines_running=0 the agent sleeps between wakeups, so
    # logger.exception output is ephemeral and nobody watches it — a
    # run that errors every time (expired Ollama key, model 404, MCP
    # key mismatch) is otherwise invisible.  Set SENTRY_DSN to surface
    # those.  No-ops when unset, so it's safe to deploy without it.
    sentry_dsn: str = ""
    sentry_traces_sample_rate: float = 0.1
    sentry_environment: str = "production"

    # ── Work discovery: push (webhook) or poll ──────────────────────
    # "push"  — Command Center calls POST /wakeup and the agent drains.
    #           Requires CC to reach the agent inbound, which only works
    #           when the agent is publicly addressable. This is how the
    #           SourceBox-hosted agent runs, and it is what makes Fly's
    #           auto-stop worthwhile: idle costs nothing.
    #
    # "poll"  — the agent asks CC for pending runs on an interval. No
    #           inbound connectivity needed, so it works from behind NAT
    #           on a home or office network. This is the mode for a
    #           self-hosted agent, and it mirrors how CameraNode already
    #           talks to Command Center: outbound only.
    #
    # /wakeup stays mounted in poll mode — it is still a valid way to
    # force an immediate drain — but nothing depends on it arriving.
    #
    # NOTE: do NOT set poll on the Fly deployment. Polling keeps the
    # machine awake, which defeats auto_stop_machines and bills you for
    # idle time. Poll is for deployments you run yourself.
    agent_mode: str = "push"

    # Only used when agent_mode == "poll". 30s keeps worst-case latency
    # from run-created to run-started at about the same order as the
    # webhook path, without hammering CC: one cheap GET per interval.
    poll_interval_seconds: float = 30.0

    # extra="ignore" means stale env vars from a prior version of this
    # config (or from the surrounding shell) don't break boot.  Pydantic's
    # default is "forbid", which would 500 on every wakeup the moment
    # someone left an old setting around.
    model_config = {"env_file": ".env", "extra": "ignore"}

    # ── Resolved LLM settings ────────────────────────────────────────
    # Explicit LLM_* wins; otherwise fall back to the OLLAMA_* trio so an
    # existing deployment keeps working with no env changes at all. This
    # is the whole backward-compatibility story for the LiteLLM move.

    @property
    def resolved_llm_model(self) -> str:
        if self.llm_model:
            return self.llm_model
        # `ollama_chat/` (not `ollama/`) — the chat endpoint is the one
        # that supports tool calling, which the agent loop requires.
        return f"ollama_chat/{self.ollama_model}"

    @property
    def resolved_llm_api_key(self) -> str:
        return self.llm_api_key or self.ollama_api_key

    @property
    def resolved_llm_api_base(self) -> str:
        if self.llm_api_base:
            return self.llm_api_base
        # Only meaningful for the Ollama fallback; a hosted provider
        # resolves its own endpoint from the model string.
        return self.ollama_host if not self.llm_model else ""

    @model_validator(mode="after")
    def _require_an_llm_credential(self):
        """Fail at boot, not mid-run, when no credential is configured.

        `ollama_api_key` used to be a required field, so a missing key
        was a startup ValidationError. Making it optional (so LLM_* can
        be used instead) would otherwise have turned that into a run
        that fetches work, calls the model, and errors — burning a
        wakeup and stranding the run.

        Skipped when api_base points somewhere local: a self-hosted
        Ollama on the same box legitimately needs no key.
        """
        if self.resolved_llm_api_key:
            return self
        base = self.resolved_llm_api_base
        if any(h in base for h in ("localhost", "127.0.0.1", "host.docker.internal")):
            return self
        raise ValueError(
            "No LLM credential configured — set LLM_API_KEY (or "
            "OLLAMA_API_KEY). The agent cannot run without one."
        )

    @model_validator(mode="after")
    def _default_mcp_key_to_agent_key(self):
        """Let one key configure a self-hosted agent.

        Command Center issues a single scoped ``osa_`` key that
        authenticates BOTH the run-queue API and the MCP tool surface,
        so a self-hoster would otherwise have to paste the identical
        value into two variables — which reads like a documentation bug
        and invites setting only one, producing an agent that fetches
        work and then fails every tool call.

        The fallback runs in this direction only. Doing the reverse
        (defaulting the run-queue key from the MCP key) would be wrong
        for the first-party deployment, which genuinely uses two
        DIFFERENT shared secrets — silently crossing them would be a
        confusing auth failure at best.
        """
        if not self.opensentry_mcp_agent_key and self.sentinel_agent_key:
            self.opensentry_mcp_agent_key = self.sentinel_agent_key
        return self

    @field_validator("agent_mode")
    @classmethod
    def _validate_agent_mode(cls, v: str) -> str:
        """Fail at startup on a typo rather than silently defaulting.

        A misspelled mode would otherwise mean the agent neither polls
        nor is reachable, and does nothing at all — with no error to
        explain why runs are piling up on Command Center.
        """
        mode = v.strip().lower()
        if mode not in {"push", "poll"}:
            raise ValueError(f"agent_mode must be 'push' or 'poll', got {v!r}")
        return mode

    @property
    def mcp_url(self) -> str:
        """Resolved MCP endpoint URL.

        Honours an explicit override (opensentry_mcp_url) when set,
        otherwise constructs ``{api_base}/mcp`` to keep the env minimal.
        """
        if self.opensentry_mcp_url:
            return self.opensentry_mcp_url
        return self.opensentry_api_base.rstrip("/") + "/mcp"
