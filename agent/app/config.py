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

from pydantic import field_validator, model_validator
from pydantic_settings import BaseSettings


class Settings(BaseSettings):
    # ── LLM (Ollama Cloud) ───────────────────────────────────────────
    ollama_api_key: str
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
    opensentry_mcp_agent_key: str = ""

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
