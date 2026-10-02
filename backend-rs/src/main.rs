//! Command Center backend entry point.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sentinel_command::auth::Authenticator;
use sentinel_command::config::Config;
use sentinel_command::{build_router, AppState, VERSION};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // One subscriber, two layers: the usual formatter, plus the bridge
    // that turns `tracing::error!` into a Sentry event. They go in ONE
    // subscriber because a second `init()` is silently ignored — which
    // is how error tracking can look configured and deliver nothing.
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

    // Sentry second, and the order took a wrong turn first. The Python
    // initialised it before anything else, "as early as possible so any
    // exception raised during app construction is still captured", and
    // that reasoning does not carry over: the layer above resolves the
    // hub at event time, so it captures everything after this line
    // whatever order the two were installed in. Putting `init` first
    // only meant its own log lines — including "SENTRY_DSN is not a
    // valid DSN" — had no subscriber yet and went nowhere. The silence
    // cost twenty minutes of chasing a hang that was a stopped Postgres
    // container.
    //
    // The guard is held to the end of `main`: dropping it flushes the
    // queue and shuts the client down, so `_` instead of `_sentry` would
    // disable Sentry on the line that enabled it.
    let _sentry = sentinel_command::sentry::init();

    let config = Config::from_env();
    let port = config.port;

    // One database driver is compiled into each binary. A URL for the
    // other one is handed to the sibling binary if it is installed, and
    // refused with a sentence if it is not.
    sentinel_command::db::dispatch_to_matching_build(&config.database_url);
    if let Some(message) = sentinel_command::config::unsupported_database_url(&config.database_url)
    {
        anyhow::bail!(message);
    }

    let pool = sentinel_command::db::connect(&config.database_url, 10).await?;

    // Schema bring-up. The migration adopts what SQLAlchemy's create_all()
    // plus the hand-rolled sync_schema() already built in production —
    // taken from `pg_dump --schema-only` rather than transcribed — so this
    // is a verified no-op against the live database. Both stacks share one
    // schema for the whole migration; neither may redefine it.
    //
    // The SQLite build's schema is the same models seen through SQLite —
    // see migrations-sqlite/.
    sentinel_command::db::MIGRATOR.run(&pool).await?;

    if config.clerk_issuer.is_none() && config.auth_provider == "clerk" {
        // Not fatal yet: nothing Rust serves is Clerk-gated until slice 1.
        // It will be fatal then, and saying so now beats discovering it
        // when the first authed route moves over.
        tracing::warn!(
            "CLERK_PUBLISHABLE_KEY missing or malformed — no issuer could be derived. \
             Clerk-gated routes cannot move to Rust until this resolves."
        );
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;

    let state = AppState {
        auth: Arc::new(Authenticator::from_config(&config, http.clone())),
        cors: sentinel_command::cors::CorsConfig::from_env(
            &config.frontend_url,
            &config.cors_allowed_origins,
        ),
        hls: Arc::new(sentinel_command::hls::HlsCache::new()),
        limiter: Arc::new(sentinel_command::ratelimit::Limiter::from_env(&config.redis_url).await),
        http,
        config: Arc::new(config),
        pool,
        started_at: Instant::now(),
        started_at_wall: chrono::Utc::now(),
    };

    // The loops that keep the video caches honest: flushing viewer
    // seconds to the database, and reaping cameras that stopped
    // pushing. Both belong to whichever process owns the caches, and
    // that is now this one.
    sentinel_command::hls::spawn_loops(state.clone());
    // And the one that keeps the newest CameraNode release known, so
    // the heartbeat path never waits on GitHub.
    sentinel_command::versions::spawn_refresh_loop(state.http.clone());
    // The outbox drain. It owns the last-tick stamp the health probe
    // reads, so it has to run in whichever process answers that probe.
    tokio::spawn(sentinel_command::email_worker::email_worker_loop(
        state.clone(),
    ));
    // And the database-backed sweeps: the offline sweep, log cleanup,
    // sentinel reaper, motion digest and disk check, plus whichever of
    // the licence check-in, data sync and plan reconcile this auth mode
    // calls for. `loops::spawn_loops` decides that, the way main.py's
    // lifespan does.
    sentinel_command::loops::spawn_loops(state.clone());

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!(port, version = VERSION, "command center listening");

    // with_connect_info so the rate limiter can fall back to the peer
    // address when no proxy header identifies the client.
    axum::serve(
        listener,
        build_router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    })
    .await?;
    Ok(())
}
