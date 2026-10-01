//! The Resend transport: one send, and the SDK's error contract.
//!
//! Ported from `backend/app/core/email.py`. `send_email` never fails —
//! every outcome, including a kill switch that is off and a Resend 5xx,
//! comes back as an `EmailSendResult` the worker decides on.
//!
//! The `error` string is load-bearing and is not free-form. It is
//! `f"{type(exc).__name__}: {exc}"` over whatever the Resend SDK
//! raised, and it is stored on the outbox row and the email log, where
//! the differential compares it character for character. So this
//! reproduces the SDK's mapping from (HTTP status, body `name`) to an
//! exception class, which was read off the SDK and then confirmed by
//! driving the real thing against a fake server:
//!
//! ```text
//!   200 {"id": ...}                  -> ok
//!   200 {}                           -> resend_no_message_id
//!   422 {"name": "validation_error"} -> ValidationError: <message>
//!   500 {"name":"application_error"} -> ApplicationError: <message>
//!   429 {"name":"rate_limit_...   "} -> RateLimitError: <message>
//!   401 {"name": "invalid_api_key"}  -> ResendError: <message>
//!   200 with an unparseable body     -> ApplicationError: Failed to decode JSON response
//!   502 with an unparseable body     -> ResendError: Failed to decode JSON response
//! ```
//!
//! The last two are the same code path: the status is kept when it is
//! 4xx or 5xx and replaced with 500 when it looks successful, and 401
//! lands on the bare `ResendError` because the SDK's table maps
//! `invalid_api_key` under 403 only.

use serde_json::{json, Value};

use crate::config::Config;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct EmailSendResult {
    pub ok: bool,
    /// The kill switch was off: nothing was sent, and the caller should
    /// treat the row as done rather than retry it forever.
    pub skipped: bool,
    pub message_id: Option<String>,
    pub error: Option<String>,
}

/// `raise_for_code_and_type` — the SDK's (code, type) -> class table.
///
/// An unknown code, or a known code with an unmapped type, is a bare
/// `ResendError`.
fn resend_error_class(code: i64, error_type: &str) -> &'static str {
    match (code, error_type) {
        (400, "validation_error") => "ValidationError",
        (422, "missing_required_field" | "missing_required_fields") => "MissingRequiredFieldsError",
        (422, "validation_error") => "ValidationError",
        (401, "missing_api_key") => "MissingApiKeyError",
        (403, "invalid_api_key") => "InvalidApiKeyError",
        (429, "rate_limit_exceeded" | "daily_quota_exceeded" | "monthly_quota_exceeded") => {
            "RateLimitError"
        }
        (500, "application_error") => "ApplicationError",
        _ => "ResendError",
    }
}

fn sdk_error(code: i64, error_type: &str, message: &str) -> String {
    format!("{}: {message}", resend_error_class(code, error_type))
}

/// `_redact` — `alice@example.com` becomes `a***@example.com`.
///
/// Enough to tell one user from another in a log without putting the
/// address itself into Sentry.
pub fn redact(address: &str) -> String {
    let Some((local, domain)) = address.split_once('@') else {
        return "***".to_string();
    };
    match local.chars().next() {
        None => format!("***@{domain}"),
        Some(first) => format!("{first}***@{domain}"),
    }
}

/// One message, as Python's keyword-only signature describes it.
pub struct OutgoingEmail<'a> {
    pub to: &'a str,
    pub subject: &'a str,
    pub body_text: &'a str,
    /// Sent as the HTML part, with `body_text` as the plain-text
    /// alternative. Both go because a pure-HTML message scores worse
    /// with some spam filters.
    pub body_html: &'a str,
    /// The notification kind, which Resend shows as a tag.
    pub kind: &'a str,
    /// Shared by every retry of one outbox row, so a retry of an
    /// attempt that secretly succeeded does not deliver twice.
    pub idempotency_key: &'a str,
}

/// `send_email(...)` — one message through Resend.
pub async fn send_email(
    config: &Config,
    client: &reqwest::Client,
    email: &OutgoingEmail<'_>,
) -> EmailSendResult {
    let OutgoingEmail {
        to,
        subject,
        body_text,
        body_html,
        kind,
        idempotency_key,
    } = *email;
    // Defence in depth: a row can reach the outbox while the switch is
    // off, and the transport refuses it rather than trusting the
    // producer to have checked.
    if !config.email_enabled {
        tracing::info!(
            kind,
            to = %to,
            subject = %subject.chars().take(80).collect::<String>(),
            "EMAIL_ENABLED=false — would have sent"
        );
        return EmailSendResult {
            ok: true,
            skipped: true,
            ..Default::default()
        };
    }

    if !config.is_email_configured() {
        // Distinct from an outage: retrying will not help until an
        // operator fixes the secret, and the worker gives up at once.
        return EmailSendResult {
            ok: false,
            error: Some(
                "resend_unconfigured: RESEND_API_KEY or EMAIL_FROM_ADDRESS missing".to_string(),
            ),
            ..Default::default()
        };
    }

    let from_field = if config.email_from_name.is_empty() {
        config.email_from_address.clone()
    } else {
        format!("{} <{}>", config.email_from_name, config.email_from_address)
    };
    let payload = json!({
        "from": from_field,
        "to": [to],
        "subject": subject,
        "text": body_text,
        "html": body_html,
        "tags": [
            {"name": "event", "value": kind},
            {"name": "source", "value": "command_center"},
        ],
    });

    // No Reply-To by design: these are no-reply senders, and support is
    // a separate channel.
    //
    // The idempotency key goes in the HTTP header, not the message. In
    // the SDK that is the difference between `options` and the payload's
    // `headers` — the latter becomes an SMTP header on the outgoing
    // mail, and Resend then sends a fresh message on every retry. The
    // failure is silent, which is what makes it worth a comment.
    let url = format!("{}/emails", config.resend_api_url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .bearer_auth(&config.resend_api_key)
        .header("Idempotency-Key", idempotency_key)
        .json(&payload)
        .send()
        .await;

    let response = match response {
        Ok(response) => response,
        Err(err) => {
            // The SDK wraps a transport failure as ResendError(code=500,
            // error_type="HttpClientError"), and `str(exc)` is the
            // message alone.
            tracing::warn!(kind, to = %redact(to), error = %err, "Resend send failed");
            return EmailSendResult {
                ok: false,
                error: Some(format!("ResendError: {err}")),
                ..Default::default()
            };
        }
    };

    let status = response.status().as_u16() as i64;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // "Keep the status for 4xx/5xx; fall back to 500 if the status
    // looks successful but the body is not."
    let error_code = if status >= 400 { status } else { 500 };

    if !content_type.contains("application/json") {
        return EmailSendResult {
            ok: false,
            error: Some(sdk_error(
                error_code,
                "application_error",
                &format!("Expected JSON response but got: {content_type}"),
            )),
            ..Default::default()
        };
    }

    let body = response.bytes().await.unwrap_or_default();
    let Ok(data) = serde_json::from_slice::<Value>(&body) else {
        return EmailSendResult {
            ok: false,
            error: Some(sdk_error(
                error_code,
                "application_error",
                "Failed to decode JSON response",
            )),
            ..Default::default()
        };
    };

    // The status wins when it is an error; otherwise the body's own
    // `statusCode` is consulted, because Resend has answered 200 with an
    // error document.
    let body_status = data.get("statusCode").and_then(Value::as_i64);
    let effective = if status >= 400 {
        Some(status)
    } else {
        body_status
    };
    if let Some(code) = effective.filter(|c| *c != 200) {
        let message = data
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Unknown error");
        let error_type = data
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("InternalServerError");
        tracing::warn!(kind, to = %redact(to), code, "Resend send failed");
        return EmailSendResult {
            ok: false,
            error: Some(sdk_error(code, error_type, message)),
            ..Default::default()
        };
    }

    let message_id = data
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| data.get("message_id").and_then(Value::as_str))
        .filter(|id| !id.is_empty());
    let Some(message_id) = message_id else {
        // A send with no id cannot be matched to the webhook event that
        // eventually reports on it, so it is a failure rather than a
        // silent loss of correlation.
        tracing::warn!(kind, to = %redact(to), "Resend returned no id");
        return EmailSendResult {
            ok: false,
            error: Some("resend_no_message_id".to_string()),
            ..Default::default()
        };
    };

    tracing::info!(kind, to = %redact(to), message_id, "sent");
    EmailSendResult {
        ok: true,
        message_id: Some(message_id.to_string()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_keeps_one_character_and_the_domain() {
        assert_eq!(redact("alice@example.com"), "a***@example.com");
        assert_eq!(redact("@example.com"), "***@example.com");
        // No at-sign at all is not an address, and nothing is kept.
        assert_eq!(redact("nonsense"), "***");
        assert_eq!(redact(""), "***");
    }

    /// The table was read off the SDK and then confirmed by driving the
    /// real thing; these are the exact strings it produced.
    #[test]
    fn the_error_class_matches_the_sdk_table() {
        assert_eq!(
            sdk_error(422, "validation_error", "bad address"),
            "ValidationError: bad address"
        );
        assert_eq!(
            sdk_error(500, "application_error", "boom"),
            "ApplicationError: boom"
        );
        assert_eq!(
            sdk_error(429, "rate_limit_exceeded", "slow down"),
            "RateLimitError: slow down"
        );
        assert_eq!(
            sdk_error(429, "monthly_quota_exceeded", "x"),
            "RateLimitError: x"
        );
        assert_eq!(
            sdk_error(400, "validation_error", "x"),
            "ValidationError: x"
        );
        assert_eq!(
            sdk_error(422, "missing_required_field", "x"),
            "MissingRequiredFieldsError: x"
        );
        assert_eq!(
            sdk_error(401, "missing_api_key", "x"),
            "MissingApiKeyError: x"
        );
        assert_eq!(
            sdk_error(403, "invalid_api_key", "x"),
            "InvalidApiKeyError: x"
        );
        // 401 with invalid_api_key is NOT in the table — it is mapped
        // under 403 — so it falls through to the bare class. Confirmed
        // against the SDK rather than assumed.
        assert_eq!(
            sdk_error(401, "invalid_api_key", "nope"),
            "ResendError: nope"
        );
        // An unknown status, and a known one with an unmapped type.
        assert_eq!(sdk_error(502, "application_error", "x"), "ResendError: x");
        assert_eq!(sdk_error(500, "something_else", "x"), "ResendError: x");
    }
}
