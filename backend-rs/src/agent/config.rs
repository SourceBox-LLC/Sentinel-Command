//! The agent's runtime configuration.
//!
//! Ported from `app/sentinel_agent/config.py`, a pydantic-settings class.
//! Three of its behaviours are the port, rather than the field list:
//!
//! * **It fails at boot, not mid-run.** No LLM credential, an unknown
//!   `AGENT_MODE`, an unparseable number — each was a startup
//!   `ValidationError`. A run that fetches work, claims it and then
//!   discovers there is no key burns a wakeup and strands the run in
//!   `running` until the reaper finds it twenty minutes later.
//! * **One key configures a self-hosted agent.** Command Center issues a
//!   single scoped `osa_` key that authenticates both the run queue and
//!   the MCP surface, so the MCP key defaults to the agent key — in that
//!   direction only. The first-party deployment uses two genuinely
//!   different shared secrets and crossing them the other way would be a
//!   confusing auth failure at best.
//! * **`.env` in the working directory is read**, under the real
//!   environment. pydantic-settings did this through `env_file`, and a
//!   self-hoster following `docs/SENTINEL_AGENT.md` has one.
//!
//! The source is injected (`from_lookup`) rather than read from
//! `std::env` inside, so the validation rules are testable without
//! mutating a process-wide environment that other tests are reading.

use std::collections::HashMap;

/// How the agent discovers work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    /// Command Center calls `POST /wakeup`. Needs inbound reachability,
    /// and is what makes a sleeping machine cost nothing.
    Push,
    /// The agent asks on an interval. Outbound only, so it works from
    /// behind NAT — the shape CameraNode already uses.
    Poll,
}

/// Which wire the configured model speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// Ollama's native `/api/chat`.
    Ollama,
    /// Anthropic's Messages API.
    Anthropic,
    /// OpenAI Chat Completions, and anything that imitates it.
    OpenAi,
}

/// A LiteLLM-style model string, taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider: Provider,
    /// The model name as the provider knows it, prefix removed.
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub llm_model: String,
    pub llm_api_key: String,
    pub llm_api_base: String,
    pub ollama_api_key: String,
    pub ollama_host: String,
    pub ollama_model: String,
    pub max_tokens: u64,
    pub max_agent_iterations: usize,
    pub llm_call_timeout_seconds: f64,
    pub mcp_tool_timeout_seconds: f64,
    pub opensentry_api_base: String,
    pub sentinel_agent_key: String,
    pub opensentry_mcp_url: String,
    pub opensentry_mcp_agent_key: String,
    pub webhook_verify_signature: bool,
    pub agent_mode: AgentMode,
    pub poll_interval_seconds: f64,
    pub host: String,
    pub port: u16,
}

impl AgentConfig {
    /// From the process environment, with `.env` in the working
    /// directory underneath it.
    pub fn from_env() -> Result<Self, String> {
        let file = std::fs::read_to_string(".env")
            .map(|text| parse_dotenv(&text))
            .unwrap_or_default();
        Self::from_lookup(|key| std::env::var(key).ok().or_else(|| file.get(key).cloned()))
    }

    /// From any key → value source. pydantic-settings matches field
    /// names case-insensitively; every documented spelling is upper-case
    /// and that is what is looked up.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let text = |key: &str, default: &str| get(key).unwrap_or_else(|| default.to_string());

        let agent_mode = {
            let raw = text("AGENT_MODE", "push");
            match raw.trim().to_lowercase().as_str() {
                "push" => AgentMode::Push,
                "poll" => AgentMode::Poll,
                // A typo would otherwise mean the agent neither polls nor
                // is reachable, and does nothing at all — with no error
                // to explain why runs are piling up on Command Center.
                _ => return Err(format!("agent_mode must be 'push' or 'poll', got {raw:?}")),
            }
        };

        let sentinel_agent_key = text("SENTINEL_AGENT_KEY", "");
        // Two spellings, first one wins: the agent's own name, then
        // Command Center's. The second exists because the hosted agent
        // shares the web tier's environment and the secret is already
        // there under that name.
        let mut opensentry_mcp_agent_key = get("OPENSENTRY_MCP_AGENT_KEY")
            .or_else(|| get("SENTINEL_AGENT_MCP_KEY"))
            .unwrap_or_default();
        if opensentry_mcp_agent_key.is_empty() && !sentinel_agent_key.is_empty() {
            opensentry_mcp_agent_key = sentinel_agent_key.clone();
        }

        let config = Self {
            llm_model: text("LLM_MODEL", ""),
            llm_api_key: text("LLM_API_KEY", ""),
            llm_api_base: text("LLM_API_BASE", ""),
            ollama_api_key: text("OLLAMA_API_KEY", ""),
            ollama_host: text("OLLAMA_HOST", "https://ollama.com"),
            ollama_model: text("OLLAMA_MODEL", "qwen3.5:cloud"),
            max_tokens: number(&get, "MAX_TOKENS", 2048)?,
            max_agent_iterations: number(&get, "MAX_AGENT_ITERATIONS", 10)?,
            llm_call_timeout_seconds: number(&get, "LLM_CALL_TIMEOUT_SECONDS", 120.0)?,
            mcp_tool_timeout_seconds: number(&get, "MCP_TOOL_TIMEOUT_SECONDS", 60.0)?,
            // `opensentry-command.fly.dev` until 2026-09-07 — a leftover
            // from the rename, and the MCP URL is DERIVED from this, so a
            // stale base takes the whole toolset down with it.
            opensentry_api_base: text("OPENSENTRY_API_BASE", "https://sentinel-command.fly.dev"),
            sentinel_agent_key,
            opensentry_mcp_url: text("OPENSENTRY_MCP_URL", ""),
            opensentry_mcp_agent_key,
            webhook_verify_signature: boolean(&get, "WEBHOOK_VERIFY_SIGNATURE", true)?,
            agent_mode,
            poll_interval_seconds: number(&get, "POLL_INTERVAL_SECONDS", 30.0)?,
            host: text("AGENT_HOST", "0.0.0.0"),
            port: number(&get, "PORT", 8080)?,
        };

        // No credential at all is a boot failure — unless the endpoint
        // is local, because a self-hosted Ollama on the same box
        // legitimately needs no key.
        if config.resolved_llm_api_key().is_empty() {
            let base = config.resolved_llm_api_base();
            let local = ["localhost", "127.0.0.1", "host.docker.internal"]
                .iter()
                .any(|host| base.contains(host));
            if !local {
                return Err("No LLM credential configured — set LLM_API_KEY (or \
                            OLLAMA_API_KEY). The agent cannot run without one."
                    .to_string());
            }
        }
        // And the model string has to name a wire this build speaks, for
        // the same reason: better a sentence now than a failed run.
        config.model_ref()?;

        // Fail closed on the one-flag footgun: with verification off,
        // `/wakeup` accepts anyone and the dev `/` route opens —
        // anonymous LLM spend, plus cross-org drain summaries. A
        // configured production key and verification off is a
        // contradiction, and it refuses to start rather than run open.
        if !config.sentinel_agent_key.is_empty() && !config.webhook_verify_signature {
            return Err(
                "SENTINEL_AGENT_KEY is configured but WEBHOOK_VERIFY_SIGNATURE is \
                 off — refusing to start with an unauthenticated drain surface. \
                 Enable verification (production) or unset the key (local dev)."
                    .to_string(),
            );
        }
        Ok(config)
    }

    /// Explicit `LLM_MODEL` wins; otherwise the `OLLAMA_*` trio, which is
    /// what keeps a deployment that predates `LLM_*` working untouched.
    pub fn resolved_llm_model(&self) -> String {
        if !self.llm_model.is_empty() {
            return self.llm_model.clone();
        }
        // `ollama_chat/`, not `ollama/`: the chat endpoint is the one
        // that supports tool calling, which the whole loop is built on.
        format!("ollama_chat/{}", self.ollama_model)
    }

    pub fn resolved_llm_api_key(&self) -> String {
        if !self.llm_api_key.is_empty() {
            self.llm_api_key.clone()
        } else {
            self.ollama_api_key.clone()
        }
    }

    pub fn resolved_llm_api_base(&self) -> String {
        if !self.llm_api_base.is_empty() {
            return self.llm_api_base.clone();
        }
        // Only meaningful for the Ollama fallback; a hosted provider
        // resolves its own endpoint from the model string.
        if self.llm_model.is_empty() {
            self.ollama_host.clone()
        } else {
            String::new()
        }
    }

    /// The MCP endpoint: an explicit override, else `{api_base}/mcp/`.
    ///
    /// **With the trailing slash, always.** Command Center serves the
    /// protocol at `/mcp/` and answers `POST /mcp` with a 307 to it —
    /// Starlette's mount redirect, reproduced for clients that follow
    /// it. The Python agent pointed at `/mcp` and its SDK followed the
    /// redirect; rmcp's client does not ("unexpected server response:
    /// HTTP 307"), which the first end-to-end run found on its very first
    /// request. Going to the real path also saves a round trip per call.
    ///
    /// An explicit override ending in `/mcp` gets the slash too, because
    /// that is exactly what a self-hoster copying the old default would
    /// have set, and it would fail in a way that reads like an auth bug.
    pub fn mcp_url(&self) -> String {
        let url = if self.opensentry_mcp_url.is_empty() {
            format!("{}/mcp/", self.opensentry_api_base.trim_end_matches('/'))
        } else {
            self.opensentry_mcp_url.clone()
        };
        if url.ends_with("/mcp") {
            format!("{url}/")
        } else {
            url
        }
    }

    /// The configured model, as a provider and a bare name.
    ///
    /// LiteLLM accepted hundreds of prefixes. This accepts the ones the
    /// agent's own documentation names — `ollama_chat/`, `anthropic/`,
    /// `openai/` — plus `ollama/` and the two unprefixed families LiteLLM
    /// infers, and REFUSES the rest by name. Silently routing an unknown
    /// prefix to some default wire would produce a run that fails on its
    /// first model call with a provider's 404, which says nothing about
    /// the configuration being the problem.
    pub fn model_ref(&self) -> Result<ModelRef, String> {
        parse_model(&self.resolved_llm_model())
    }
}

/// Split a LiteLLM-style model string.
pub fn parse_model(spec: &str) -> Result<ModelRef, String> {
    let spec = spec.trim();
    if let Some((prefix, model)) = spec.split_once('/') {
        let provider = match prefix {
            "ollama_chat" | "ollama" => Some(Provider::Ollama),
            "anthropic" => Some(Provider::Anthropic),
            "openai" => Some(Provider::OpenAi),
            _ => None,
        };
        if let Some(provider) = provider {
            if model.is_empty() {
                return Err(format!("LLM_MODEL {spec:?} names a provider and no model"));
            }
            return Ok(ModelRef {
                provider,
                model: model.to_string(),
            });
        }
        return Err(format!(
            "LLM_MODEL {spec:?} uses a provider prefix this agent does not speak. \
             Supported: ollama_chat/<model>, anthropic/<model>, openai/<model> — \
             and openai/<model> with LLM_API_BASE reaches any OpenAI-compatible \
             endpoint."
        ));
    }
    // Unprefixed, the way LiteLLM infers it for the two big families.
    if spec.starts_with("claude-") {
        return Ok(ModelRef {
            provider: Provider::Anthropic,
            model: spec.to_string(),
        });
    }
    if spec.starts_with("gpt-")
        || spec.starts_with("o1")
        || spec.starts_with("o3")
        || spec.starts_with("o4")
    {
        return Ok(ModelRef {
            provider: Provider::OpenAi,
            model: spec.to_string(),
        });
    }
    Err(format!(
        "LLM_MODEL {spec:?} has no provider prefix. Use ollama_chat/<model>, \
         anthropic/<model> or openai/<model>."
    ))
}

/// A number, or a sentence naming the variable that was not one.
fn number<T: std::str::FromStr>(
    get: &impl Fn(&str) -> Option<String>,
    key: &str,
    default: T,
) -> Result<T, String> {
    match get(key) {
        None => Ok(default),
        Some(raw) => raw
            .trim()
            .parse::<T>()
            .map_err(|_| format!("{key} must be a number, got {raw:?}")),
    }
}

/// pydantic's boolean coercion: the eight spellings it accepts, and an
/// error for anything else rather than a guess.
fn boolean(
    get: &impl Fn(&str) -> Option<String>,
    key: &str,
    default: bool,
) -> Result<bool, String> {
    match get(key) {
        None => Ok(default),
        Some(raw) => match raw.trim().to_lowercase().as_str() {
            "1" | "true" | "t" | "yes" | "y" | "on" => Ok(true),
            "0" | "false" | "f" | "no" | "n" | "off" => Ok(false),
            _ => Err(format!("{key} must be a boolean, got {raw:?}")),
        },
    }
}

/// `KEY=VALUE` lines, the way python-dotenv reads the common cases:
/// comments, blank lines, an optional `export`, and one layer of matching
/// quotes. Deliberately not a shell — no interpolation — which is also
/// why a `$` in an argon2 hash survives it.
pub fn parse_dotenv(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, mut value) = (key.trim(), value.trim());
        for quote in ['"', '\''] {
            if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
                value = &value[1..value.len() - 1];
                break;
            }
        }
        if !key.is_empty() {
            out.insert(key.to_string(), value.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> Result<AgentConfig, String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        AgentConfig::from_lookup(|key| map.get(key).cloned())
    }

    /// The backward-compatibility story: `OLLAMA_*` alone still works,
    /// and resolves to the chat endpoint's prefix.
    #[test]
    fn the_ollama_trio_is_the_default_provider() {
        let c = config(&[("OLLAMA_API_KEY", "k")]).unwrap();
        assert_eq!(c.resolved_llm_model(), "ollama_chat/qwen3.5:cloud");
        assert_eq!(c.resolved_llm_api_key(), "k");
        assert_eq!(c.resolved_llm_api_base(), "https://ollama.com");
        assert_eq!(
            c.model_ref().unwrap(),
            ModelRef {
                provider: Provider::Ollama,
                model: "qwen3.5:cloud".into()
            }
        );
    }

    /// `LLM_*` wins, and a hosted provider gets NO base from the Ollama
    /// fallback — pointing Anthropic's client at ollama.com would be the
    /// result otherwise.
    #[test]
    fn llm_settings_override_and_do_not_inherit_the_ollama_host() {
        let c = config(&[
            ("LLM_MODEL", "anthropic/claude-sonnet-5"),
            ("LLM_API_KEY", "sk"),
            ("OLLAMA_API_KEY", "old"),
        ])
        .unwrap();
        assert_eq!(c.resolved_llm_api_key(), "sk");
        assert_eq!(c.resolved_llm_api_base(), "");
        assert_eq!(c.model_ref().unwrap().provider, Provider::Anthropic);
    }

    #[test]
    fn no_credential_is_a_boot_failure_unless_the_endpoint_is_local() {
        let err = config(&[]).unwrap_err();
        assert!(err.contains("No LLM credential configured"), "{err}");
        // A local Ollama needs no key.
        for host in [
            "http://localhost:11434",
            "http://127.0.0.1:11434",
            "http://host.docker.internal:11434",
        ] {
            assert!(config(&[("OLLAMA_HOST", host)]).is_ok(), "{host}");
        }
    }

    /// One key configures a self-hosted agent — in one direction only.
    #[test]
    fn the_mcp_key_defaults_to_the_agent_key_and_not_the_reverse() {
        let c = config(&[("OLLAMA_API_KEY", "k"), ("SENTINEL_AGENT_KEY", "osa_one")]).unwrap();
        assert_eq!(c.opensentry_mcp_agent_key, "osa_one");

        let c = config(&[
            ("OLLAMA_API_KEY", "k"),
            ("SENTINEL_AGENT_MCP_KEY", "mcp-secret"),
        ])
        .unwrap();
        assert_eq!(c.opensentry_mcp_agent_key, "mcp-secret");
        assert_eq!(
            c.sentinel_agent_key, "",
            "the run-queue key must not be inferred"
        );

        // The agent's own spelling beats Command Center's.
        let c = config(&[
            ("OLLAMA_API_KEY", "k"),
            ("OPENSENTRY_MCP_AGENT_KEY", "a"),
            ("SENTINEL_AGENT_MCP_KEY", "b"),
        ])
        .unwrap();
        assert_eq!(c.opensentry_mcp_agent_key, "a");
    }

    #[test]
    fn a_misspelled_mode_refuses_to_boot() {
        let err = config(&[("OLLAMA_API_KEY", "k"), ("AGENT_MODE", "pull")]).unwrap_err();
        assert!(err.contains("agent_mode must be 'push' or 'poll'"), "{err}");
        assert_eq!(
            config(&[("OLLAMA_API_KEY", "k"), ("AGENT_MODE", " POLL ")])
                .unwrap()
                .agent_mode,
            AgentMode::Poll
        );
    }

    #[test]
    fn a_key_with_verification_off_refuses_to_boot() {
        let err = config(&[
            ("OLLAMA_API_KEY", "k"),
            ("SENTINEL_AGENT_KEY", "secret"),
            ("WEBHOOK_VERIFY_SIGNATURE", "false"),
        ])
        .unwrap_err();
        assert!(err.contains("refusing to start"), "{err}");
        // Verification off with NO key is local dev and is allowed.
        assert!(config(&[("OLLAMA_API_KEY", "k"), ("WEBHOOK_VERIFY_SIGNATURE", "0")]).is_ok());
    }

    #[test]
    fn the_mcp_url_is_derived_unless_overridden() {
        let c = config(&[
            ("OLLAMA_API_KEY", "k"),
            ("OPENSENTRY_API_BASE", "http://cc:8000/"),
        ])
        .unwrap();
        assert_eq!(c.mcp_url(), "http://cc:8000/mcp/");
        let c = config(&[
            ("OLLAMA_API_KEY", "k"),
            ("OPENSENTRY_MCP_URL", "http://x/mcp/"),
        ])
        .unwrap();
        assert_eq!(c.mcp_url(), "http://x/mcp/");
        // The old default, set explicitly: rmcp does not follow the 307.
        let c = config(&[
            ("OLLAMA_API_KEY", "k"),
            ("OPENSENTRY_MCP_URL", "http://x/mcp"),
        ])
        .unwrap();
        assert_eq!(c.mcp_url(), "http://x/mcp/");
        // A custom path is left exactly as given.
        let c = config(&[
            ("OLLAMA_API_KEY", "k"),
            ("OPENSENTRY_MCP_URL", "http://x/tools"),
        ])
        .unwrap();
        assert_eq!(c.mcp_url(), "http://x/tools");
    }

    #[test]
    fn an_unknown_provider_prefix_is_named_not_guessed() {
        let err = parse_model("gemini/gemini-3-pro").unwrap_err();
        assert!(err.contains("does not speak"), "{err}");
        assert!(parse_model("mystery-model").is_err());
        assert!(parse_model("openai/").is_err());
        assert_eq!(
            parse_model("claude-sonnet-5").unwrap().provider,
            Provider::Anthropic
        );
        assert_eq!(parse_model("gpt-5.1").unwrap().provider, Provider::OpenAi);
        // A model name may itself contain a slash; only the first splits.
        assert_eq!(
            parse_model("openai/org/model-x").unwrap().model,
            "org/model-x"
        );
    }

    #[test]
    fn a_bad_number_or_boolean_names_its_variable() {
        let err = config(&[("OLLAMA_API_KEY", "k"), ("MAX_TOKENS", "lots")]).unwrap_err();
        assert!(err.contains("MAX_TOKENS"), "{err}");
        let err = config(&[
            ("OLLAMA_API_KEY", "k"),
            ("WEBHOOK_VERIFY_SIGNATURE", "maybe"),
        ])
        .unwrap_err();
        assert!(err.contains("WEBHOOK_VERIFY_SIGNATURE"), "{err}");
    }

    #[test]
    fn dotenv_reads_the_common_shapes_and_does_not_interpolate() {
        let env = parse_dotenv(
            "# comment\n\nAGENT_MODE=poll\nexport LLM_API_KEY=\"sk-1\"\nHASH='$argon2id$v=19'\nEMPTY=\nnot a pair\n",
        );
        assert_eq!(env["AGENT_MODE"], "poll");
        assert_eq!(env["LLM_API_KEY"], "sk-1");
        assert_eq!(env["HASH"], "$argon2id$v=19");
        assert_eq!(env["EMPTY"], "");
        assert_eq!(env.len(), 4);
    }
}
