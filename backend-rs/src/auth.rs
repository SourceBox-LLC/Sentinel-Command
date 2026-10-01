//! Session verification — the migration's keystone.
//!
//! There is no Rust Clerk SDK, and none is needed for the hot path. The
//! Python service's own comment says it plainly: verification "is local
//! RS256 most of the time", wrapped in `asyncio.to_thread` only because
//! the Python SDK is synchronous. Locally verifying an RS256 JWT against
//! a cached JWKS is the whole mechanism — see `jwks.rs` for the cache and
//! `claims.rs` for turning verified claims into an `AuthUser`.
//!
//! 75 of the 100 routes are gated on this, so it has to reproduce
//! `backend/app/core/auth.py` exactly — organisation membership, the
//! admin/viewer split, the billing gate, and the status codes, which the
//! SPA branches on (a 400 means "pick an organisation", a 401 means "sign
//! in again", and confusing the two loops the user).

pub mod claims;
pub mod jwks;
pub mod local;

use axum::extract::FromRequestParts;
use axum::http::{request::Parts, StatusCode};
use jsonwebtoken::{Algorithm, Validation};
use serde_json::Value;

pub use claims::{auth_user_from_claims, decode_v2_permissions, AuthUser, ClaimError};

use crate::app::AppState;
use crate::config::Config;
use crate::error::ApiError;

/// Cookie Clerk stores the session JWT in. The SDK accepts either this or
/// an `Authorization: Bearer` header, and Command Center's clients use
/// both — the SPA sends the header, but a same-origin navigation carries
/// only the cookie.
const CLERK_SESSION_COOKIE: &str = "__session";

#[derive(Debug)]
pub enum AuthError {
    /// Clerk selected as the provider but not configured. 503.
    NotConfigured,
    /// No credential, or one that does not verify. 401.
    NotAuthenticated,
    /// Something went wrong that is not the caller's fault — a JWKS
    /// fetch failure, a malformed publishable key. 401 with a distinct
    /// message, and the detail goes to the log rather than the response.
    Failed,
    /// Verified, but no organisation selected. 400, and deliberately not
    /// a 401: the caller is signed in and must not be bounced to sign-in.
    NoOrganization,
}

impl From<AuthError> for ApiError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::NotConfigured => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Clerk authentication not configured. Set CLERK_SECRET_KEY and CLERK_PUBLISHABLE_KEY.",
            ),
            AuthError::NotAuthenticated => ApiError::unauthorized("Not authenticated"),
            AuthError::Failed => ApiError::unauthorized("Authentication failed"),
            AuthError::NoOrganization => ApiError::bad_request(
                "No organization selected. Please create or join an organization.",
            ),
        }
    }
}

impl From<ClaimError> for AuthError {
    fn from(err: ClaimError) -> Self {
        match err {
            ClaimError::NotAuthenticated => AuthError::NotAuthenticated,
            ClaimError::NoOrganization => AuthError::NoOrganization,
            ClaimError::Malformed(claim) => {
                // Clerk signed this, so a wrong-typed claim means the
                // wire format moved under us. That is an operator
                // problem and needs to be visible as one.
                tracing::error!(claim, "session token carried an unexpected claim type");
                AuthError::Failed
            }
        }
    }
}

/// Which credential scheme this deployment runs.
///
/// Resolved once at startup rather than per request, so a request never
/// pays for parsing the publishable key or building a validation set.
pub enum Authenticator {
    Clerk(ClerkVerifier),
    Local {
        secret: String,
        email: String,
        username: String,
    },
    /// `AUTH_PROVIDER=clerk` with no keys set. Every authenticated route
    /// answers 503 — the same signal the Python service gives.
    Unconfigured,
}

impl Authenticator {
    pub fn from_config(config: &Config, http: reqwest::Client) -> Self {
        if config.is_local_auth() {
            return Authenticator::Local {
                secret: config.app_secret_key.clone(),
                email: config.local_admin_email.clone(),
                username: config.local_admin_username.clone(),
            };
        }
        if !config.is_clerk_configured() {
            return Authenticator::Unconfigured;
        }
        Authenticator::Clerk(ClerkVerifier::new(
            config.clerk_issuer.clone(),
            config.frontend_url.clone(),
            http,
        ))
    }

    /// Resolve the caller from a request's headers.
    ///
    /// On success the caller's identity is tagged onto the per-request
    /// Sentry scope, where the Python did the same from
    /// `get_current_user`. Deliberately after the token is verified and
    /// deliberately only `user_id`, `org_id` and `plan`: an email or a
    /// username would be PII, and the point of the tags is to find which
    /// org an error belongs to, not who was logged in.
    pub async fn authenticate(&self, parts: &Parts) -> Result<AuthUser, AuthError> {
        let resolved = self.authenticate_inner(parts).await;
        if let Ok(ref user) = resolved {
            crate::sentry::set_user_context(&user.user_id, &user.org_id, &user.plan);
        }
        resolved
    }

    async fn authenticate_inner(&self, parts: &Parts) -> Result<AuthUser, AuthError> {
        match self {
            Authenticator::Unconfigured => Err(AuthError::NotConfigured),

            Authenticator::Local {
                secret,
                email,
                username,
            } => {
                // Local mode reads only the header — there is no Clerk
                // cookie to fall back to.
                let token = bearer_token(parts).ok_or(AuthError::NotAuthenticated)?;
                let claims =
                    local::verify_token(secret, token).ok_or(AuthError::NotAuthenticated)?;
                Ok(local::auth_user_for(&claims, email, username))
            }

            Authenticator::Clerk(verifier) => {
                let token = bearer_token(parts)
                    .map(str::to_string)
                    .or_else(|| session_cookie(parts))
                    .ok_or(AuthError::NotAuthenticated)?;
                let claims = verifier.verify(&token).await?;
                Ok(auth_user_from_claims(&claims)?)
            }
        }
    }
}

fn bearer_token(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn session_cookie(parts: &Parts) -> Option<String> {
    let header = parts.headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for pair in header.split(';') {
        let (name, value) = pair.split_once('=')?;
        if name.trim() == CLERK_SESSION_COOKIE {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Verifies Clerk session tokens: RS256, against a cached JWKS.
pub struct ClerkVerifier {
    issuer: Option<String>,
    jwks: Option<jwks::JwksCache>,
    /// The Python service passes `authorized_parties=[FRONTEND_URL]`,
    /// and the SDK then *requires* `azp` to be present and to match one
    /// of them exactly.
    authorized_party: String,
}

impl ClerkVerifier {
    pub fn new(issuer: Option<String>, frontend_url: String, http: reqwest::Client) -> Self {
        let jwks = issuer.as_deref().map(|iss| jwks::JwksCache::new(iss, http));
        Self {
            issuer,
            jwks,
            // Stored verbatim. The SDK's check is `azp not in
            // authorized_parties` — plain string membership, with no
            // trailing-slash normalisation — so normalising here would
            // accept origins the Python rejects.
            authorized_party: frontend_url,
        }
    }

    pub async fn verify(&self, token: &str) -> Result<Value, AuthError> {
        // Configured but unusable — a malformed publishable key. Python
        // reaches the SDK, which throws, and its blanket handler turns
        // that into "Authentication failed"; same outcome here.
        let (Some(issuer), Some(jwks)) = (self.issuer.as_deref(), self.jwks.as_ref()) else {
            tracing::error!(
                "clerk is configured but no issuer could be derived from \
                 CLERK_PUBLISHABLE_KEY; no token can be verified"
            );
            return Err(AuthError::Failed);
        };

        let header = jsonwebtoken::decode_header(token).map_err(|_| AuthError::NotAuthenticated)?;
        // Belt and braces, not the barrier: `Validation::new(RS256)`
        // below already restricts the accepted algorithms, and removing
        // this check leaves every algorithm-confusion test passing
        // (measured). It stays because it costs nothing and makes the
        // intent legible at the point the header is read — but the
        // guarantee comes from the validation set, so that is the line
        // to leave alone.
        if header.alg != Algorithm::RS256 {
            return Err(AuthError::NotAuthenticated);
        }
        let kid = header.kid.ok_or(AuthError::NotAuthenticated)?;

        let key = match jwks.key_for(&kid).await {
            Ok(key) => key,
            Err(jwks::JwksError::UnknownKey(_)) => {
                // Clerk does not know this signing key: the token is not
                // from this instance. That is the caller's problem.
                return Err(AuthError::NotAuthenticated);
            }
            Err(err) => {
                tracing::error!(error = %err, "jwks unavailable; cannot verify session");
                return Err(AuthError::Failed);
            }
        };

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[issuer]);
        // Clerk does not set `aud` on session tokens; `azp` is checked
        // below instead, which is what the SDK's authorized_parties does.
        validation.validate_aud = false;
        validation.set_required_spec_claims(&["exp", "iss"]);
        validation.validate_nbf = true;
        // Clerk's own SDKs allow a few seconds for clock drift between
        // their issuing host and ours; without it a correct token fails
        // for the first seconds of its life.
        validation.leeway = 5;

        let data = jsonwebtoken::decode::<Value>(token, &key, &validation)
            .map_err(|_| AuthError::NotAuthenticated)?;

        // `azp` identifies the origin the token was minted for.
        //
        // A **missing** azp is rejected, not waved through. Clerk's docs
        // say azp "could be omitted if, for privacy-related reasons,
        // Origin is empty or null", which reads like a reason to treat
        // it as optional — but the Python SDK this must match does:
        //
        //     if options.authorized_parties is not None:
        //         azp = payload.get("azp")
        //         if azp is None or azp not in options.authorized_parties:
        //             raise TokenVerificationError(...)
        //
        // and Command Center always passes authorized_parties. So an
        // absent azp is a 401 over there, and accepting it here would
        // make the Rust tier the more permissive of the two.
        //
        // The comparison is exact — `not in` on a list of strings, no
        // trailing-slash normalisation.
        let azp = data.claims.get("azp").and_then(Value::as_str);
        match azp {
            Some(azp) if azp == self.authorized_party => {}
            _ => {
                tracing::warn!(
                    azp = ?azp,
                    "token rejected: azp absent or not the configured frontend"
                );
                return Err(AuthError::NotAuthenticated);
            }
        }

        Ok(data.claims)
    }
}

// --- extractors -------------------------------------------------------
//
// These mirror the FastAPI dependencies: `get_current_user`,
// `require_view`, `require_admin`. `require_active_billing` needs the
// database and lives with the routes that use it.

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(state.auth.authenticate(parts).await?)
    }
}

/// `require_view`. Every org member can view cameras, so this never
/// rejects an authenticated caller today — it exists so that if that
/// stops being true, the gate is already at every call site.
pub struct RequireView(pub AuthUser);

impl FromRequestParts<AppState> for RequireView {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = state.auth.authenticate(parts).await?;
        if !user.can_view_cameras() {
            return Err(ApiError::forbidden("View permission required"));
        }
        Ok(RequireView(user))
    }
}

/// `require_admin`.
pub struct RequireAdmin(pub AuthUser);

impl FromRequestParts<AppState> for RequireAdmin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = state.auth.authenticate(parts).await?;
        if !user.is_admin() {
            return Err(ApiError::forbidden("Admin permission required"));
        }
        Ok(RequireAdmin(user))
    }
}

/// `require_active_billing` — admin, and payment not past due.
///
/// Used for writes (create a node, mint a key) so a past-due org can
/// still read its own cameras but cannot provision new resources.
///
/// The order matters and is not arbitrary: admin is checked first, so a
/// non-admin in a past-due org gets 403, not 402. Telling a viewer about
/// the organisation's billing state would be a leak, and it would send
/// them to a billing page they have no permission to act on.
pub struct RequireActiveBilling(pub AuthUser);

impl FromRequestParts<AppState> for RequireActiveBilling {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = state.auth.authenticate(parts).await?;
        if !user.is_admin() {
            return Err(ApiError::forbidden("Admin permission required"));
        }
        if crate::settings::payment_past_due(&state.pool, &user.org_id).await? {
            return Err(ApiError::new(
                StatusCode::PAYMENT_REQUIRED,
                "Your payment is past due. Please update your billing information \
                 before making changes.",
            ));
        }
        Ok(RequireActiveBilling(user))
    }
}

/// Derive the Clerk issuer from a publishable key.
///
/// A Clerk publishable key is `pk_test_` / `pk_live_` followed by the
/// base64 of the Frontend API host with a `$` terminator, e.g.
/// `pk_test_<base64("example-42.clerk.accounts.dev$")>`. Session tokens
/// are issued by `https://<that host>`, so the key alone is enough to know
/// what issuer to trust — no configuration and no network call.
///
/// Returns `None` for an empty or malformed key rather than guessing: a
/// wrong issuer must fail closed at verification, not silently trust the
/// wrong tenant.
pub fn issuer_from_publishable_key(key: &str) -> Option<String> {
    let encoded = key
        .strip_prefix("pk_test_")
        .or_else(|| key.strip_prefix("pk_live_"))?;
    if encoded.is_empty() {
        return None;
    }

    let decoded = base64_decode(encoded)?;
    let host = String::from_utf8(decoded).ok()?;
    let host = host.strip_suffix('$').unwrap_or(&host).trim().to_string();
    if host.is_empty() || host.contains('/') || host.contains(' ') {
        return None;
    }
    Some(format!("https://{host}"))
}

/// Minimal standard-alphabet base64 decoder, padding optional.
///
/// Clerk keys are the only base64 this crate handles, so a dependency for
/// it would be a dependency for one call site.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;

    for ch in input.bytes() {
        if ch == b'=' {
            break;
        }
        let val = TABLE.iter().position(|&c| c == ch)? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_issuer_from_a_test_key() {
        // base64("example-42.clerk.accounts.dev$")
        let key = "pk_test_ZXhhbXBsZS00Mi5jbGVyay5hY2NvdW50cy5kZXYk";
        assert_eq!(
            issuer_from_publishable_key(key).as_deref(),
            Some("https://example-42.clerk.accounts.dev")
        );
    }

    #[test]
    fn handles_live_keys_the_same_way() {
        let key = "pk_live_ZXhhbXBsZS00Mi5jbGVyay5hY2NvdW50cy5kZXYk";
        assert_eq!(
            issuer_from_publishable_key(key).as_deref(),
            Some("https://example-42.clerk.accounts.dev")
        );
    }

    #[test]
    fn tolerates_a_missing_dollar_terminator() {
        // base64("example-42.clerk.accounts.dev")
        let key = "pk_test_ZXhhbXBsZS00Mi5jbGVyay5hY2NvdW50cy5kZXY=";
        assert_eq!(
            issuer_from_publishable_key(key).as_deref(),
            Some("https://example-42.clerk.accounts.dev")
        );
    }

    #[test]
    fn fails_closed_on_anything_malformed() {
        // A wrong issuer must never be guessed — it would mean trusting
        // tokens from somewhere other than this tenant's Clerk instance.
        for key in [
            "",
            "pk_test_",
            "sk_test_abc",
            "not-a-key",
            "pk_test_!!!!",
            // decodes to text containing a slash — not a bare host
            "pk_test_aHR0cDovL2V2aWwuZXhhbXBsZS8k",
        ] {
            assert!(
                issuer_from_publishable_key(key).is_none(),
                "should reject {key:?}"
            );
        }
    }

    fn parts_with(headers: &[(&str, &str)]) -> Parts {
        let mut builder = axum::http::Request::builder().uri("/");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    #[test]
    fn reads_a_bearer_token_from_the_authorization_header() {
        let parts = parts_with(&[("authorization", "Bearer abc.def.ghi")]);
        assert_eq!(bearer_token(&parts), Some("abc.def.ghi"));
    }

    #[test]
    fn ignores_an_authorization_header_that_is_not_bearer() {
        let parts = parts_with(&[("authorization", "Basic dXNlcjpwYXNz")]);
        assert_eq!(bearer_token(&parts), None);
    }

    #[test]
    fn finds_the_session_cookie_among_others() {
        let parts = parts_with(&[("cookie", "foo=1; __session=tok.en.here; bar=2")]);
        assert_eq!(session_cookie(&parts).as_deref(), Some("tok.en.here"));
    }

    #[test]
    fn returns_no_cookie_when_the_session_one_is_absent() {
        let parts = parts_with(&[("cookie", "foo=1; bar=2")]);
        assert_eq!(session_cookie(&parts), None);
    }

    #[tokio::test]
    async fn an_unconfigured_clerk_deployment_answers_503() {
        // Not a 401: the caller did nothing wrong, and an operator
        // reading the log needs to see a configuration problem rather
        // than a wave of failed logins.
        let auth = Authenticator::Unconfigured;
        let parts = parts_with(&[("authorization", "Bearer whatever")]);
        let err: ApiError = auth.authenticate(&parts).await.unwrap_err().into();
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn local_mode_rejects_a_request_with_no_credential() {
        let auth = Authenticator::Local {
            secret: "s3cret".into(),
            email: "a@b.c".into(),
            username: "admin".into(),
        };
        let err: ApiError = auth
            .authenticate(&parts_with(&[]))
            .await
            .unwrap_err()
            .into();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn local_mode_resolves_a_valid_token() {
        let auth = Authenticator::Local {
            secret: "s3cret".into(),
            email: "a@b.c".into(),
            username: "admin".into(),
        };
        let token = local::issue_token("s3cret", "self-host").unwrap();
        let user = auth
            .authenticate(&parts_with(&[("authorization", &format!("Bearer {token}"))]))
            .await
            .unwrap();
        assert_eq!(user.org_id, "self-host");
        assert!(user.is_admin());
    }

    #[tokio::test]
    async fn local_mode_ignores_the_clerk_cookie() {
        // There is no Clerk in local mode; accepting a cookie named
        // __session would accept something nothing here ever signed.
        let auth = Authenticator::Local {
            secret: "s3cret".into(),
            email: "a@b.c".into(),
            username: "admin".into(),
        };
        let token = local::issue_token("s3cret", "self-host").unwrap();
        let err: ApiError = auth
            .authenticate(&parts_with(&[("cookie", &format!("__session={token}"))]))
            .await
            .unwrap_err()
            .into();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn claim_errors_keep_their_distinct_statuses() {
        // The SPA branches on these: 400 prompts for an organisation,
        // 401 sends the user back to sign-in. Swapping them loops.
        let no_org: ApiError = AuthError::from(ClaimError::NoOrganization).into();
        assert_eq!(no_org.status, StatusCode::BAD_REQUEST);
        let not_auth: ApiError = AuthError::from(ClaimError::NotAuthenticated).into();
        assert_eq!(not_auth.status, StatusCode::UNAUTHORIZED);
    }
}
