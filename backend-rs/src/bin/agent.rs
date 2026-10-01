//! `sentinel-agent` — the Sentinel AI agent's entry point.
//!
//! Replaces `python -m app.sentinel_agent`. One binary serves both places
//! the agent runs: Fly, as the `agent` process group, and a self-hosted
//! operator's own box, usually with `AGENT_MODE=poll` so it needs no
//! inbound connectivity.
//!
//! It is an HTTP server rather than a bare worker because push mode
//! receives a signed wakeup, and because `/health` is what Fly and any
//! other monitor ask.

use sentinel_command::agent::config::AgentConfig;
use sentinel_command::agent::server::{serve, AgentState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with(tracing_subscriber::fmt::layer())
            .with(sentinel_command::sentry::tracing_layer())
            .init();
    }
    // The agent sleeps between wakeups, so its logs are ephemeral and
    // nobody is watching them: a run that errors every time — an expired
    // key, a model 404, an MCP key mismatch — is invisible without this.
    let _sentry = sentinel_command::sentry::init_with_default_environment(Some("production"));

    // Configuration failures are sentences, printed once, and a non-zero
    // exit — before anything binds a port.
    let config = AgentConfig::from_env().map_err(anyhow::Error::msg)?;
    let state = AgentState::new(config).map_err(anyhow::Error::msg)?;
    serve(state).await
}
