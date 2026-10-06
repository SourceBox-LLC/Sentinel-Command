//! Local (self-hosted) session tokens.
//!
//! Used only when `AUTH_PROVIDER=local`: one fixed admin account, no
//! Clerk, no invite flow, no multi-org. Tokens are HS256 JWTs signed with
//! `APP_SECRET_KEY`.
//!
//! Ported from `backend/app/core/local_auth.py`. The 30-day lifetime is
//! deliberate and documented there: this is a camera dashboard left open
//! on a wall display, and a short expiry would log it out the next time a
//! long-lived connection reconnected.

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use super::claims::AuthUser;

const TOKEN_TTL_SECONDS: i64 = 30 * 24 * 3600;
pub const LOCAL_USER_ID: &str = "local-admin";

/// The `sub` every local token carries. Checked on the way back in so a
/// JWT signed with `APP_SECRET_KEY` for some other purpose — the email
/// unsubscribe links use the same key — cannot be replayed as a session.
const JWT_SUBJECT: &str = "sentinel-local-auth";

#[derive(Debug, Serialize, Deserialize)]
pub struct LocalClaims {
    pub sub: String,
    pub user_id: String,
    pub org_id: String,
    pub org_role: String,
    pub iat: i64,
    pub exp: i64,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Mint a fresh session token for the local admin.
pub fn issue_token(secret: &str, org_id: &str) -> Result<String, jsonwebtoken::errors::Error> {
    let iat = now();
    let claims = LocalClaims {
        sub: JWT_SUBJECT.to_string(),
        user_id: LOCAL_USER_ID.to_string(),
        org_id: org_id.to_string(),
        org_role: "org:admin".to_string(),
        iat,
        exp: iat + TOKEN_TTL_SECONDS,
    };
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
}

/// Verify a local session token, returning its claims.
///
/// `None` on any failure — expired, bad signature, malformed, wrong
/// subject, or no secret configured. The caller turns that into a 401
/// without distinguishing the cases, exactly as the Python does: telling
/// a caller *why* their token failed tells an attacker the same thing.
pub fn verify_token(secret: &str, token: &str) -> Option<LocalClaims> {
    if token.is_empty() || secret.is_empty() {
        return None;
    }

    let mut validation = Validation::new(Algorithm::HS256);
    // Python passes options={"require": [...]}; these are the same
    // fields, and every one of them is non-optional in `LocalClaims`, so
    // a token missing any of them fails to deserialise.
    validation.set_required_spec_claims(&["sub", "exp"]);
    // The Python service does not set an audience, and pyjwt does not
    // verify one unless asked.
    validation.validate_aud = false;

    let data = jsonwebtoken::decode::<LocalClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .ok()?;

    if data.claims.sub != JWT_SUBJECT {
        return None;
    }
    Some(data.claims)
}

/// The `AuthUser` a verified local token resolves to.
///
/// Fully unlocked: the self-hosted tier has no billing or licensing gate.
/// `admin` unlocks the paid-tier feature checks in `audit.py` and
/// `cameras.py`; `cameras` mirrors the wire-visible flag Clerk sends.
pub fn auth_user_for(claims: &LocalClaims, email: &str, username: &str) -> AuthUser {
    AuthUser {
        user_id: claims.user_id.clone(),
        org_id: claims.org_id.clone(),
        org_role: claims.org_role.clone(),
        org_permissions: Vec::new(),
        email: email.to_string(),
        username: username.to_string(),
        plan: "self_host".to_string(),
        features: vec!["admin".to_string(), "cameras".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-secret-not-used-anywhere-real";

    #[test]
    fn a_freshly_issued_token_verifies() {
        let token = issue_token(SECRET, "self-host").unwrap();
        let claims = verify_token(SECRET, &token).expect("should verify");
        assert_eq!(claims.user_id, LOCAL_USER_ID);
        assert_eq!(claims.org_id, "self-host");
        assert_eq!(claims.org_role, "org:admin");
    }

    #[test]
    fn the_resolved_user_is_an_unlocked_admin() {
        let token = issue_token(SECRET, "self-host").unwrap();
        let claims = verify_token(SECRET, &token).unwrap();
        let user = auth_user_for(&claims, "a@b.c", "admin");
        assert!(user.is_admin());
        assert!(user.can_view_cameras());
        assert_eq!(user.plan, "self_host");
        assert_eq!(user.features, vec!["admin", "cameras"]);
    }

    #[test]
    fn a_token_signed_with_another_secret_is_rejected() {
        let token = issue_token("some-other-secret", "self-host").unwrap();
        assert!(verify_token(SECRET, &token).is_none());
    }

    #[test]
    fn a_token_for_another_purpose_is_not_a_session() {
        // Same key, same algorithm, different `sub` — this is what an
        // email unsubscribe token looks like, and it must not be usable
        // as a login.
        #[derive(serde::Serialize)]
        struct Other {
            sub: &'static str,
            user_id: &'static str,
            org_id: &'static str,
            org_role: &'static str,
            iat: i64,
            exp: i64,
        }
        let token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &Other {
                sub: "email-unsubscribe",
                user_id: LOCAL_USER_ID,
                org_id: "self-host",
                org_role: "org:admin",
                iat: now(),
                exp: now() + 3600,
            },
            &EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap();
        assert!(verify_token(SECRET, &token).is_none());
    }

    #[test]
    fn an_expired_token_is_rejected() {
        let iat = now() - 10_000;
        let token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &LocalClaims {
                sub: JWT_SUBJECT.to_string(),
                user_id: LOCAL_USER_ID.to_string(),
                org_id: "self-host".to_string(),
                org_role: "org:admin".to_string(),
                iat,
                exp: iat + 60,
            },
            &EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap();
        assert!(verify_token(SECRET, &token).is_none());
    }

    #[test]
    fn nothing_verifies_without_a_configured_secret() {
        // An install that forgot APP_SECRET_KEY must reject everyone,
        // not accept everyone.
        let token = issue_token(SECRET, "self-host").unwrap();
        assert!(verify_token("", &token).is_none());
        assert!(verify_token(SECRET, "").is_none());
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        for token in ["not-a-jwt", "a.b.c", "...", "eyJhbGciOiJIUzI1NiJ9"] {
            assert!(verify_token(SECRET, token).is_none());
        }
    }

    #[test]
    fn an_unsigned_token_is_rejected() {
        // alg=none with the right claims — the classic JWT bypass.
        let token = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.\
                     eyJzdWIiOiJzZW50aW5lbC1sb2NhbC1hdXRoIiwidXNlcl9pZCI6ImxvY2FsLWFkbWluIiwi\
                     b3JnX2lkIjoic2VsZi1ob3N0Iiwib3JnX3JvbGUiOiJvcmc6YWRtaW4iLCJpYXQiOjAsImV4\
                     cCI6OTk5OTk5OTk5OX0.";
        assert!(verify_token(SECRET, token).is_none());
    }
}
