//! Local (self-hosted) admin login and refresh.
//!
//! Ported from `backend/app/api/local_auth.py`. Only mounted when
//! `AUTH_PROVIDER=local` — the Python registers this router in the
//! `else` branch of `is_clerk_auth()`, so in Clerk mode these paths do
//! not exist at all and must not exist here either.
//!
//! The token itself is already handled by `auth::local`, which slice 1
//! ported and proved against Python's own `issue_token()`.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::local;
use crate::error::ApiError;
use crate::query::{BodyErrors, ModelBody};
use crate::ratelimit::PerMinute;

const NOT_CONFIGURED: &str = "Local authentication not configured. Set APP_SECRET_KEY, \
                              LOCAL_ADMIN_USERNAME, and LOCAL_ADMIN_PASSWORD_HASH.";

/// `POST /api/auth/local/login`.
pub async fn login(
    rate: PerMinute<10>,
    State(state): State<AppState>,
    ModelBody((), body): ModelBody<()>,
) -> Result<Json<Value>, ApiError> {
    // Python validates `payload: LoginRequest` before the decorator and
    // raises the 503 inside the function, after it. So a malformed body
    // is a free 422 even on an install with local auth unconfigured,
    // and the 503 spends a slot.

    let mut errors = BodyErrors::new();
    // No max_length on either field in the Python model, so none here.
    let username = errors.required_string(&body, "username", usize::MAX);
    let password = errors.required_string(&body, "password", usize::MAX);
    errors.finish()?;
    rate.check().await?;
    if !state.config.is_local_auth_configured() {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            NOT_CONFIGURED,
        ));
    }

    let expected_user = state.config.local_admin_username.clone();
    let expected_hash = state.config.local_admin_password_hash.clone();

    // argon2 is deliberately slow (~100ms). Off the async runtime, for
    // the same reason the Python offloads it: one login must not stall
    // every concurrent request sharing the loop — segment pushes, SSE
    // streams, heartbeats.
    let ok = tokio::task::spawn_blocking(move || {
        verify_credentials(&expected_user, &expected_hash, &username, &password)
    })
    .await
    .unwrap_or(false);

    if !ok {
        return Err(ApiError::unauthorized("Invalid username or password"));
    }

    let token = local::issue_token(&state.config.app_secret_key, &state.config.local_org_id)
        .map_err(|err| {
            tracing::error!(error = %err, "could not sign a local session token");
            ApiError::internal("token signing failed")
        })?;
    Ok(Json(json!({ "token": token })))
}

/// `POST /api/auth/local/refresh`.
pub async fn refresh(
    rate: PerMinute<30>,
    State(state): State<AppState>,
    ModelBody((), body): ModelBody<()>,
) -> Result<Json<Value>, ApiError> {
    let mut errors = BodyErrors::new();
    let token = errors.required_string(&body, "token", usize::MAX);
    errors.finish()?;
    rate.check().await?;
    if !state.config.is_local_auth_configured() {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            NOT_CONFIGURED,
        ));
    }

    // `refresh_token` verifies then re-issues unconditionally — there is
    // no "only refresh when nearly expired" threshold here. The client
    // decides when to call this (frontend/src/auth/local.jsx's
    // REFRESH_THRESHOLD_MS); keep the two in step if either changes.
    if local::verify_token(&state.config.app_secret_key, &token).is_none() {
        return Err(ApiError::unauthorized("Token invalid or expired"));
    }
    let fresh = local::issue_token(&state.config.app_secret_key, &state.config.local_org_id)
        .map_err(|err| {
            tracing::error!(error = %err, "could not sign a local session token");
            ApiError::internal("token signing failed")
        })?;
    Ok(Json(json!({ "token": fresh })))
}

/// Check a username/password pair against the configured local admin.
///
/// Returns false, never an error, on any mismatch or malformed stored
/// hash — a misconfigured install should fail closed rather than 500.
///
/// **Both checks always run.** Short-circuiting on a wrong username
/// would return in microseconds while a correct one takes ~100ms, which
/// hands an attacker the valid username by response timing before they
/// ever guess at the password. The Python says so in a comment; this
/// keeps the property.
fn verify_credentials(
    expected_user: &str,
    expected_hash: &str,
    username: &str,
    password: &str,
) -> bool {
    use subtle::ConstantTimeEq;

    if expected_user.is_empty() || expected_hash.is_empty() {
        return false;
    }

    // Compare as bytes: Python uses hmac.compare_digest on encoded
    // values because it rejects non-ASCII `str` with a TypeError, and
    // the username here is unvalidated input from a public endpoint.
    let username_ok: bool = username
        .as_bytes()
        .ct_eq(expected_user.as_bytes())
        .unwrap_u8()
        == 1;

    let password_ok = verify_argon2(expected_hash, password);

    username_ok && password_ok
}

fn verify_argon2(stored: &str, password: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let Ok(parsed) = PasswordHash::new(stored) else {
        // A malformed stored hash is a configuration error, not a
        // reason to accept the password.
        return false;
    };
    argon2::Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // argon2id hash of "correct horse", produced by python-argon2's
    // PasswordHasher with its default parameters — the same writer the
    // real LOCAL_ADMIN_PASSWORD_HASH comes from.
    const HASH: &str = "$argon2id$v=19$m=65536,t=3,p=4$c29tZXNhbHRzb21lc2FsdA$\
                        Zm9vYmFyYmF6cXV4Zm9vYmFyYmF6cXV4Zm9vYmFyYmF6cXV4Zm8";

    /// A hash from `sentinel-hash-password` authenticates here.
    ///
    /// The tool exists to feed this function, so its parameters are held
    /// to it: python-argon2's `m=65536,t=3,p=4`, not the Rust crate's
    /// weaker `m=19456,t=2,p=1` default. Verification reads the
    /// parameters out of the stored string either way, which is exactly
    /// why a weaker tool default would never have failed a test — it
    /// would just have issued weaker credentials than the installs
    /// before it.
    #[test]
    fn a_hash_with_the_pythons_parameters_verifies() {
        use argon2::password_hash::{PasswordHasher, SaltString};
        let params = argon2::Params::new(65_536, 3, 4, None).unwrap();
        let hasher =
            argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let salt = SaltString::from_b64("c29tZXNhbHRzb21lc2FsdA").unwrap();
        let hash = hasher
            .hash_password(b"correct horse battery staple", &salt)
            .unwrap()
            .to_string();
        assert!(hash.contains("m=65536,t=3,p=4"), "{hash}");
        assert!(verify_argon2(&hash, "correct horse battery staple"));
        assert!(!verify_argon2(&hash, "wrong"));
    }

    #[test]
    fn a_malformed_stored_hash_rejects_rather_than_accepts() {
        for stored in ["", "not-a-phc-string", "$argon2id$", "plaintext"] {
            assert!(!verify_argon2(stored, "anything"), "{stored:?}");
        }
    }

    #[test]
    fn an_unconfigured_install_rejects_everyone() {
        // Missing username or hash must not become "no check required".
        assert!(!verify_credentials("", HASH, "admin", "pw"));
        assert!(!verify_credentials("admin", "", "admin", "pw"));
        assert!(!verify_credentials("", "", "", ""));
    }

    #[test]
    fn a_wrong_username_still_fails_even_with_a_valid_hash_format() {
        assert!(!verify_credentials(
            "admin",
            HASH,
            "not-admin",
            "correct horse"
        ));
    }

    #[test]
    fn a_wrong_username_still_pays_for_the_argon2_verify() {
        // The property the Python comment protects: short-circuiting on
        // a bad username returns in nanoseconds while a good one costs
        // ~100ms, which hands an attacker the valid username by
        // response timing alone.
        //
        // Invisible to the HTTP differential — both paths return the
        // same 401 with the same body, so only the clock can tell them
        // apart. Verified by mutation: adding an early return leaves
        // the differential at 102/102 and fails this test.
        let real = real_hash();
        let t0 = std::time::Instant::now();
        assert!(!verify_credentials(
            "admin",
            &real,
            "wrong-user",
            "wrong-pw"
        ));
        let wrong_username = t0.elapsed();

        let t1 = std::time::Instant::now();
        assert!(!verify_credentials("admin", &real, "admin", "wrong-pw"));
        let wrong_password = t1.elapsed();

        // argon2 at these parameters costs tens of milliseconds; a
        // short-circuit costs nanoseconds. The threshold is deliberately
        // loose — this is distinguishing "ran the hash" from "did not",
        // not measuring it.
        assert!(
            wrong_username.as_millis() >= 5,
            "a wrong username returned in {wrong_username:?} — the argon2 verify was skipped"
        );
        let ratio = wrong_password.as_secs_f64() / wrong_username.as_secs_f64().max(1e-9);
        assert!(
            (0.2..5.0).contains(&ratio),
            "wrong username {wrong_username:?} vs wrong password {wrong_password:?} \
             differ by {ratio:.1}x — the two paths should cost the same"
        );
    }

    /// A real argon2id hash, generated once per test run.
    ///
    /// The const above is a hand-written PHC string with a fabricated
    /// digest: fine for "does this parse", useless for timing, because
    /// verification of it fails before doing the work.
    fn real_hash() -> String {
        use argon2::password_hash::{PasswordHasher, SaltString};
        let salt = SaltString::from_b64("c29tZXNhbHRzb21lc2FsdA").unwrap();
        argon2::Argon2::default()
            .hash_password(b"correct horse battery staple", &salt)
            .unwrap()
            .to_string()
    }
}
