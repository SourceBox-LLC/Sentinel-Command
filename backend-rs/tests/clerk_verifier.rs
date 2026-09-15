//! End-to-end tests for Clerk session verification.
//!
//! These run against a real JWKS server on localhost rather than a mocked
//! key lookup, because the parts most likely to be wrong are exactly the
//! ones a mock would paper over: `kid` selection, cache behaviour when a
//! key is missing, and whether a refresh failure signs everybody out.
//!
//! The attack cases matter more than the happy path. 75 routes sit behind
//! this, and every one of the rejections below is a way in if it stops
//! being a rejection: `alg: none`, an HS256 token keyed on the public key
//! Clerk publishes, a token from another Clerk instance, a tampered
//! payload, an expired session.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{extract::State, routing::get, Router};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use sentinel_command::auth::{AuthError, ClerkVerifier};
use serde_json::{json, Value};

const SIGNING_KEY: &str = include_str!("fixtures/test_signing_key.pem");
const JWKS: &str = include_str!("fixtures/test_jwks.json");
const KID: &str = "test-key-1";
const FRONTEND: &str = "https://app.example.com";

/// A JWKS server that counts how often it is asked, so the cache can be
/// tested rather than assumed.
#[derive(Clone)]
struct Jwks {
    body: Arc<String>,
    hits: Arc<AtomicUsize>,
    fail: Arc<AtomicUsize>,
}

async fn serve_jwks(State(state): State<Jwks>) -> (axum::http::StatusCode, String) {
    state.hits.fetch_add(1, Ordering::SeqCst);
    if state.fail.load(Ordering::SeqCst) > 0 {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "boom".to_string(),
        );
    }
    (axum::http::StatusCode::OK, (*state.body).clone())
}

struct Harness {
    issuer: String,
    hits: Arc<AtomicUsize>,
    fail: Arc<AtomicUsize>,
}

async fn start_jwks_server(body: String) -> Harness {
    let hits = Arc::new(AtomicUsize::new(0));
    let fail = Arc::new(AtomicUsize::new(0));
    let state = Jwks {
        body: Arc::new(body),
        hits: Arc::clone(&hits),
        fail: Arc::clone(&fail),
    };
    let app = Router::new()
        .route("/.well-known/jwks.json", get(serve_jwks))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    Harness {
        issuer: format!("http://{addr}"),
        hits,
        fail,
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A well-formed Clerk-shaped session token, before any tampering.
fn base_claims(issuer: &str) -> Value {
    json!({
        "sub": "user_2abc",
        "iss": issuer,
        "azp": FRONTEND,
        "exp": now() + 3600,
        "nbf": now() - 10,
        "iat": now() - 10,
        "sid": "sess_123",
        "o": {"id": "org_2xyz", "rol": "admin", "per": "read,manage_cameras", "fpm": "3"},
        "fea": "o:cameras,o:admin",
        "pla": "o:pro",
    })
}

fn sign(claims: &Value, kid: Option<&str>) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = kid.map(str::to_string);
    let key = EncodingKey::from_rsa_pem(SIGNING_KEY.as_bytes()).unwrap();
    jsonwebtoken::encode(&header, claims, &key).unwrap()
}

fn verifier(issuer: &str) -> ClerkVerifier {
    ClerkVerifier::new(
        Some(issuer.to_string()),
        FRONTEND.to_string(),
        reqwest::Client::new(),
    )
}

#[tokio::test]
async fn a_valid_token_resolves_to_its_claims() {
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);
    let token = sign(&base_claims(&h.issuer), Some(KID));

    let claims = v.verify(&token).await.expect("should verify");
    assert_eq!(claims["sub"], "user_2abc");
    assert_eq!(claims["o"]["id"], "org_2xyz");
}

#[tokio::test]
async fn the_jwks_is_fetched_once_and_then_cached() {
    // The Python service's comment records why this matters: its SDK
    // refreshed synchronously with a ten-deep retry ladder, and inline on
    // the event loop that froze every tenant during a Clerk blip. One
    // fetch per key, not one per request.
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);
    let token = sign(&base_claims(&h.issuer), Some(KID));

    for _ in 0..25 {
        v.verify(&token).await.expect("should verify");
    }
    assert_eq!(
        h.hits.load(Ordering::SeqCst),
        1,
        "25 verifications should have cost exactly one JWKS fetch"
    );
}

#[tokio::test]
async fn a_cached_key_survives_the_jwks_going_down() {
    // A Clerk outage must not become a total outage here.
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);
    let token = sign(&base_claims(&h.issuer), Some(KID));

    v.verify(&token).await.expect("warms the cache");
    h.fail.store(1, Ordering::SeqCst);

    v.verify(&token)
        .await
        .expect("a held key must keep working while the JWKS is unreachable");
}

#[tokio::test]
async fn an_unsigned_token_is_rejected() {
    // alg=none with otherwise perfect claims. The header's own `alg` is
    // never what decides how a token is verified.
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);

    let claims = base_claims(&h.issuer);
    let header = URL_SAFE_NO_PAD_encode(br#"{"alg":"none","typ":"JWT","kid":"test-key-1"}"#);
    let payload = URL_SAFE_NO_PAD_encode(serde_json::to_string(&claims).unwrap().as_bytes());
    let token = format!("{header}.{payload}.");

    assert!(matches!(
        v.verify(&token).await,
        Err(AuthError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn an_hs256_token_signed_with_the_public_key_is_rejected() {
    // The classic algorithm-confusion attack: Clerk publishes the RSA
    // public key, so if the verifier honoured `alg: HS256` it would treat
    // that public value as a shared secret anyone can sign with.
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);

    let jwks: Value = serde_json::from_str(JWKS).unwrap();
    let public_n = jwks["keys"][0]["n"].as_str().unwrap();

    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some(KID.to_string());
    let token = jsonwebtoken::encode(
        &header,
        &base_claims(&h.issuer),
        &EncodingKey::from_secret(public_n.as_bytes()),
    )
    .unwrap();

    assert!(matches!(
        v.verify(&token).await,
        Err(AuthError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn a_tampered_payload_is_rejected() {
    // Promote the caller to admin of another org by editing the payload
    // and keeping the signature.
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);
    let token = sign(&base_claims(&h.issuer), Some(KID));

    let mut parts: Vec<&str> = token.split('.').collect();
    let forged = json!({"sub": "user_2abc", "iss": h.issuer, "azp": FRONTEND,
                        "exp": now() + 3600,
                        "o": {"id": "org_SOMEONE_ELSE", "rol": "admin"}});
    let payload = URL_SAFE_NO_PAD_encode(serde_json::to_string(&forged).unwrap().as_bytes());
    parts[1] = &payload;
    let tampered = parts.join(".");

    assert!(matches!(
        v.verify(&tampered).await,
        Err(AuthError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn an_expired_token_is_rejected() {
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);

    let mut claims = base_claims(&h.issuer);
    claims["exp"] = json!(now() - 60);
    let token = sign(&claims, Some(KID));

    assert!(matches!(
        v.verify(&token).await,
        Err(AuthError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn a_token_that_is_not_yet_valid_is_rejected() {
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);

    let mut claims = base_claims(&h.issuer);
    claims["nbf"] = json!(now() + 600);
    let token = sign(&claims, Some(KID));

    assert!(matches!(
        v.verify(&token).await,
        Err(AuthError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn a_token_from_another_issuer_is_rejected() {
    // Same signing key, different `iss`. This is what a token minted by
    // a different Clerk instance looks like, and accepting it would mean
    // cross-tenant access.
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);

    let mut claims = base_claims(&h.issuer);
    claims["iss"] = json!("https://someone-else.clerk.accounts.dev");
    let token = sign(&claims, Some(KID));

    assert!(matches!(
        v.verify(&token).await,
        Err(AuthError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn a_token_minted_for_another_origin_is_rejected() {
    // `azp` is what the Python service passes as authorized_parties.
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);

    let mut claims = base_claims(&h.issuer);
    claims["azp"] = json!("https://evil.example.com");
    let token = sign(&claims, Some(KID));

    assert!(matches!(
        v.verify(&token).await,
        Err(AuthError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn a_token_with_no_azp_is_rejected() {
    // This test asserted the opposite until the Python SDK was read.
    // Clerk's docs say azp "could be omitted if, for privacy-related
    // reasons, Origin is empty or null", which reads like a reason to
    // treat it as optional. But clerk_backend_api 7.0.0 does:
    //
    //     if azp is None or azp not in options.authorized_parties:
    //         raise TokenVerificationError(...)
    //
    // and Command Center always passes authorized_parties, so a missing
    // azp is a 401 there. Accepting it here made Rust the more
    // permissive tier — the one direction a port must never drift.
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);

    let mut claims = base_claims(&h.issuer);
    claims.as_object_mut().unwrap().remove("azp");
    let token = sign(&claims, Some(KID));

    assert!(matches!(
        v.verify(&token).await,
        Err(AuthError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn azp_is_compared_exactly_not_normalised() {
    // The SDK's check is `azp not in authorized_parties` — plain string
    // membership. A trailing slash is a different origin to it, so
    // normalising here would accept what Python rejects.
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);

    for azp in [
        &format!("{FRONTEND}/"),
        &FRONTEND.replace("https://", "http://"),
        &FRONTEND.to_uppercase(),
    ] {
        let mut claims = base_claims(&h.issuer);
        claims["azp"] = json!(azp);
        let token = sign(&claims, Some(KID));
        assert!(
            matches!(v.verify(&token).await, Err(AuthError::NotAuthenticated)),
            "{azp} should not match {FRONTEND}"
        );
    }
}

#[tokio::test]
async fn the_clock_skew_allowance_matches_the_sdk() {
    // clerk_backend_api's VerifyTokenOptions.clock_skew_in_ms defaults
    // to 5000, so a token whose nbf is 3 seconds in the future is still
    // accepted and one 30 seconds out is not.
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);

    let mut ok = base_claims(&h.issuer);
    ok["nbf"] = json!(now() + 3);
    v.verify(&sign(&ok, Some(KID)))
        .await
        .expect("3s of drift is within the 5s allowance");

    let mut bad = base_claims(&h.issuer);
    bad["nbf"] = json!(now() + 30);
    assert!(v.verify(&sign(&bad, Some(KID))).await.is_err());
}

#[tokio::test]
async fn a_token_with_an_unknown_kid_is_rejected() {
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);
    let token = sign(&base_claims(&h.issuer), Some("some-other-key"));

    assert!(matches!(
        v.verify(&token).await,
        Err(AuthError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn a_token_with_no_kid_is_rejected() {
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);
    let token = sign(&base_claims(&h.issuer), None);

    assert!(matches!(
        v.verify(&token).await,
        Err(AuthError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn an_unknown_kid_does_not_hammer_the_jwks_endpoint() {
    // Otherwise a flood of tokens bearing garbage kids is a free way to
    // make this service attack Clerk on an attacker's behalf.
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);

    for i in 0..50 {
        let token = sign(&base_claims(&h.issuer), Some(&format!("junk-{i}")));
        let _ = v.verify(&token).await;
    }
    let hits = h.hits.load(Ordering::SeqCst);
    assert!(
        hits <= 2,
        "50 unknown-kid tokens caused {hits} JWKS fetches; the rate limit is not holding"
    );
}

#[tokio::test]
async fn garbage_is_rejected_without_panicking() {
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = verifier(&h.issuer);

    for token in ["", "not-a-jwt", "a.b.c", "...", "....", "eyJhbGciOiJSUzI1NiJ9"] {
        assert!(
            v.verify(token).await.is_err(),
            "should reject {token:?} without panicking"
        );
    }
}

#[tokio::test]
async fn a_verifier_with_no_issuer_fails_closed() {
    // A malformed CLERK_PUBLISHABLE_KEY leaves no issuer to trust. That
    // must reject everything, not accept everything.
    let h = start_jwks_server(JWKS.to_string()).await;
    let v = ClerkVerifier::new(None, FRONTEND.to_string(), reqwest::Client::new());
    let token = sign(&base_claims(&h.issuer), Some(KID));

    assert!(matches!(v.verify(&token).await, Err(AuthError::Failed)));
}

/// base64url without padding — the JWT encoding. Written out rather than
/// pulled from a crate for the same reason `auth.rs` hand-rolls its
/// decoder: one call site.
#[allow(non_snake_case)]
fn URL_SAFE_NO_PAD_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        let chars = [
            TABLE[(n >> 18) as usize & 63],
            TABLE[(n >> 12) as usize & 63],
            TABLE[(n >> 6) as usize & 63],
            TABLE[n as usize & 63],
        ];
        let keep = chunk.len() + 1;
        for &c in chars.iter().take(keep) {
            out.push(c as char);
        }
    }
    out
}
