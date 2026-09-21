//! Install and MCP-setup scripts, and the binary download redirect.
//!
//! Ported from `backend/app/api/install.py`. Three of these read a file
//! off disk and set headers; the fourth resolves the newest GitHub
//! release through `crate::versions`, which is where that cache lives
//! now — and unlike the heartbeat path, this one is allowed to fetch,
//! because a human waiting on a download can wait five seconds where a
//! node's heartbeat cannot.

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use serde_json::Value;
use axum::response::{IntoResponse, Response};

use crate::app::AppState;
use crate::error::ApiError;
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
pub async fn install_sh(rate: PerMinute<30>, State(state): State<AppState>) -> Response {
    if let Err(err) = rate.check().await {
        return err.into_response();
    }
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
pub async fn mcp_setup_sh(rate: PerMinute<30>, State(state): State<AppState>) -> Response {
    if let Err(err) = rate.check().await {
        return err.into_response();
    }
    script(
        &state,
        "mcp-setup.sh",
        "text/x-shellscript; charset=utf-8",
        Some(SETUP_SCRIPT_CACHE),
    )
    .await
}

/// `GET /mcp-setup.ps1` — the Windows equivalent.
pub async fn mcp_setup_ps1(rate: PerMinute<30>, State(state): State<AppState>) -> Response {
    if let Err(err) = rate.check().await {
        return err.into_response();
    }
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

/// The combinations `install.sh` itself supports, so a friendlier URL
/// cannot give a different answer than the one-liner would have.
const ALLOWED_OS: [&str; 3] = ["linux", "macos", "windows"];
const ALLOWED_ARCH: [&str; 3] = ["x86_64", "aarch64", "armv7"];

/// `GET /downloads/{os_name}/{arch}` — 302 to the matching asset on the
/// latest release.
///
/// A vendor URL to publish in documentation, so GitHub's URL structure
/// does not leak into it. When GitHub is unreachable or the release has
/// nothing for the combination, this 404s (or 503s) rather than
/// guessing: the install script has a source-build fallback and the
/// client is expected to take it.
pub async fn download_binary(
    rate: PerMinute<60>,
    State(state): State<AppState>,
    axum::extract::Path((os_name, arch)): axum::extract::Path<(String, String)>,
) -> Result<Response, ApiError> {
    rate.check().await?;
    let os_key = os_name.to_lowercase();
    let arch_key = arch.to_lowercase();

    if !ALLOWED_OS.contains(&os_key.as_str()) {
        return Err(ApiError::not_found(format!(
            "Unsupported OS '{os_name}'. Try one of: {}.",
            python_list(&ALLOWED_OS)
        )));
    }
    if !ALLOWED_ARCH.contains(&arch_key.as_str()) {
        return Err(ApiError::not_found(format!(
            "Unsupported arch '{arch}'. Try one of: {}.",
            python_list(&ALLOWED_ARCH)
        )));
    }

    let Some(release) = crate::versions::refresh_release(&state.http, false).await else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Release metadata unavailable. Try /install.sh on Linux/macOS, or download \
             the MSI directly from the latest GitHub release on Windows.",
        ));
    };

    let Some(url) = pick_asset(&release, &os_key, &arch_key) else {
        let tag = release
            .get("tag_name")
            .and_then(|v| v.as_str())
            .unwrap_or("latest");
        return Err(ApiError::not_found(format!(
            "No prebuilt binary for {os_key}/{arch_key} in release {tag}. \
             Try the install script for a source fallback."
        )));
    };

    // Starlette's RedirectResponse quotes the location, leaving the
    // characters a URL is allowed to keep.
    Ok((
        StatusCode::FOUND,
        [(header::LOCATION, url.as_str())],
    )
        .into_response())
}

/// `f"{sorted(the_set)}"` — a repr'd Python list, and *sorted*: the
/// Python holds these in a set and sorts it for the message, so the
/// architectures read alphabetically rather than in declaration order.
fn python_list(items: &[&str]) -> String {
    let mut sorted: Vec<&str> = items.to_vec();
    sorted.sort_unstable();
    let inner: Vec<String> = sorted.iter().map(|item| crate::pyrepr::repr_str(item)).collect();
    format!("[{}]", inner.join(", "))
}

/// `_pick_asset`: the asset whose name matches `<os>.*<arch>`, ranked.
///
/// Windows prefers the `.msi`, which registers the service and lands
/// the binary in Program Files; the `.zip` is a bare executable, and
/// until this ranking was corrected an operator following the
/// dashboard's download link got no install at all. Elsewhere an
/// archive beats a raw binary, matching `install.sh`.
fn pick_asset(release: &Value, os_name: &str, arch: &str) -> Option<String> {
    let assets = release.get("assets")?.as_array()?;
    let is_windows = os_name == "windows";

    let rank = |name: &str| -> u8 {
        let lower = name.to_lowercase();
        if is_windows {
            if lower.ends_with(".msi") {
                return 0;
            }
            if lower.ends_with(".zip") {
                return 1;
            }
            return 2;
        }
        if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            return 0;
        }
        if lower.ends_with(".zip") {
            return 1;
        }
        2
    };

    // `re.search(f"{os}.*{arch}", name, IGNORECASE)` — both halves are
    // escaped in the Python, and neither contains a metacharacter, so
    // this is a case-insensitive "os somewhere before arch".
    let mut candidates: Vec<&Value> = assets
        .iter()
        .filter(|asset| {
            asset
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|name| !name.is_empty())
                .is_some_and(|name| {
                    let lower = name.to_lowercase();
                    match lower.find(os_name) {
                        Some(at) => lower[at + os_name.len()..].contains(arch),
                        None => false,
                    }
                })
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }

    // `list.sort` is stable, so equal ranks keep the release's own order.
    candidates.sort_by_key(|asset| {
        rank(asset.get("name").and_then(|v| v.as_str()).unwrap_or_default())
    });
    candidates[0]
        .get("browser_download_url")
        .and_then(|v| v.as_str())
        .filter(|url| !url.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release(names: &[&str]) -> Value {
        json!({
            "tag_name": "v0.1.77",
            "assets": names
                .iter()
                .map(|name| json!({
                    "name": name,
                    "browser_download_url": format!("https://example.invalid/{name}"),
                }))
                .collect::<Vec<_>>(),
        })
    }

    #[test]
    fn windows_prefers_the_installer_over_the_archive() {
        // The ranking that was wrong until 2026-04-28: an operator
        // following the dashboard's download link got a bare exe.
        let rel = release(&[
            "cameranode-windows-x86_64.zip",
            "cameranode-windows-x86_64.msi",
        ]);
        assert_eq!(
            pick_asset(&rel, "windows", "x86_64").unwrap(),
            "https://example.invalid/cameranode-windows-x86_64.msi"
        );
    }

    #[test]
    fn elsewhere_an_archive_beats_a_raw_binary() {
        let rel = release(&[
            "cameranode-linux-x86_64",
            "cameranode-linux-x86_64.tar.gz",
        ]);
        assert_eq!(
            pick_asset(&rel, "linux", "x86_64").unwrap(),
            "https://example.invalid/cameranode-linux-x86_64.tar.gz"
        );
    }

    #[test]
    fn the_match_is_case_insensitive_and_ordered() {
        let rel = release(&["CameraNode-Linux-AARCH64.tar.gz"]);
        assert!(pick_asset(&rel, "linux", "aarch64").is_some());
        // The architecture has to come *after* the OS in the name, as
        // the pattern requires.
        let rel = release(&["aarch64-then-linux.tar.gz"]);
        assert!(pick_asset(&rel, "linux", "aarch64").is_none());
    }

    #[test]
    fn nothing_matching_is_none_rather_than_a_guess() {
        let rel = release(&["cameranode-macos-aarch64.tar.gz"]);
        assert!(pick_asset(&rel, "linux", "x86_64").is_none());
        assert!(pick_asset(&json!({"tag_name": "v1"}), "linux", "x86_64").is_none());
        // An asset with no download URL is not a candidate either.
        let rel = json!({"assets": [{"name": "cameranode-linux-x86_64.tar.gz"}]});
        assert!(pick_asset(&rel, "linux", "x86_64").is_none());
    }

    #[test]
    fn the_unsupported_lists_read_like_pythons() {
        assert_eq!(python_list(&ALLOWED_OS), "['linux', 'macos', 'windows']");
        // Sorted, not declared: the Python sorts a set for this message.
        assert_eq!(python_list(&ALLOWED_ARCH), "['aarch64', 'armv7', 'x86_64']");
    }
}
