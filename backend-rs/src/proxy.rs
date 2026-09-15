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
    http::{header, HeaderMap, HeaderName, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;

use crate::app::AppState;

/// The upstream client.
///
/// hyper rather than reqwest, deliberately. reqwest routes every request
/// through a `Url`, which normalises the path per RFC 3986: `..` segments
/// are resolved and `/./` collapsed. Python does no such thing, so
/// `GET /api/cameras/../nodes` reached it as `GET /api/nodes` and was
/// answered 200, where Python served directly answers 404. A proxy that
/// silently rewrites the path is not a transparent proxy, and during the
/// migration transparency is the whole contract.
pub type ProxyClient =
    hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, Body>;

pub fn build_client() -> ProxyClient {
    hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(hyper_util::client::legacy::connect::HttpConnector::new())
}

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
        .unwrap_or("/")
        .to_string();

    // Rebuild the URI rather than formatting a string and re-parsing it:
    // `path_and_query` goes across exactly as it arrived, `..` and all.
    let uri = match upstream_uri(&state.config.upstream, &path_and_query) {
        Some(uri) => uri,
        None => {
            tracing::error!(upstream = %state.config.upstream, "proxy: upstream is not a valid URL");
            return (StatusCode::BAD_GATEWAY, "upstream is misconfigured").into_response();
        }
    };

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

    let mut builder = hyper::Request::builder()
        .method(parts.method.clone())
        .uri(uri);

    for (name, value) in parts.headers.iter() {
        // Host is re-derived from the upstream authority below; passing
        // the client's through would name the wrong server.
        if is_hop_by_hop(name) || name == header::HOST {
            continue;
        }
        builder = builder.header(name, value);
    }

    let outbound = match builder.body(Body::from(body_bytes)) {
        Ok(r) => r,
        Err(err) => {
            tracing::error!(error = %err, "proxy: could not build upstream request");
            return (StatusCode::BAD_GATEWAY, "could not build upstream request").into_response();
        }
    };

    let upstream = match state.proxy.request(outbound).await {
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

    let (up_parts, up_body) = upstream.into_parts();
    let mut headers = HeaderMap::new();
    for (name, value) in up_parts.headers.iter() {
        if is_hop_by_hop(name) || name == header::CONTENT_LENGTH {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }

    let bytes = match up_body.collect().await {
        Ok(b) => b.to_bytes(),
        Err(err) => {
            tracing::error!(error = %err, "proxy: could not read upstream body");
            return (StatusCode::BAD_GATEWAY, "upstream response was truncated").into_response();
        }
    };

    (up_parts.status, headers, Body::from(bytes)).into_response()
}

/// Join the configured upstream origin to a request's raw path+query.
///
/// Deliberately string-free at the path: the authority comes from config
/// and the path comes from the client, and they are assembled through
/// `Uri::builder` so nothing re-parses (and therefore re-normalises) the
/// path on the way.
fn upstream_uri(upstream: &str, path_and_query: &str) -> Option<Uri> {
    let base: Uri = upstream.parse().ok()?;
    Uri::builder()
        .scheme(base.scheme()?.clone())
        .authority(base.authority()?.clone())
        .path_and_query(path_and_query)
        .build()
        .ok()
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

    #[test]
    fn the_raw_path_survives_the_hop() {
        // This is a regression test for a real divergence found by the
        // slice-2 HTTP differential: built through reqwest's `Url`,
        // `/api/cameras/../nodes` arrived at Python as `/api/nodes` and
        // was answered 200, where Python serving directly answers 404.
        // A proxy that rewrites paths is not transparent.
        for path in [
            "/api/cameras/../nodes",
            "/api/cameras/./x",
            "/a//b",
            "/api/cameras/%2e%2e/nodes",
            "/api/cameras?q=1&r=2",
            "/api/cameras/cam%20space",
            "/",
        ] {
            let uri = upstream_uri("http://127.0.0.1:8001", path).expect(path);
            assert_eq!(
                uri.path_and_query().unwrap().as_str(),
                path,
                "path was rewritten in transit"
            );
            assert_eq!(uri.host(), Some("127.0.0.1"));
            assert_eq!(uri.port_u16(), Some(8001));
        }
    }

    #[test]
    fn a_misconfigured_upstream_is_rejected_rather_than_guessed() {
        for upstream in ["", "not-a-url", "/just/a/path"] {
            assert!(upstream_uri(upstream, "/api/cameras").is_none(), "{upstream}");
        }
    }
}
