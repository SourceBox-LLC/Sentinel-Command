//! Error tracking. Ported from `app/core/sentry.py`.
//!
//! **This was missing from the rewrite until now, and the gap was
//! silent in the worst way.** Production's DSN is injected by the Fly
//! Sentry extension, so nothing in the repository looks unconfigured;
//! the Rust tier simply never read it. Two things stop working and
//! neither announces itself:
//!
//! * every unhandled error becomes a line in `fly logs` and nothing
//!   else — no grouping, no alert, no stack trace kept;
//! * `loops::check_disk_critical` has no delivery path at all. It fires
//!   one `tracing::error!` with structured fields when `/data` passes
//!   95%, and that is deliberate — the alert is operator-side and must
//!   NOT go through customer notifications (a multi-tenant violation
//!   removed in May 2026). Sentry was the thing listening.
//!
//! So the `tracing` integration below is not a nice-to-have: it is what
//! makes an `error!` an alert. ERROR becomes an event, INFO and WARN
//! become breadcrumbs, which is exactly what the Python's
//! `LoggingIntegration(level=INFO, event_level=ERROR)` did.
//!
//! What is deliberately NOT ported:
//!
//! * `capture_exception` — it had no callers. A wrapper whose only
//!   purpose was to avoid importing the SDK is not worth carrying into a
//!   language where the import is free.
//! * profiling. The Python pinned `profiles_sample_rate=0.0` with the
//!   reason "doubles event count and we're on the free tier"; the
//!   sponsored plan is the same plan.
//!
//! The scrubber IS ported, and it matters more here than it reads.
//! `send_default_pii = false` already keeps headers and bodies out, but
//! a URL is not PII and carries secrets on this service specifically:
//! the pre-v0.1.65 CameraNode WebSocket handshake puts `api_key=` in the
//! query string. Every event therefore loses its query string before it
//! leaves the process.

use std::borrow::Cow;

/// Hold this for the life of the process — dropping it flushes and
/// disables the client.
pub type Guard = sentry::ClientInitGuard;

/// Header names redacted on the way out, lowercased.
///
/// Belt and braces: `send_default_pii = false` means headers are not
/// attached in the first place. They are listed because the cost of
/// being wrong about that is a node API key or the multi-tenant agent
/// secret in a third-party error tracker.
const REDACTED_HEADERS: [&str; 5] = [
    "authorization",
    "cookie",
    "x-node-api-key",
    "x-sentinel-agent-key",
    "x-agent-org-override",
];

/// Start Sentry, or don't.
///
/// `None` when `SENTRY_DSN` is unset or unparseable, which is the normal
/// case for local dev and every test. The Python logged and returned
/// False; this returns `None` and logs, and the caller holds the guard.
///
/// A bad DSN is logged and ignored rather than fatal, for the Python's
/// stated reason: nothing about a monitoring tool should be able to take
/// the app down.
pub fn init() -> Option<Guard> {
    let dsn = std::env::var("SENTRY_DSN")
        .unwrap_or_default()
        .trim()
        .to_string();
    if dsn.is_empty() {
        tracing::info!("[Sentry] SENTRY_DSN not set — error tracking disabled");
        return None;
    }

    let traces_sample_rate = std::env::var("SENTRY_TRACES_SAMPLE_RATE")
        .ok()
        .and_then(|raw| raw.trim().parse::<f32>().ok())
        .unwrap_or(0.1);

    // Fly injects FLY_APP_NAME and FLY_MACHINE_VERSION when deployed.
    // Falling back to "development" locally is what keeps a dev run from
    // being grouped with production on the dashboard.
    let environment = std::env::var("SENTRY_ENVIRONMENT")
        .ok()
        .filter(|v| !v.is_empty());
    let environment = environment.unwrap_or_else(|| {
        if std::env::var("FLY_APP_NAME").is_ok_and(|v| !v.is_empty()) {
            "production".to_string()
        } else {
            "development".to_string()
        }
    });
    let release = std::env::var("SENTRY_RELEASE")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            std::env::var("FLY_MACHINE_VERSION")
                .ok()
                .filter(|v| !v.is_empty())
        });

    // `ClientOptions` is `#[non_exhaustive]`, so it is built by mutating
    // the default rather than by a struct literal.
    let mut options = sentry::ClientOptions::default();
    options.environment = Some(Cow::Owned(environment.clone()));
    options.release = release.clone().map(Cow::Owned);
    // Explicit: no auth headers, IPs or cookies leave the process unless
    // something pins them to a scope on purpose.
    options.send_default_pii = false;
    // The Python's `traces_sample_rate`. A rate of zero means Disabled
    // rather than FixedRate(0.0) — same effect, and it keeps the
    // tracing machinery out of the hot path entirely.
    options.traces_sampling_strategy = if traces_sample_rate > 0.0 {
        sentry::TracesSamplingStrategy::FixedRate(traces_sample_rate)
    } else {
        sentry::TracesSamplingStrategy::Disabled
    };
    options.before_send = Some(std::sync::Arc::new(scrub));

    let dsn = match dsn.parse::<sentry::types::Dsn>() {
        Ok(dsn) => dsn,
        Err(err) => {
            tracing::error!(error = %err, "[Sentry] SENTRY_DSN is not a valid DSN — error tracking disabled");
            return None;
        }
    };
    let guard = sentry::init((dsn, options));
    tracing::info!(
        environment = %environment,
        release = release.as_deref().unwrap_or("(unset)"),
        traces_sample_rate,
        "[Sentry] initialized"
    );
    Some(guard)
}

/// The `tracing` bridge: ERROR is an event, INFO and WARN are
/// breadcrumbs, everything quieter is dropped.
///
/// Returned as a layer rather than installed here because the subscriber
/// is built once in `main`, before the config is read — and a second
/// `tracing` subscriber is not an error anyone sees, it is simply
/// ignored.
pub fn tracing_layer<S>() -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    sentry::integrations::tracing::layer().event_filter(|metadata| {
        use sentry::integrations::tracing::EventFilter;
        // `tower_http`'s TraceLayer logs its own ERROR for every failed
        // response, so a 500 arrived at Sentry TWICE: once from the
        // handler that knows what went wrong, and once as "response
        // failed" from the layer that only knows the status. Measured
        // against a real collector. The second carries nothing the first
        // does not, groups separately, and doubles the event count on a
        // plan the Python was already counting events against — so it is
        // a breadcrumb on the real event instead.
        if metadata.target().starts_with("tower_http::trace") {
            return EventFilter::Breadcrumb;
        }
        match *metadata.level() {
            tracing::Level::ERROR => EventFilter::Event,
            tracing::Level::WARN | tracing::Level::INFO => EventFilter::Breadcrumb,
            _ => EventFilter::Ignore,
        }
    })
}

/// Attach the identity tags triage needs, and nothing more.
///
/// Called from the auth layer once a token has been validated, as the
/// Python did from `get_current_user`. No email, no username, no IP:
/// those are PII and are not needed to find which org an error belongs
/// to.
///
/// Safe when Sentry is not initialised — `configure_scope` is a no-op
/// without a client, so this needs no `is_initialized` check of its own.
/// It is also per-request rather than per-thread, because the router
/// wraps every request in `NewSentryLayer`; without that, these tags
/// would leak from one request onto the next task that happened to land
/// on the same thread.
pub fn set_user_context(user_id: &str, org_id: &str, plan: &str) {
    sentry::configure_scope(|scope| {
        if !user_id.is_empty() {
            scope.set_tag("user_id", user_id);
            scope.set_user(Some(sentry::User {
                id: Some(user_id.to_string()),
                ..Default::default()
            }));
        }
        if !org_id.is_empty() {
            scope.set_tag("org_id", org_id);
        }
        if !plan.is_empty() {
            scope.set_tag("plan", plan);
        }
    });
}

/// `_scrub_event`: drop the query string, redact the secret headers.
fn scrub(mut event: sentry::protocol::Event<'static>) -> Option<sentry::protocol::Event<'static>> {
    if let Some(request) = event.request.as_mut() {
        // Cheap and total: removes `?api_key=…` rather than trying to
        // recognise which parameter is a secret.
        request.query_string = None;
        if let Some(url) = request.url.as_mut() {
            url.set_query(None);
        }
        for (name, value) in request.headers.iter_mut() {
            if REDACTED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
                *value = "[redacted]".to_string();
            }
        }
    }
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No DSN, no client — the property that lets every test and every
    /// local run ignore this module entirely.
    #[test]
    fn without_a_dsn_there_is_no_client() {
        // `init` reads the process environment, and the tests in this
        // binary share one. Only assert the branch that does not need to
        // set a variable: with no DSN configured there is no guard.
        if std::env::var("SENTRY_DSN").is_err() {
            assert!(init().is_none());
        }
    }

    /// The scrubber, which is the part that could leak a credential.
    #[test]
    fn the_query_string_and_the_secret_headers_do_not_leave() {
        let mut request = sentry::protocol::Request {
            url: Some(
                "https://sentinel-command.com/ws/node?api_key=secret&node_id=n1"
                    .parse()
                    .unwrap(),
            ),
            query_string: Some("api_key=secret&node_id=n1".to_string()),
            ..Default::default()
        };
        for name in [
            "Authorization",
            "Cookie",
            "X-Node-API-Key",
            "X-Sentinel-Agent-Key",
            "X-Agent-Org-Override",
            "User-Agent",
        ] {
            request
                .headers
                .insert(name.to_string(), "secret-value".to_string());
        }
        let event = sentry::protocol::Event {
            request: Some(request),
            ..Default::default()
        };

        let scrubbed = scrub(event).expect("the event is kept, only scrubbed");
        let request = scrubbed.request.expect("request survives");
        assert!(request.query_string.is_none());
        assert_eq!(
            request.url.as_ref().map(|u| u.as_str()),
            Some("https://sentinel-command.com/ws/node"),
            "the api_key in the URL is the one the WebSocket handshake puts there"
        );
        for name in [
            "Authorization",
            "Cookie",
            "X-Node-API-Key",
            "X-Sentinel-Agent-Key",
            "X-Agent-Org-Override",
        ] {
            assert_eq!(
                request.headers.get(name).map(String::as_str),
                Some("[redacted]"),
                "{name} was not redacted"
            );
        }
        // Not a secret, and useful for triage — so it is kept, which is
        // what makes the list a list rather than a blanket wipe.
        assert_eq!(
            request.headers.get("User-Agent").map(String::as_str),
            Some("secret-value")
        );
    }

    /// Every name on the redaction list is lowercase, because the lookup
    /// lowercases the header and compares. A capitalised entry would
    /// never match and the test above would still pass for the others.
    #[test]
    fn the_redaction_list_is_lowercase() {
        for name in REDACTED_HEADERS {
            assert_eq!(name, name.to_ascii_lowercase(), "{name} would never match");
        }
    }
}
