//! Inbound webhooks. Only Resend's is ported; Clerk's reaches the org
//! membership lookup, the email outbox and the HLS caches.
//!
//! Resend signs with Svix, so the signature is the whole security
//! boundary: without it, anyone who learns the URL can forge a bounce
//! for any address and have this service stop emailing them.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::app::AppState;
use crate::error::ApiError;
use crate::models::now_naive;
use crate::ratelimit::PerMinute;

/// How far a signed timestamp may be from now, in seconds. Svix's
/// default, and the reason a captured request cannot be replayed
/// tomorrow.
const TOLERANCE_SECONDS: i64 = 5 * 60;

/// Which Resend events suppress an address, and the reason recorded.
fn suppression_reason(event_type: &str) -> Option<&'static str> {
    match event_type {
        "email.bounced" => Some("bounce"),
        "email.complained" => Some("complaint"),
        _ => None,
    }
}

/// Verify a Svix signature over the raw body.
///
/// `v1,<base64 HMAC-SHA256>` of `{id}.{timestamp}.{payload}`, keyed by
/// the secret with its `whsec_` prefix stripped and the rest base64
/// decoded. The header carries space-separated signatures so a secret
/// can be rotated without dropping deliveries — any one matching is
/// enough, and each is compared in constant time.
///
/// Both header spellings are read. Svix accepts `svix-*` and
/// `webhook-*`, and the Python handler reads both for its idempotency
/// key; verifying only one spelling would refuse deliveries the Python
/// accepts.
pub fn verify_svix(secret: &str, headers: &HeaderMap, body: &[u8], now: i64) -> bool {
    let header = |a: &str, b: &str| {
        headers
            .get(a)
            .or_else(|| headers.get(b))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    };
    let id = header("svix-id", "webhook-id");
    let timestamp = header("svix-timestamp", "webhook-timestamp");
    let signature = header("svix-signature", "webhook-signature");
    if id.is_empty() || timestamp.is_empty() || signature.is_empty() {
        return false;
    }
    let Ok(ts) = timestamp.parse::<i64>() else {
        return false;
    };
    if (now - ts).abs() > TOLERANCE_SECONDS {
        return false;
    }

    let key = match secret.strip_prefix("whsec_") {
        Some(rest) => base64_decode(rest),
        None => base64_decode(secret),
    };
    let Some(key) = key else { return false };

    let mut signed = Vec::with_capacity(id.len() + timestamp.len() + body.len() + 2);
    signed.extend_from_slice(id.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(timestamp.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(body);
    let expected = base64_encode(&hmac_sha256(&key, &signed));

    signature.split(' ').any(|part| {
        part.strip_prefix("v1,")
            .is_some_and(|sig| sig.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1)
    })
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut key = key.to_vec();
    if key.len() > 64 {
        key = Sha256::digest(&key).to_vec();
    }
    key.resize(64, 0);
    let ipad: Vec<u8> = key.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = key.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha256::digest([&ipad[..], message].concat());
    Sha256::digest([&opad[..], &inner[..]].concat()).to_vec()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for ch in input.bytes() {
        if ch == b'=' {
            break;
        }
        let val = B64.iter().position(|&c| c == ch)? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// `POST /api/webhooks/resend` — delivery events.
pub async fn resend_webhook(
    rate: PerMinute<600>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    // No dependency runs before the Python handler, so every request
    // spends a slot, including one that is about to be refused. The
    // limit is generous because Resend bursts a campaign's events.
    rate.check().await?;

    let Some(secret) = state.config.resend_webhook_secret.as_deref() else {
        // Refusing is the only safe answer: without the secret a
        // forged bounce could suppress any address.
        tracing::error!("RESEND_WEBHOOK_SECRET not set — cannot verify Resend signatures");
        return Err(ApiError::bad_request("Webhook processing unavailable"));
    };
    if !verify_svix(secret, &headers, &body, chrono::Utc::now().timestamp()) {
        return Err(ApiError::bad_request("Invalid signature"));
    }
    // Parsed after verification, and a non-object body is as malformed
    // as unparseable JSON — `event.get(...)` would raise otherwise.
    let Ok(Value::Object(event)) = serde_json::from_slice::<Value>(&body) else {
        return Err(ApiError::bad_request("Malformed payload"));
    };

    // `event.get("type") or ""` and `event.get("data") or {}` keep any
    // *truthy* value, whatever its type. A truthy non-object `data` is
    // carried along and only fails when a branch calls `data.get(...)`,
    // so it is a 500 for the events that read it and harmless for the
    // rest.
    let type_value = event.get("type").cloned().unwrap_or(Value::Null);
    let event_type = if crate::pyrepr::truthy(&type_value) {
        type_value.as_str().map(str::to_string)
    } else {
        Some(String::new())
    };
    let data_value = event.get("data").cloned().unwrap_or(Value::Null);
    let data: Option<serde_json::Map<String, Value>> = match data_value {
        v if !crate::pyrepr::truthy(&v) => Some(serde_json::Map::new()),
        Value::Object(map) => Some(map),
        _ => None,
    };

    // Idempotency. Resend retries, and a retried bounce that
    // re-suppressed an address would be harmless — but a retried
    // event that re-ran the outbox update would not stay harmless
    // forever, and Clerk's handler dedups the same way.
    let msg_id = headers
        .get("svix-id")
        .or_else(|| headers.get("webhook-id"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !msg_id.is_empty() {
        let seen: Option<(String,)> =
            sqlx::query_as("SELECT COALESCE(event_type, '') FROM processed_webhooks WHERE svix_msg_id = $1")
                .bind(&msg_id)
                .fetch_optional(&state.pool)
                .await?;
        if let Some((previous,)) = seen {
            tracing::info!(msg_id, previous, "resend webhook already processed — skipping");
            return Ok(Json(json!({ "status": "duplicate", "svix_id": msg_id })));
        }
    }

    // `event_type in _RESEND_SUPPRESSION_EVENTS` hashes event_type, and a
    // list or object is unhashable: TypeError, a 500 — after the dedup
    // check above, so a retried delivery with such a type still answers
    // "duplicate".
    if matches!(type_value, Value::Array(_) | Value::Object(_)) && crate::pyrepr::truthy(&type_value) {
        return Err(ApiError::internal("resend event type is unhashable"));
    }
    let event_type_text = event_type.clone().unwrap_or_default();
    if let Some(reason) = suppression_reason(&event_type_text) {
        let Some(data) = data.as_ref() else {
            return Err(ApiError::internal("resend event data is not an object"));
        };
        for address in extract_addresses(data) {
            insert_suppression(&state, &address, reason, "resend_webhook").await;
        }
        // Informational: the suppression list is what stops the next
        // send. Marking the originating row keeps the outbox readable.
        if let Some(email_id) = data.get("email_id").and_then(Value::as_str) {
            let error = format!("webhook_event:{event_type_text}:reason={reason}");
            if let Err(err) = sqlx::query(
                "UPDATE email_outbox SET status = 'suppressed', error = $1
                  WHERE resend_message_id = $2 AND status = 'sent'",
            )
            .bind(&error)
            .bind(email_id)
            .execute(&state.pool)
            .await
            {
                tracing::error!(error = %err, email_id, "failed to mark outbox row suppressed");
            }
        }
    } else if event_type_text == "email.delivered" {
        let Some(data) = data.as_ref() else {
            return Err(ApiError::internal("resend event data is not an object"));
        };
        if let Some(email_id) = data.get("email_id").and_then(Value::as_str) {
            tracing::info!(email_id, "resend delivered confirmation");
        }
    }

    // A truthy non-string `type` is stored as Postgres casts it —
    // `5` becomes '5', `true` becomes 'true' — and a list or object
    // cannot be bound at all, so Python's insert fails, is rolled back,
    // and the delivery is simply not recorded.
    let stored_type = match &type_value {
        _ if event_type.is_some() => event_type.clone(),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    if let (false, Some(event_type)) = (msg_id.is_empty(), stored_type) {
        // A raced insert is benign: the other request processed it.
        if let Err(err) = sqlx::query(
            "INSERT INTO processed_webhooks (svix_msg_id, event_type, processed_at)
             VALUES ($1, $2, $3)",
        )
        .bind(&msg_id)
        .bind(&event_type)
        .bind(now_naive())
        .execute(&state.pool)
        .await
        {
            tracing::info!(error = %err, msg_id, "resend webhook dedup insert raced — ignoring");
        }
    }

    Ok(Json(json!({ "received": true })))
}

/// Recipients out of an event payload.
///
/// Resend sends `to` as a list or a bare string depending on the event
/// and SDK version, and sometimes not at all — in which case nobody is
/// suppressed, which is better than suppressing the wrong address.
fn extract_addresses(data: &serde_json::Map<String, Value>) -> Vec<String> {
    match data.get("to") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .filter(|a| a.contains('@'))
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) if s.contains('@') => vec![s.clone()],
        _ => vec![],
    }
}

/// Suppress an address, ignoring a duplicate.
///
/// The address is already on the list, which is the outcome either way.
async fn insert_suppression(state: &AppState, address: &str, reason: &str, source: &str) {
    let addr = address.trim().to_lowercase();
    if addr.is_empty() || !addr.contains('@') {
        return;
    }
    let result = sqlx::query(
        "INSERT INTO email_suppression (address, reason, source, created_at)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&addr)
    .bind(reason)
    .bind(source)
    .bind(now_naive())
    .execute(&state.pool)
    .await;
    match result {
        Ok(_) => tracing::info!(reason, source, "suppressed an address"),
        Err(err) => tracing::debug!(error = %err, "suppression insert failed (likely duplicate)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    // Signature produced by the svix library itself, for
    // id=msg_test, timestamp=1700000000, body={"type":"email.bounced"},
    // secret=whsec_c2VjcmV0LWtleS1mb3ItdGVzdGluZw==.
    const BODY: &[u8] = br#"{"type":"email.bounced"}"#;
    const SECRET: &str = "whsec_c2VjcmV0LWtleS1mb3ItdGVzdGluZw==";
    const SIG: &str = "v1,8wuNSx2MbKoFjWXkMHUCuOParUarbJUOBht5eWxihcw=";

    #[test]
    fn a_valid_signature_verifies() {
        let h = headers(&[
            ("svix-id", "msg_test"),
            ("svix-timestamp", "1700000000"),
            ("svix-signature", SIG),
        ]);
        assert!(verify_svix(SECRET, &h, BODY, 1_700_000_000));
        // Either header spelling.
        let h2 = headers(&[
            ("webhook-id", "msg_test"),
            ("webhook-timestamp", "1700000000"),
            ("webhook-signature", SIG),
        ]);
        assert!(verify_svix(SECRET, &h2, BODY, 1_700_000_000));
        // One of several, as during a secret rotation.
        let h3 = headers(&[
            ("svix-id", "msg_test"),
            ("svix-timestamp", "1700000000"),
            ("svix-signature", &format!("v1,AAAA= {SIG}")),
        ]);
        assert!(verify_svix(SECRET, &h3, BODY, 1_700_000_000));
    }

    #[test]
    fn anything_tampered_with_fails() {
        let h = headers(&[
            ("svix-id", "msg_test"),
            ("svix-timestamp", "1700000000"),
            ("svix-signature", SIG),
        ]);
        // A changed body, id, or key.
        assert!(!verify_svix(SECRET, &h, br#"{"type":"email.delivered"}"#, 1_700_000_000));
        assert!(!verify_svix("whsec_b3RoZXI=", &h, BODY, 1_700_000_000));
        let wrong_id = headers(&[
            ("svix-id", "msg_other"),
            ("svix-timestamp", "1700000000"),
            ("svix-signature", SIG),
        ]);
        assert!(!verify_svix(SECRET, &wrong_id, BODY, 1_700_000_000));
    }

    #[test]
    fn a_signature_expires_in_both_directions() {
        let h = headers(&[
            ("svix-id", "msg_test"),
            ("svix-timestamp", "1700000000"),
            ("svix-signature", SIG),
        ]);
        assert!(verify_svix(SECRET, &h, BODY, 1_700_000_000 + 299));
        assert!(!verify_svix(SECRET, &h, BODY, 1_700_000_000 + 301));
        // A timestamp in the future is refused too: accepting one
        // would let a captured request be held and replayed later.
        assert!(verify_svix(SECRET, &h, BODY, 1_700_000_000 - 299));
        assert!(!verify_svix(SECRET, &h, BODY, 1_700_000_000 - 301));
    }

    #[test]
    fn missing_headers_are_refused_rather_than_skipped() {
        for pairs in [
            vec![("svix-timestamp", "1700000000"), ("svix-signature", SIG)],
            vec![("svix-id", "msg_test"), ("svix-signature", SIG)],
            vec![("svix-id", "msg_test"), ("svix-timestamp", "1700000000")],
            vec![],
        ] {
            assert!(!verify_svix(SECRET, &headers(&pairs), BODY, 1_700_000_000));
        }
        // A non-numeric timestamp is a refusal, not a parse that
        // defaults to zero.
        let bad = headers(&[
            ("svix-id", "msg_test"),
            ("svix-timestamp", "not-a-number"),
            ("svix-signature", SIG),
        ]);
        assert!(!verify_svix(SECRET, &bad, BODY, 1_700_000_000));
    }

    #[test]
    fn base64_round_trips() {
        for raw in [&b""[..], b"a", b"ab", b"abc", b"abcd", &[0u8, 255, 16][..]] {
            let encoded = base64_encode(raw);
            assert_eq!(base64_decode(&encoded).as_deref(), Some(raw), "{encoded}");
        }
    }

    #[test]
    fn only_addresses_are_extracted() {
        let map = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(
            extract_addresses(&map(json!({"to": ["a@b.com", "nope", 5]}))),
            vec!["a@b.com"]
        );
        assert_eq!(extract_addresses(&map(json!({"to": "a@b.com"}))), vec!["a@b.com"]);
        assert!(extract_addresses(&map(json!({"to": "nope"}))).is_empty());
        assert!(extract_addresses(&map(json!({}))).is_empty());
        assert!(extract_addresses(&map(json!({"to": 5}))).is_empty());
    }
}
