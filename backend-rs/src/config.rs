//! Environment configuration.
//!
//! Every name and default is carried over from the Python service's
//! `backend/app/core/config.py`. The Fly app's secrets are already set
//! against these names, so renaming one silently reverts it to a default
//! at the next deploy.

use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    /// Port this process listens on. Rust owns the public 8000.
    pub port: u16,
    /// Where the Python app now listens, on localhost. Everything this
    /// service has not yet ported is proxied here — see `proxy.rs`.
    pub upstream: String,
    /// Directory holding the built SPA (`frontend/dist` copied to
    /// `/app/static` by the Dockerfile).
    pub static_dir: String,

    // --- Clerk ---------------------------------------------------------
    pub clerk_secret_key: String,
    pub clerk_publishable_key: String,
    /// Resolved from the publishable key; see `auth::issuer_from_publishable_key`.
    pub clerk_issuer: Option<String>,

    pub local_org_id: String,
    pub auth_provider: String,
}

fn var_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

impl Config {
    pub fn from_env() -> Self {
        let clerk_publishable_key = var_or("CLERK_PUBLISHABLE_KEY", "");
        Self {
            database_url: normalize_database_url(&var_or(
                "DATABASE_URL",
                "postgresql://postgres:postgres@localhost:5432/sentinel",
            )),
            port: var_or("PORT", "8000").parse().unwrap_or(8000),
            upstream: var_or("PYTHON_UPSTREAM", "http://127.0.0.1:8001"),
            static_dir: var_or("STATIC_DIR", "/app/static"),
            clerk_issuer: crate::auth::issuer_from_publishable_key(&clerk_publishable_key),
            clerk_secret_key: var_or("CLERK_SECRET_KEY", ""),
            clerk_publishable_key,
            local_org_id: var_or("LOCAL_ORG_ID", "local"),
            auth_provider: var_or("AUTH_PROVIDER", "clerk"),
        }
    }
}

/// Strip SQLAlchemy's `+driver` from a URL scheme.
///
/// The Fly secret holds `postgresql+psycopg://…`, which sqlx does not
/// understand. Normalising here means the secret does not have to be
/// rewritten during a migration that is already changing the runtime —
/// and it means both stacks can read the same variable while they run
/// side by side.
pub fn normalize_database_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match scheme.split_once('+') {
            Some((base, _driver)) => format!("{base}://{rest}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_database_url;

    #[test]
    fn strips_the_sqlalchemy_driver_suffix() {
        assert_eq!(
            normalize_database_url("postgresql+psycopg://u:p@h:5432/db"),
            "postgresql://u:p@h:5432/db"
        );
    }

    #[test]
    fn leaves_a_plain_url_and_a_plus_in_the_password_alone() {
        for url in [
            "postgres://u:p@h/db",
            "postgresql://user:pa+ss@h/db",
            "sqlite:///./local.db",
        ] {
            assert_eq!(normalize_database_url(url), url);
        }
    }
}
