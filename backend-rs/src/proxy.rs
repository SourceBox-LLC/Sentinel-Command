//! The strangler's other half: forward anything not yet ported to Python.
//!
//! Rust owns `:8000`. Python moved to `:8001` on localhost inside the same
//! container, so a forwarded request costs a loopback hop and nothing else
//! — no extra machine, no cross-host latency, no Fly config beyond the
//! entrypoint.
//!
//! This handler is deliberately dumb. It does not parse bodies, does not
//! interpret status codes and does not retry: a proxied route must behave
//! exactly as it did when Python served it directly, including its
//! failures. The only judgement here is which headers not to forward.
//!
//! As route groups move into Rust they are registered ahead of this
//! fallback and simply stop reaching it. When the last one moves, this
//! file and the second process are deleted together.

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderName, StatusCode},
    response::{IntoResponse, Response},
};

use crate::app::AppState;

/// Hop-by-hop headers (RFC 9110 §7.6.1). Forwarding these corrupts the
/// connection semantics between us and the client — `Connection` in
/// particular would tell the client about a keep-alive that belongs to the
/// upstream socket, not theirs.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(&name.as_str())
}

pub async fn forward(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();

    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", state.config.upstream, path_and_query);

    // Collect the body rather than streaming it. Uploads here are bounded
    // by SEGMENT_PUSH_MAX_BYTES and the axum body limit already applied
    // upstream of this handler, and buffering keeps the proxy a single
    // obvious hop instead of a streaming pipeline with its own failure
    // modes. Revisit if a genuinely large endpoint ends up proxied for
    // long.
    let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(err) => {
            tracing::error!(error = %err, "proxy: could not read request body");
            return (StatusCode::BAD_REQUEST, "could not read request body").into_response();
        }
    };

    let mut outbound = state
        .http
        .request(parts.method.clone(), &url)
        .body(body_bytes);

    for (name, value) in parts.headers.iter() {
        // Host must be re-derived by reqwest for the upstream socket.
        if is_hop_by_hop(name) || name == header::HOST {
            continue;
        }
        outbound = outbound.header(name, value);
    }

    let upstream = match outbound.send().await {
        Ok(r) => r,
        Err(err) => {
            // The Python process is in the same container; a failure here
            // means it is down or wedged, which is an outage rather than a
            // client error.
            tracing::error!(error = %err, path = %path_and_query, "proxy: upstream unreachable");
            return (
                StatusCode::BAD_GATEWAY,
                "upstream application is not responding",
            )
                .into_response();
        }
    };

    let status = upstream.status();
    let mut headers = HeaderMap::new();
    for (name, value) in upstream.headers().iter() {
        if is_hop_by_hop(name) || name == header::CONTENT_LENGTH {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => {
            tracing::error!(error = %err, "proxy: could not read upstream body");
            return (StatusCode::BAD_GATEWAY, "upstream response was truncated").into_response();
        }
    };

    (status, headers, Body::from(bytes)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_are_not_forwarded() {
        for name in ["connection", "upgrade", "transfer-encoding", "te"] {
            assert!(is_hop_by_hop(&HeaderName::from_static(name)), "{name}");
        }
    }

    #[test]
    fn ordinary_headers_are_forwarded() {
        for name in ["authorization", "content-type", "x-node-api-key", "cookie"] {
            assert!(!is_hop_by_hop(&HeaderName::from_static(name)), "{name}");
        }
    }
}
