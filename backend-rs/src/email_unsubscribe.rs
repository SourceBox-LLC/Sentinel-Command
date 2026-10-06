//! The signed link in every email footer.
//!
//! Ported from `backend/app/core/email_unsubscribe.py`. A click needs no
//! authentication, so the authority is the token: `(org_id, kind, rcpt)`
//! signed with a secret derived from the deployment's own, and good for
//! 400 days.
//!
//! Two things about this module are not ordinary JWT handling.
//!
//! **The token is minted by hand, not by `jsonwebtoken::encode`.** The
//! URL is substituted into `email_outbox.body_text` and `body_html`,
//! which the write differential compares column by column, so the
//! token has to be the same *bytes* Python would have written — and
//! PyJWT writes its header as `{"alg":...,"typ":...}` where
//! `jsonwebtoken` writes `{"typ":...,"alg":...}`. Different bytes,
//! different signature, every email row a difference. The claims have
//! the same problem: PyJWT's `json.dumps` defaults to
//! `ensure_ascii=True`, so a non-ASCII address is escaped rather than
//! emitted as UTF-8.
//!
//! **Verification uses the library.** Minting has one shape and has to
//! match to the byte; verifying has to accept whatever either stack
//! minted, which is a property of the decoded JSON and not of its
//! spelling — and it is the side where an algorithm-confusion bug
//! would live. `leeway` is pinned to zero because `jsonwebtoken`
//! defaults to sixty seconds and PyJWT to none.
//!
//! Failing closed is deliberate: with no secret configured the module
//! refuses to mint and rejects everything, where an earlier version of
//! the Python fell back to a hardcoded string and made this public
//! endpoint forgeable on any misconfigured deploy.

use serde::Deserialize;

use crate::audit::python_json_string;
use crate::crypto::{base64_url_nopad, hex, hmac_sha256};

/// `_TOKEN_TTL_SECONDS`. CAN-SPAM's floor is 30 days; 400 covers any
/// realistic inbox archaeology without being a forever-credential.
const TOKEN_TTL_SECONDS: i64 = 400 * 24 * 3600;

/// `_DERIVE_LABEL`. Bump the suffix to rotate every outstanding link
/// without touching the deployment's own secret.
const DERIVE_LABEL: &[u8] = b"sentinel-email-unsubscribe-v1";

/// The `sub` claim. A valid signature with the wrong subject is still
/// refused, so a future token minted from the same key cannot be
/// replayed here.
const SUBJECT: &str = "email-unsubscribe";

/// `_get_secret()`.
///
/// The base is `APP_SECRET_KEY` for a self-hosted install and
/// `CLERK_SECRET_KEY` for a hosted one — gated strictly on the auth
/// provider rather than "prefer `APP_SECRET_KEY` if set", because a
/// hosted operator setting that generically-named variable for some
/// unrelated purpose would otherwise silently invalidate every
/// outstanding link for the next 400 days with no warning.
pub fn derive_secret(base: &str) -> Option<String> {
    if base.is_empty() {
        return None;
    }
    Some(hex(&hmac_sha256(base.as_bytes(), DERIVE_LABEL)))
}

/// Which variable the base comes from, so the failure can name it.
pub fn secret_base_name(is_local_auth: bool) -> &'static str {
    if is_local_auth {
        "APP_SECRET_KEY"
    } else {
        "CLERK_SECRET_KEY"
    }
}

/// `make_token(org_id, kind, recipient)`, with the clock passed in so a
/// test can pin it.
///
/// `None` when no signing secret is configured — the Python raises, and
/// its one caller treats the raise as "this recipient gets no row".
pub fn make_token(
    secret: &str,
    org_id: &str,
    kind: &str,
    recipient: &str,
    now: i64,
) -> Option<String> {
    if secret.is_empty() {
        return None;
    }
    // PyJWT's header, in PyJWT's key order.
    let header = base64_url_nopad(br#"{"alg":"HS256","typ":"JWT"}"#);
    // `json.dumps(payload, separators=(",", ":"))` over a dict built in
    // this order.
    let claims = format!(
        r#"{{"org_id":{},"kind":{},"rcpt":{},"iat":{now},"exp":{},"sub":{}}}"#,
        python_json_string(org_id),
        python_json_string(kind),
        python_json_string(&recipient.trim().to_lowercase()),
        now + TOKEN_TTL_SECONDS,
        python_json_string(SUBJECT),
    );
    let claims = base64_url_nopad(claims.as_bytes());
    let signed = format!("{header}.{claims}");
    let signature = base64_url_nopad(&hmac_sha256(secret.as_bytes(), signed.as_bytes()));
    Some(format!("{signed}.{signature}"))
}

#[derive(Debug, Deserialize)]
struct Claims {
    #[serde(default)]
    org_id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    rcpt: String,
    #[serde(default)]
    sub: String,
}

/// `verify_token(token)` → `(org_id, kind, recipient)`.
///
/// `None` on anything wrong: bad signature, expired, malformed, a
/// missing or empty claim, the wrong subject, or no secret configured.
/// The Python logs each at INFO rather than WARN — after a secret
/// rotation a stream of these is expected, not an attack.
pub fn verify_token(secret: &str, token: &str) -> Option<(String, String, String)> {
    if token.is_empty() {
        return None;
    }
    if secret.is_empty() {
        tracing::info!("[Unsubscribe] no signing secret configured — rejecting");
        return None;
    }

    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    // PyJWT allows no slack at all; jsonwebtoken allows a minute.
    validation.leeway = 0;
    // These tokens carry no audience, and an unconfigured audience
    // check is not something to leave to a default.
    validation.validate_aud = false;
    validation.set_required_spec_claims(&["exp"]);

    let decoded = jsonwebtoken::decode::<Claims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    );
    let claims = match decoded {
        Ok(data) => data.claims,
        Err(err) => {
            tracing::info!(error = ?err.kind(), "[Unsubscribe] token verify failed");
            return None;
        }
    };

    if claims.sub != SUBJECT {
        tracing::info!("[Unsubscribe] token sub mismatch");
        return None;
    }
    if claims.org_id.is_empty() || claims.kind.is_empty() || claims.rcpt.is_empty() {
        return None;
    }
    Some((claims.org_id, claims.kind, claims.rcpt))
}

/// `build_unsubscribe_url(org_id, kind, recipient)`.
pub fn build_unsubscribe_url(
    secret: &str,
    frontend_url: &str,
    org_id: &str,
    kind: &str,
    recipient: &str,
    now: i64,
) -> Option<String> {
    let token = make_token(secret, org_id, kind, recipient, now)?;
    let base = frontend_url.trim_end_matches('/');
    Some(format!(
        "{base}/api/notifications/email/unsubscribe?t={token}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLERK: &str = "sk_test_deadbeef";

    fn secret(base: &str) -> String {
        derive_secret(base).unwrap()
    }

    /// The derived secret, from `hmac.new(base, label, sha256).hexdigest()`.
    #[test]
    fn the_secret_is_derived_not_used_raw() {
        assert_eq!(
            secret(CLERK),
            "be0f1eb3f090e14614cf8f419729ec05938fe23554b30154bfc17e5abbe9151d"
        );
        // A base longer than the HMAC block size still derives.
        assert_eq!(
            secret(&"k".repeat(100)),
            "5682742a575373e5dcfa11cb9f42c690c98bf762638e75d7a771ad5872746b5a"
        );
        // Nothing configured is not "sign with the empty string".
        assert_eq!(derive_secret(""), None);
        assert_eq!(make_token("", "org", "kind", "a@b.c", 0), None);
    }

    /// Byte for byte what PyJWT produced for the same inputs. These
    /// strings land inside an `email_outbox` column the differential
    /// compares literally.
    #[test]
    fn tokens_match_pyjwts_bytes() {
        for (base, org, kind, rcpt, now, want) in [
            (
                CLERK, "org_abc", "camera_offline",
                // Stripped and folded to lower case before signing.
                " Alice@Example.COM ", 1_700_000_000i64,
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJvcmdfaWQiOiJvcmdfYWJjIiwia2luZCI6ImNhbWVyYV9vZmZsaW5lIiwicmNwdCI6ImFsaWNlQGV4YW1wbGUuY29tIiwiaWF0IjoxNzAwMDAwMDAwLCJleHAiOjE3MzQ1NjAwMDAsInN1YiI6ImVtYWlsLXVuc3Vic2NyaWJlIn0.n8UZEQAQC7qgSyalnh-nST5cv8SN9pRBYLX9MHqDROw",
            ),
            (
                CLERK, "self-host", "motion", "a@b.c", 1,
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJvcmdfaWQiOiJzZWxmLWhvc3QiLCJraW5kIjoibW90aW9uIiwicmNwdCI6ImFAYi5jIiwiaWF0IjoxLCJleHAiOjM0NTYwMDAxLCJzdWIiOiJlbWFpbC11bnN1YnNjcmliZSJ9.FVBa1ly__6E3jfd0yYFVfx7mgFcTNRRFgT2wgNjjI-E",
            ),
            (
                &"k".repeat(100), "org_x", "welcome", "", 1_758_412_800,
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJvcmdfaWQiOiJvcmdfeCIsImtpbmQiOiJ3ZWxjb21lIiwicmNwdCI6IiIsImlhdCI6MTc1ODQxMjgwMCwiZXhwIjoxNzkyOTcyODAwLCJzdWIiOiJlbWFpbC11bnN1YnNjcmliZSJ9.XaLrV3f4EbWirXbNzpRcoPetCSWV4joXzctFVxgb0B8",
            ),
        ] {
            assert_eq!(
                make_token(&secret(base), org, kind, rcpt, now).unwrap(),
                want,
                "{org}/{kind}"
            );
        }
    }

    #[test]
    fn a_token_round_trips() {
        let secret = secret(CLERK);
        let now = chrono::Utc::now().timestamp();
        let token = make_token(&secret, "org_abc", "camera_offline", "A@b.C", now).unwrap();
        assert_eq!(
            verify_token(&secret, &token),
            Some(("org_abc".into(), "camera_offline".into(), "a@b.c".into()))
        );
    }

    #[test]
    fn a_token_signed_with_another_secret_is_refused() {
        let now = chrono::Utc::now().timestamp();
        let token = make_token(&secret(CLERK), "org_abc", "motion", "a@b.c", now).unwrap();
        assert_eq!(verify_token(&secret("sk_test_other"), &token), None);
        // And the raw base never signs anything, so it cannot verify.
        assert_eq!(verify_token(CLERK, &token), None);
        assert_eq!(verify_token("", &token), None);
        assert_eq!(verify_token(&secret(CLERK), ""), None);
    }

    #[test]
    fn an_expired_token_is_refused_with_no_slack() {
        let secret = secret(CLERK);
        let now = chrono::Utc::now().timestamp();
        // One second past the TTL. jsonwebtoken would allow a minute of
        // slack by default and PyJWT allows none, so this is the case
        // where the library's default would have diverged.
        let token = make_token(
            &secret,
            "org",
            "motion",
            "a@b.c",
            now - TOKEN_TTL_SECONDS - 1,
        );
        assert_eq!(verify_token(&secret, &token.unwrap()), None);
        // A second inside it still verifies.
        let token = make_token(
            &secret,
            "org",
            "motion",
            "a@b.c",
            now - TOKEN_TTL_SECONDS + 5,
        );
        assert!(verify_token(&secret, &token.unwrap()).is_some());
    }

    #[test]
    fn a_valid_signature_with_the_wrong_subject_is_refused() {
        let secret = secret(CLERK);
        let exp = chrono::Utc::now().timestamp() + 3600;
        // Minted the same way, but claiming to be something else.
        let header = base64_url_nopad(br#"{"alg":"HS256","typ":"JWT"}"#);
        for claims in [
            format!(
                r#"{{"org_id":"o","kind":"k","rcpt":"a@b.c","exp":{exp},"sub":"password-reset"}}"#
            ),
            // A missing claim is as bad as a wrong one.
            format!(r#"{{"kind":"k","rcpt":"a@b.c","exp":{exp},"sub":"email-unsubscribe"}}"#),
            format!(r#"{{"org_id":"o","rcpt":"a@b.c","exp":{exp},"sub":"email-unsubscribe"}}"#),
            format!(r#"{{"org_id":"o","kind":"k","exp":{exp},"sub":"email-unsubscribe"}}"#),
            // An empty one too — the Python's check is falsiness.
            format!(
                r#"{{"org_id":"","kind":"k","rcpt":"a@b.c","exp":{exp},"sub":"email-unsubscribe"}}"#
            ),
            // No exp at all.
            r#"{"org_id":"o","kind":"k","rcpt":"a@b.c","sub":"email-unsubscribe"}"#.to_string(),
        ] {
            let signed = format!("{header}.{}", base64_url_nopad(claims.as_bytes()));
            let sig = base64_url_nopad(&hmac_sha256(secret.as_bytes(), signed.as_bytes()));
            assert_eq!(
                verify_token(&secret, &format!("{signed}.{sig}")),
                None,
                "{claims}"
            );
        }
    }

    #[test]
    fn a_non_ascii_address_is_escaped_the_way_json_dumps_escapes_it() {
        // PyJWT's json.dumps defaults to ensure_ascii=True, so the
        // claims segment holds `é` and not the UTF-8 bytes.
        assert_eq!(
            make_token(&secret(CLERK), "org", "motion", "café@x.test", 0).unwrap(),
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJvcmdfaWQiOiJvcmciLCJraW5kIjoibW90aW9uIiwicmNwdCI6ImNhZlx1MDBlOUB4LnRlc3QiLCJpYXQiOjAsImV4cCI6MzQ1NjAwMDAsInN1YiI6ImVtYWlsLXVuc3Vic2NyaWJlIn0.C51h658RS3d0-ERXLxYVugaOO36EE4p1hjQc9RYNvOg"
        );
    }

    #[test]
    fn the_url_carries_the_token_and_one_slash() {
        let secret = secret(CLERK);
        for frontend in [
            "https://example.test",
            "https://example.test/",
            "https://example.test///",
        ] {
            let url =
                build_unsubscribe_url(&secret, frontend, "org", "motion", "a@b.c", 0).unwrap();
            assert!(
                url.starts_with("https://example.test/api/notifications/email/unsubscribe?t="),
                "{url}"
            );
        }
        assert_eq!(
            build_unsubscribe_url("", "https://x", "o", "k", "a@b.c", 0),
            None
        );
    }
}
