//! Command Center backend entry point.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::postgres::PgPoolOptions;

use sentinel_command::config::Config;
use sentinel_command::{build_router, AppState, VERSION};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::from_env();
    let port = config.port;

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&config.database_url)
        .await?;

    // Schema bring-up. The migration adopts what SQLAlchemy's create_all()
    // plus the hand-rolled sync_schema() already built in production —
    // taken from `pg_dump --schema-only` rather than transcribed — so this
    // is a verified no-op against the live database. Both stacks share one
    // schema for the whole migration; neither may redefine it.
    sqlx::migrate!("./migrations").run(&pool).await?;

    if config.clerk_issuer.is_none() && config.auth_provider == "clerk" {
        // Not fatal yet: nothing Rust serves is Clerk-gated until slice 1.
        // It will be fatal then, and saying so now beats discovering it
        // when the first authed route moves over.
        tracing::warn!(
            "CLERK_PUBLISHABLE_KEY missing or malformed — no issuer could be derived. \
             Clerk-gated routes cannot move to Rust until this resolves."
        );
    }

    let state = AppState {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?,
        config: Arc::new(config),
        pool,
        started_at: Instant::now(),
    };

    let upstream = state.config.upstream.clone();
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!(port, %upstream, version = VERSION, "command center (rust tier) listening");

    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
