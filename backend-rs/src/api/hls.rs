//! The live video path: playlists and segments, in and out.
//!
//! Ported from `backend/app/api/hls.py`. The caches these run on are in
//! `crate::hls`; this is the four routes.
//!
//! Two pushed by the CameraNode, authenticated by its API key, and two
//! fetched by the browser, authenticated by a session. `POST /motion`
//! shares the file in Python but not the cache — it reaches the
//! WebSocket module's motion handling instead — so it stays on the
//! proxy until that moves.
//!
//! Things worth knowing before changing any of it:
//!
//! **The node-key routes read their credential inside the function**,
//! not through a dependency, so the rate limiter has already counted
//! the request by the time the key is looked at. A missing or wrong key
//! spends a slot. That is why `rate.check()` is the first line of each.
//!
//! **The body cap is checked before the body is read.** An honest
//! client sets Content-Length, and rejecting on it is what makes an
//! attempted 10 GB upload cost nothing; the post-read check is there
//! for chunked requests that declare nothing.
//!
//! **`\d` is Unicode.** `segment_٣.ts` matches Python's segment-filename
//! pattern, and `segment_1.ts\n` matches too, because `$` accepts one
//! trailing newline. Both are reproduced rather than tidied — the
//! filename is a cache key, and a port that accepted a different set
//! would cache under keys the other stack cannot serve.

use std::sync::OnceLock;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use regex::Regex;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::models::now_naive;
use crate::plans::{self, PlanContext};
use crate::query::{path_segment, Query};
use crate::ratelimit::PerMinute;

/// `_RE_SEGMENT_FILENAME` — `re.match`, so anchored at the start, and
/// Python's `$` also matches before one trailing newline.
fn segment_filename_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^segment_\d+\.ts\n?$").expect("static pattern"))
}

/// `_RE_SEGMENT_URI`, applied per line. The lookahead that skips
/// comment lines is the caller's `starts_with('#')` test, because a
/// line is the unit either way.
fn segment_uri_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:.*[/\\])?(segment_\d+\.ts)[ \t\r]*$").expect("static pattern")
    })
}

/// `_RE_CODECS`.
fn codecs_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^#EXT-X-CODECS:.*$").expect("static pattern"))
}

/// `_rewrite_playlist`: segment URIs become relative proxy paths, and
/// any `#EXT-X-CODECS` line loses its content.
///
/// The codecs line is removed rather than corrected because it is only
/// valid in a master playlist: in a media playlist hls.js starts
/// parsing the file as a master, never fires `MANIFEST_PARSED`, and the
/// player sits at "Connecting…" forever.
///
/// Per line, because both patterns are `MULTILINE` and `.` does not
/// match a newline — so a line is exactly the span either can touch.
/// The URI pattern is anchored at both ends, so a rewritten line loses
/// whatever trailing spaces or carriage return it had.
pub fn rewrite_playlist(raw: &str) -> String {
    let lines: Vec<String> = raw
        .split('\n')
        .map(|line| {
            if !line.starts_with('#') {
                if let Some(captures) = segment_uri_re().captures(line) {
                    return format!("segment/{}", &captures[1]);
                }
            }
            if codecs_re().is_match(line) {
                return String::new();
            }
            line.to_string()
        })
        .collect();
    lines.join("\n")
}

/// `_read_capped_body`: the declared length first, then the real one.
async fn read_capped_body(request: Request, max_bytes: usize) -> Result<Bytes, ApiError> {
    if let Some(declared) = request.headers().get(header::CONTENT_LENGTH) {
        // `int(header)` — Python accepts a sign and surrounding
        // whitespace here, and anything else is the 400. That 400 is
        // unreachable behind a real server, though: uvicorn's h11 and
        // hyper both refuse a malformed Content-Length at the protocol
        // level, before any handler sees it. Kept because the Python
        // keeps it, and because the two servers refuse it differently
        // — see expected_divergences.md.
        let raw = String::from_utf8_lossy(declared.as_bytes()).to_string();
        let Ok(crate::pyint::PyInt::Small(declared_int)) = crate::pyint::str_as_int(&raw) else {
            // A value too large for i64 is still a number to Python, and
            // still over the cap.
            return match crate::pyint::str_as_int(&raw) {
                Ok(_) => Err(too_large(format!(
                    "Body declared {raw} bytes; max is {max_bytes}"
                ))),
                Err(_) => Err(ApiError::bad_request("Invalid Content-Length header")),
            };
        };
        if declared_int > max_bytes as i64 {
            return Err(too_large(format!(
                "Body declared {declared_int} bytes; max is {max_bytes}"
            )));
        }
    }

    // The post-read check, for a chunked body that declared nothing.
    //
    // Python reads the whole thing — `await request.body()` — and its
    // 413 names the real length, so the message cannot be produced
    // without knowing it. It does not follow that the bytes have to be
    // kept: this counts every one and keeps only what fits, so an
    // oversized chunked upload is refused with the same number in it
    // and a bounded amount of memory. Starlette has no streaming-cap
    // primitive to do the same with, which its own docstring says.
    let mut stream = request.into_body().into_data_stream();
    let mut body = Vec::new();
    let mut total: usize = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ApiError::bad_request("could not read request body"))?;
        total = total.saturating_add(chunk.len());
        if total <= max_bytes {
            body.extend_from_slice(&chunk);
        }
    }
    if total > max_bytes {
        return Err(too_large(format!(
            "Body is {total} bytes; max is {max_bytes}"
        )));
    }
    Ok(Bytes::from(body))
}

fn too_large(detail: String) -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, detail)
}

/// The node behind an `X-Node-API-Key`, and the camera it is pushing
/// for.
///
/// Both checks are the Python's: the node by its key hash, then the
/// camera by *three* columns — its id, its node, and the node's org.
/// The last is defence in depth, so a future schema drift cannot let a
/// node in one org push into another's camera.
struct PushTarget {
    org_id: String,
    /// The node's own id, not its primary key — what a motion row
    /// stores and what the ownership check joins on.
    node_id: String,
    camera_name: String,
    disabled_by_plan: bool,
}

async fn resolve_push_target(
    state: &AppState,
    headers: &HeaderMap,
    camera_id: &str,
) -> Result<PushTarget, ApiError> {
    let Some(key) = headers.get("x-node-api-key") else {
        return Err(ApiError::unauthorized("Missing API key"));
    };
    let node: Option<(i32, String, String)> =
        sqlx::query_as("SELECT id, org_id, node_id FROM camera_nodes WHERE api_key_hash = $1")
            .bind(crate::api::node_writes::node_key_hash(key.as_bytes()))
            .fetch_optional(&state.pool)
            .await?;
    let Some((node_pk, org_id, node_id)) = node else {
        return Err(ApiError::unauthorized("Invalid API key"));
    };

    let camera: Option<(String, bool)> = sqlx::query_as(
        "SELECT name, disabled_by_plan FROM cameras \
          WHERE camera_id = $1 AND node_id = $2 AND org_id = $3",
    )
    .bind(camera_id)
    .bind(node_pk)
    .bind(&org_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((camera_name, disabled_by_plan)) = camera else {
        return Err(ApiError::not_found("Camera not found"));
    };

    Ok(PushTarget {
        org_id,
        node_id,
        camera_name,
        disabled_by_plan,
    })
}

/// `GET /api/cameras/{camera_id}/stream.m3u8`.
pub async fn get_hls_playlist(
    State(state): State<AppState>,
    user: AuthUser,
    Path(camera_id): Path<String>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let camera_id = path_segment(&camera_id)?;
    let camera: Option<(Option<i32>,)> =
        sqlx::query_as("SELECT node_id FROM cameras WHERE camera_id = $1 AND org_id = $2")
            .bind(camera_id)
            .bind(&user.org_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some((node_id,)) = camera else {
        return Err(ApiError::not_found("Camera not found"));
    };
    // `if not camera.node_id` — a camera with no node has no stream.
    let Some(node_id) = node_id.filter(|id| *id != 0) else {
        return Err(ApiError::not_found("Camera node not found"));
    };

    // This route is polled about once a second per viewer, so the log
    // self-throttles to one row per user and camera per five minutes.
    // The node_id written is the integer foreign key as a string, which
    // is what `str(camera.node_id)` produces.
    if state.hls.access_log_due(&user.user_id, camera_id) {
        let user_agent: String = headers
            .get(header::USER_AGENT)
            .map(|v| {
                String::from_utf8_lossy(v.as_bytes())
                    .chars()
                    .take(500)
                    .collect()
            })
            .unwrap_or_default();
        let logged = sqlx::query(
            "INSERT INTO stream_access_logs
                 (user_id, user_email, org_id, camera_id, node_id, ip_address, user_agent, accessed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&user.user_id)
        .bind(&user.email)
        .bind(&user.org_id)
        .bind(camera_id)
        .bind(node_id.to_string())
        .bind(peer.ip().to_string())
        .bind(&user_agent)
        .bind(now_naive())
        .execute(&state.pool)
        .await;
        if let Err(err) = logged {
            // Python catches and rolls back: a missing access log is
            // not worth failing a playlist fetch over.
            tracing::warn!(error = %err, "Failed to log stream access");
        }
    }

    let cached = state.hls.playlist(camera_id);
    let fresh = cached.as_ref().filter(|(_, age)| playlist_is_fresh(*age));

    if let Some((playlist, age)) = fresh {
        if state.hls.first_stream_get(camera_id) {
            tracing::info!(
                camera_id,
                playlist_age = age.as_secs_f64(),
                bytes = playlist.len(),
                cached_segments = state.hls.segment_count(camera_id),
                "hls: first stream.m3u8 HIT"
            );
        }
        return Ok((no_store_headers(), playlist.clone()).into_response());
    }

    // One line per camera per restart, so an operator can tell "the node
    // never pushed a playlist" from "nobody ever asked for the stream".
    // hls.js retries every 400ms; unmuted this would be a flood.
    if state.hls.first_stream_get(camera_id) {
        tracing::warn!(
            camera_id,
            playlist_cached = cached.is_some(),
            segment_cache_entries = state.hls.segment_count(camera_id),
            "hls: first stream.m3u8 MISS — CameraNode hasn't POST /playlist for this camera yet"
        );
    }
    Err(ApiError::not_found("Stream not started yet"))
}

/// Whether a cached playlist is still worth serving.
///
/// Its own function so the boundary can be tested without waiting
/// thirty seconds: a scenario harness cannot tell "serves a stale
/// playlist" from "serves a fresh one" inside a run that takes
/// milliseconds.
fn playlist_is_fresh(age: std::time::Duration) -> bool {
    age < crate::hls::PLAYLIST_CACHE_MAX_AGE
}

/// The playlist response's headers, in Starlette's order: what the
/// handler passed, then the content type it derived.
fn no_store_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate, max-age=0"),
    );
    headers.insert("pragma", HeaderValue::from_static("no-cache"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.apple.mpegurl"),
    );
    headers
}

/// `GET /api/cameras/{camera_id}/segment/{filename}`.
pub async fn get_hls_segment(
    State(state): State<AppState>,
    user: AuthUser,
    Path((camera_id, filename)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let camera_id = path_segment(&camera_id)?;
    // The filename is a path parameter too, so a percent-encoded slash
    // means Starlette's router never matched this route at all and the
    // answer is its 404, not the handler's 400.
    let filename = path_segment(&filename)?;
    let exists: Option<(i32,)> =
        sqlx::query_as("SELECT id FROM cameras WHERE camera_id = $1 AND org_id = $2")
            .bind(camera_id)
            .bind(&user.org_id)
            .fetch_optional(&state.pool)
            .await?;
    if exists.is_none() {
        return Err(ApiError::not_found("Camera not found"));
    }
    if !segment_filename_re().is_match(filename) {
        return Err(ApiError::bad_request("Invalid segment filename"));
    }

    // The effective plan, not the token's claim: a stale JWT must not
    // keep buying paid-tier viewer hours after the grace window closes.
    // The first segment of the month for an org costs one database read
    // and every later one is in memory.
    let plan = plans::effective_plan_for_caps(&plan_ctx(&state), &user.org_id, true).await;
    let max_hours = plans::get_plan_limits(&plan).max_viewer_hours_per_month;
    if max_hours > 0 {
        let used = state
            .hls
            .warm_viewer_seconds(&state.pool, &user.org_id)
            .await;
        if used >= max_hours * 3600 {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "Monthly viewer-hour cap reached ({max_hours}h on your current plan). \
                     Live playback will resume on the 1st of next month, or upgrade your \
                     plan for more viewing time."
                ),
            )
            .with_header("Retry-After", "3600"));
        }
    }

    let Some(body) = state.hls.segment(camera_id, filename) else {
        return Err(ApiError::not_found("Segment not found"));
    };
    // Only a served segment is charged — a 404 or a capped request is
    // not a second of video.
    state.hls.record_viewer_second(&user.org_id);
    Ok((
        [
            (header::CACHE_CONTROL, "public, max-age=3600"),
            (header::CONTENT_TYPE, "video/mp2t"),
        ],
        body,
    )
        .into_response())
}

fn plan_ctx(state: &AppState) -> PlanContext<'_> {
    PlanContext {
        pool: &state.pool,
        client: &state.http,
        clerk_base_url: &state.config.clerk_api_url,
        clerk_secret: &state.config.clerk_secret_key,
        local_auth: state.config.is_local_auth(),
    }
}

/// `POST /api/cameras/{camera_id}/push-segment?filename=…`.
pub async fn push_segment(
    rate: PerMinute<1200>,
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    // First, because the key is read inside the function in Python and
    // the limiter therefore counts every request, authenticated or not.
    rate.check().await?;
    let camera_id = path_segment(&camera_id)?.to_string();

    let mut query = Query::parse(request.uri().query());
    let filename = query.required_str("filename");
    query.finish()?;
    let target = resolve_push_target(&state, request.headers(), &camera_id).await?;

    // Over the plan's camera cap, `enforce_camera_cap` has flagged the
    // newest cameras. A 402 with the reason lets the node say why in its
    // own interface instead of retrying a push that will never succeed.
    if target.disabled_by_plan {
        // `get_plan_limits_for_org` resolves the *nominal* plan, not
        // the effective one the segment route uses — so a past-due org
        // still sees its paid tier's name and cap in this refusal.
        let plan = plans::resolve_org_plan(&plan_ctx(&state), &target.org_id).await;
        let limits = plans::get_plan_limits(&plan);
        let plan_name = plans::get_plan_display_name(&plan);
        return Err(ApiError::new(
            StatusCode::PAYMENT_REQUIRED,
            json!({
                "message": "Camera suspended by plan limit",
                "plan_limit_hit": {
                    "plan": plan_name,
                    "max_cameras": limits.max_cameras,
                    "skipped": [target.camera_name],
                    "detail": format!(
                        "Camera '{}' is over the {plan_name} plan limit ({} cameras). \
                         Upgrade to resume streaming.",
                        target.camera_name, limits.max_cameras
                    ),
                },
            }),
        ));
    }

    if !segment_filename_re().is_match(&filename) {
        return Err(ApiError::bad_request("Invalid segment filename"));
    }

    let body = read_capped_body(request, state.config.segment_push_max_bytes).await?;
    let cached_segments = state.hls.push_segment(
        &camera_id,
        &filename,
        body,
        state.config.segment_cache_max_per_camera,
        state.config.segment_cache_max_total_bytes,
    );
    Ok(Json(
        json!({ "success": true, "cached_segments": cached_segments }),
    ))
}

/// `POST /api/cameras/{camera_id}/playlist`.
pub async fn update_hls_playlist(
    rate: PerMinute<600>,
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;
    let camera_id = path_segment(&camera_id)?.to_string();
    resolve_push_target(&state, request.headers(), &camera_id).await?;

    let body = read_capped_body(request, state.config.playlist_push_max_bytes).await?;
    // The decode error is the response, so it is CPython's message and
    // not Rust's — see `crate::pycodec`.
    let playlist = match std::str::from_utf8(&body) {
        Ok(text) => text,
        Err(_) => {
            let detail = crate::pycodec::utf8_decode_error(&body)
                .unwrap_or_else(|| "invalid utf-8".to_string());
            return Err(ApiError::bad_request(format!(
                "Invalid playlist content: {detail}"
            )));
        }
    };

    let rewritten = rewrite_playlist(playlist);
    state.hls.set_playlist(&camera_id, rewritten.clone());

    if state.hls.first_playlist_push(&camera_id) {
        // The first non-comment line is the first segment URI, which is
        // the ground truth for what the rewriter is being handed.
        let sample: String = playlist
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.trim_start().starts_with('#'))
            .unwrap_or("<none>")
            .chars()
            .take(200)
            .collect();
        tracing::info!(
            camera_id,
            raw_bytes = playlist.len(),
            rewritten_bytes = rewritten.len(),
            first_segment_uri = %crate::pyrepr::repr_str(&sample),
            "hls: first playlist push"
        );
    }

    if state
        .hls
        .bump_playlist_count(&camera_id, state.config.cleanup_interval)
    {
        state.hls.evict_caches();
    }

    Ok(Json(
        json!({ "success": true, "message": "Playlist updated" }),
    ))
}

/// `POST /api/cameras/{camera_id}/motion`.
///
/// The reliable half of motion reporting: it works whether or not the
/// node's WebSocket is up, which is why CameraNode uses it rather than
/// the `event` frame.
pub async fn push_motion_event(
    rate: PerMinute<120>,
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;
    let camera_id = path_segment(&camera_id)?.to_string();
    let target = resolve_push_target(&state, request.headers(), &camera_id).await?;

    // The per-org kill switch, for when a misbehaving sensor is
    // flooding events and an admin needs a server-side stop without
    // reaching the node. Answered 200 with `ingested: false` rather
    // than an error, so CameraNode treats it as a deliberate refusal
    // and does not spend its retry budget — the same shape the
    // plan-cap suspension uses.
    //
    // Checked *before* the body is read, as the Python does: a
    // malformed body reaches nothing while ingestion is off.
    let enabled = crate::settings::get(
        &state.pool,
        &target.org_id,
        "motion_ingestion_enabled",
        Some("true"),
    )
    .await?
    .unwrap_or_else(|| "true".to_string());
    if enabled.to_lowercase() != "true" {
        return Ok(Json(json!({
            "success": true,
            "ingested": false,
            "reason": "ingestion_disabled",
        })));
    }

    let body = axum::body::to_bytes(request.into_body(), 2 * 1024 * 1024)
        .await
        .map_err(|_| ApiError::internal("could not read the request body"))?;
    let body = crate::query::parse_handler_json(&body)?;

    crate::api::ws::handle_motion_event(
        &state,
        &target.node_id,
        &target.org_id,
        &json!({
            "camera_id": camera_id,
            "score": body.get("score").cloned().unwrap_or(Value::Null),
            "segment_seq": body.get("segment_seq").cloned().unwrap_or(Value::Null),
            "timestamp": body.get("timestamp").cloned().unwrap_or(Value::Null),
        }),
    )
    .await;

    Ok(Json(json!({ "success": true, "ingested": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filename_pattern_is_pythons() {
        for good in [
            "segment_1.ts",
            "segment_00001.ts",
            "segment_1.ts\n",
            "segment_٣.ts",
        ] {
            assert!(segment_filename_re().is_match(good), "{good:?}");
        }
        for bad in [
            "SEGMENT_1.ts",
            "segment_.ts",
            "x/segment_1.ts",
            "segment_1.ts\r\n",
            "segment_1.ts\nx",
            "segment_1.tsx",
            "",
        ] {
            assert!(!segment_filename_re().is_match(bad), "{bad:?}");
        }
    }

    #[test]
    fn segment_uris_become_relative_proxy_paths() {
        let raw = "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:1.0,\nsegment_00001.ts\n";
        assert_eq!(
            rewrite_playlist(raw),
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:1.0,\nsegment/segment_00001.ts\n"
        );
        // Any prefix is dropped, forward or back slash, and trailing
        // whitespace goes with it.
        assert_eq!(
            rewrite_playlist("/var/hls/segment_7.ts"),
            "segment/segment_7.ts"
        );
        assert_eq!(
            rewrite_playlist("C:\\hls\\segment_7.ts"),
            "segment/segment_7.ts"
        );
        assert_eq!(rewrite_playlist("segment_7.ts  \r"), "segment/segment_7.ts");
        // A comment line is never a URI, even one that ends like a
        // segment name.
        assert_eq!(rewrite_playlist("#segment_7.ts"), "#segment_7.ts");
    }

    #[test]
    fn the_codecs_line_is_removed_but_its_newline_is_not() {
        assert_eq!(
            rewrite_playlist("#EXTM3U\n#EXT-X-CODECS:avc1.64001f,mp4a.40.2\nsegment_1.ts"),
            "#EXTM3U\n\nsegment/segment_1.ts"
        );
    }

    #[test]
    fn a_playlist_is_fresh_for_thirty_seconds() {
        use std::time::Duration;
        assert!(playlist_is_fresh(Duration::ZERO));
        assert!(playlist_is_fresh(Duration::from_millis(29_999)));
        // The comparison is `<`, so exactly thirty seconds is stale.
        assert!(!playlist_is_fresh(Duration::from_secs(30)));
        assert!(!playlist_is_fresh(Duration::from_secs(31)));
    }

    #[test]
    fn a_playlist_with_no_segments_is_unchanged() {
        let raw = "#EXTM3U\n#EXT-X-ENDLIST\n";
        assert_eq!(rewrite_playlist(raw), raw);
    }
}
