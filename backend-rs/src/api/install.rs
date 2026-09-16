//! Install and MCP-setup scripts.
//!
//! Ported from `backend/app/api/install.py`, minus one route:
//! `/downloads/{os}/{arch}` resolves the newest GitHub release through
//! `release_cache`, an in-process TTL cache this crate does not have
//! yet, so it stays on the proxy. The three here just read a file off
//! disk and set headers.

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::app::AppState;
use crate::ratelimit::PerMinute;

/// These scripts are fixed in master and redeployed often — six changes
/// in one evening during an auto-setup debugging session, per the
/// Python's own note. With no explicit header a browser or CDN may
/// cache indefinitely under its default heuristic and keep serving a
/// known-broken script after the fix ships.
const SETUP_SCRIPT_CACHE: &str = "no-cache, max-age=60";

/// Read one script and dress it the way Starlette's `PlainTextResponse`
/// does.
///
/// The `; charset=utf-8` is not decoration: Starlette appends it to any
/// `media_type` beginning with `text/` that does not already name a
/// charset, so the wire value is `text/x-shellscript; charset=utf-8`
/// even though the handler passes the bare type. Header insertion order
/// matches Python's too — explicit headers first, then the two the
/// framework derives.
async fn script(
    state: &AppState,
    filename: &'static str,
    media_type: &'static str,
    cache: Option<&'static str>,
) -> Response {
    let path = std::path::Path::new(&state.config.scripts_dir).join(filename);
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(content) => content,
        Err(err) => {
            // Python lets `read_text` raise, which Starlette renders as
            // a bare 500. Matching that rather than inventing a 404: a
            // missing script is a broken deployment, not a missing
            // resource, and it should look like one to the operator.
            tracing::error!(error = %err, ?path, "install script unreadable");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "Internal Server Error",
            )
                .into_response();
        }
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("inline; filename={filename}"))
            .expect("filename is a static ASCII literal"),
    );
    if let Some(cache) = cache {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    }
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(media_type));

    (headers, content).into_response()
}

/// `GET /install.sh` — the CameraNode installer for Linux and macOS.
pub async fn install_sh(_rate: PerMinute<30>, State(state): State<AppState>) -> Response {
    // No Cache-Control, matching the Python: only the two mcp-setup
    // scripts carry one.
    script(
        &state,
        "install.sh",
        "text/x-shellscript; charset=utf-8",
        None,
    )
    .await
}

/// `GET /mcp-setup.sh` — points an MCP client at this Command Center.
/// Unrelated to installing a CameraNode.
pub async fn mcp_setup_sh(_rate: PerMinute<30>, State(state): State<AppState>) -> Response {
    script(
        &state,
        "mcp-setup.sh",
        "text/x-shellscript; charset=utf-8",
        Some(SETUP_SCRIPT_CACHE),
    )
    .await
}

/// `GET /mcp-setup.ps1` — the Windows equivalent.
pub async fn mcp_setup_ps1(_rate: PerMinute<30>, State(state): State<AppState>) -> Response {
    // `text/plain`, not `x-shellscript`. The Python differs between the
    // two deliberately and a client sniffing the type would notice.
    script(
        &state,
        "mcp-setup.ps1",
        "text/plain; charset=utf-8",
        Some(SETUP_SCRIPT_CACHE),
    )
    .await
}
