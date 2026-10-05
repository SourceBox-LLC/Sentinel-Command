//! Node-key and node-management writes.
//!
//! Ported from `backend/app/api/nodes.py` (`/validate`, create, rotate
//! key) and `backend/app/api/cameras.py` (`/codec`, `/danger/wipe-logs`).
//!
//! Two of these authenticate a CameraNode by its API key rather than a
//! session, and the hashing has a trap the Sentinel agent key does not:
//! Starlette decodes header values as latin-1, and these handlers hash
//! `api_key.encode()` — **UTF-8** of that decoded text. A key byte
//! above 0x7F is therefore hashed as two bytes here and as one byte by
//! the agent-key path, which re-encodes latin-1. Hashing the raw bytes
//! would authenticate a different set of keys than Python does.
//!
//! Several branches below exist only to return the same *status* Python
//! does for input no real CameraNode sends — a number where a string
//! belongs is a 500 there, because `len(5)` raises outside any `try`.
//! One such branch deliberately does not match; see `codec` and
//! `tests/differential/expected_divergences.md`.

use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::app::AppState;
use crate::audit::{audit_label, python_json, write_audit};
use crate::auth::{RequireActiveBilling, RequireAdmin};
use crate::error::ApiError;
use crate::models::{iso_naive, now_naive};
use crate::plans::{self, PlanContext, PAID_PLAN_SLUGS};
use crate::pyrepr;
use crate::query::{path_segment, BodyErrors, ModelBody};
use crate::ratelimit::{PerHour, PerMinute};

/// `hashlib.sha256(api_key.encode()).hexdigest()` for a header value
/// Starlette has already decoded as latin-1.
pub fn node_key_hash(header: &[u8]) -> String {
    let decoded: String = header.iter().map(|&b| b as char).collect();
    Sha256::digest(decoded.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn header_bytes<'a>(headers: &'a HeaderMap, name: &str) -> &'a [u8] {
    headers.get(name).map(|v| v.as_bytes()).unwrap_or_default()
}

pub(crate) async fn body_bytes(request: Request) -> Result<axum::body::Bytes, ApiError> {
    axum::body::to_bytes(request.into_body(), 2 * 1024 * 1024)
        .await
        .map_err(|_| ApiError::bad_request("could not read request body"))
}

// ---------------------------------------------------------------------
// POST /api/nodes/validate
// ---------------------------------------------------------------------

pub async fn validate_node(
    rate: PerMinute<10>,
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    // Everything this route checks happens inside the Python function,
    // after the decorator — so every request spends a slot.
    rate.check().await?;
    let key = header_bytes(request.headers(), "x-node-api-key").to_vec();
    if key.is_empty() {
        return Err(ApiError::unauthorized("API key required"));
    }

    // `await request.json()` ignores Content-Type, and an empty body is
    // a decode error like any other.
    let bytes = body_bytes(request).await?;
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return Err(ApiError::bad_request("Invalid JSON body"));
    };
    // The Python called `.get` on whatever arrived and bound `node_id`
    // to a varchar however it was typed, so a list body, a numeric id or
    // an object id escaped as an unhandled exception — a 500 for the
    // caller's mistake. They are 400s, beside the route's other 400s.
    let Some(map) = body.as_object() else {
        return Err(ApiError::bad_request(
            "Invalid JSON body: expected an object",
        ));
    };
    let node_id = map.get("node_id").cloned().unwrap_or(Value::Null);
    if !pyrepr::truthy(&node_id) {
        return Err(ApiError::bad_request("node_id is required"));
    }
    let Value::String(node_id) = node_id else {
        return Err(ApiError::bad_request("node_id must be a string"));
    };

    let row: Option<(i32, String, String, String)> = sqlx::query_as(
        "SELECT id, node_id, name, api_key_hash FROM camera_nodes WHERE node_id = $1 LIMIT 1",
    )
    .bind(&node_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((id, node_id, name, stored_hash)) = row else {
        return Err(ApiError::not_found(format!("Node '{node_id}' not found")));
    };

    if node_key_hash(&key) != stored_hash {
        record_node_register_error(
            &state.pool,
            id,
            "Invalid API key — rotate the key in Settings and re-run the installer.",
        )
        .await;
        return Err(ApiError::forbidden("Invalid API key for this node"));
    }

    Ok(Json(
        json!({ "success": true, "node_id": node_id, "name": name }),
    ))
}

/// Persist why a node is stuck in `pending`, so the dashboard can show
/// it instead of sending the operator to read CameraNode logs.
///
/// Best-effort: the caller is about to return a 4xx either way, so a
/// failure here is logged and swallowed.
pub(crate) async fn record_node_register_error(pool: &crate::db::Pool, id: i32, reason: &str) {
    let reason: String = reason.chars().take(500).collect();
    let now = now_naive();
    if let Err(err) = sqlx::query(
        "UPDATE camera_nodes
            SET last_register_error = $1, last_register_error_at = $2, updated_at = $2
          WHERE id = $3",
    )
    .bind(reason)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await
    {
        tracing::error!(error = %err, node = id, "failed to persist last_register_error");
    }
}

// ---------------------------------------------------------------------
// POST /api/cameras/{camera_id}/codec
// ---------------------------------------------------------------------

/// The `video_codec` / `audio_codec` column width.
const CODEC_MAX_CHARS: usize = 50;

/// `sanitize_video_codec`: upgrade a suspicious H.264 level.
///
/// Older CameraNode builds wrote `level_idc=0` for the Pi's hardware
/// encoder, which rounded to H.264 level 1.0 — QCIF — and browsers then
/// refused the MSE attach on the first real 720p frame. Any level below
/// 2.0 is upgraded to 3.0.
pub fn sanitize_video_codec(codec: &str) -> String {
    let chars: Vec<char> = codec.chars().collect();
    if codec.is_empty() || !codec.starts_with("avc1.") || chars.len() != 11 {
        return codec.to_string();
    }
    let level: String = chars[9..].iter().collect::<String>().to_lowercase();
    let Some(value) = python_int_base16(&level) else {
        return codec.to_string();
    };
    if value < 0x14 {
        let prefix: String = chars[..9].iter().collect();
        return format!("{prefix}1e");
    }
    codec.to_string()
}

/// `int(s, 16)` for the two-character tail of a codec string.
///
/// Python strips surrounding whitespace, takes a sign, and rejects an
/// underscore in either position. So `"-1"` parses to -1 and gets
/// upgraded, which is correct: a negative level is as malformed as a
/// zero one. Unicode decimal digits, which Python would also accept,
/// are not handled — ffprobe does not emit them.
fn python_int_base16(s: &str) -> Option<i64> {
    let trimmed = s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c));
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = i64::from_str_radix(digits, 16).ok()?;
    Some(if negative { -v } else { v })
}

pub async fn report_camera_codec(
    rate: PerMinute<30>,
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let camera_id = path_segment(&camera_id)?.to_string();
    rate.check().await?;

    let key = header_bytes(request.headers(), "x-node-api-key").to_vec();
    if key.is_empty() {
        return Err(ApiError::unauthorized("Missing API key"));
    }
    let node: Option<(i32, String, Option<String>)> = sqlx::query_as(
        "SELECT id, org_id, video_codec FROM camera_nodes WHERE api_key_hash = $1 LIMIT 1",
    )
    .bind(node_key_hash(&key))
    .fetch_optional(&state.pool)
    .await?;
    let Some((node_pk, node_org, node_video)) = node else {
        return Err(ApiError::unauthorized("Invalid API key"));
    };

    // Camera and node org are both checked, as defence in depth against
    // the two ever diverging.
    let camera: Option<(i32,)> = sqlx::query_as(
        "SELECT id FROM cameras WHERE camera_id = $1 AND node_id = $2 AND org_id = $3 LIMIT 1",
    )
    .bind(&camera_id)
    .bind(node_pk)
    .bind(&node_org)
    .fetch_optional(&state.pool)
    .await?;
    let Some((camera_pk,)) = camera else {
        return Err(ApiError::not_found("Camera not found"));
    };

    let bytes = body_bytes(request).await?;
    // Decoding AND `.get` sit inside the same `try` in Python, so a
    // non-object body is the same 400 as malformed JSON.
    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => map,
        _ => return Err(ApiError::bad_request("Invalid request body")),
    };
    let video = body.get("video_codec").cloned().unwrap_or(Value::Null);
    let audio = body.get("audio_codec").cloned().unwrap_or(Value::Null);

    if !pyrepr::truthy(&video) {
        return Err(ApiError::bad_request("video_codec is required"));
    }
    // A codec is a string of at most the column's 50 characters with no
    // line break — it is written into an HLS `CODECS` attribute verbatim.
    //
    // The Python allowed 64 (PYTHON_BUGS.md #3), so 51-64 characters
    // passed and then failed the column: a 500 on PostgreSQL, silently
    // stored on SQLite. A value with no length, or not a string at all,
    // was a TypeError and a 500 too. All of it is the caller's mistake,
    // so all of it is a 400 now, and SQLite and PostgreSQL agree.
    let valid = |v: &Value| -> Option<String> {
        let Value::String(s) = v else { return None };
        (s.chars().count() <= CODEC_MAX_CHARS && !s.contains(['\n', '\r'])).then(|| s.clone())
    };
    let Some(video) = valid(&video) else {
        return Err(ApiError::bad_request("Invalid video_codec format"));
    };
    let video = sanitize_video_codec(&video);

    let audio = match audio {
        a if !pyrepr::truthy(&a) => "mp4a.40.2".to_string(),
        a => match valid(&a) {
            Some(s) => s,
            None => return Err(ApiError::bad_request("Invalid audio_codec format")),
        },
    };

    let now = now_naive();
    let mut tx = crate::db::begin_write(&state.pool).await?;
    sqlx::query(
        "UPDATE cameras
            SET video_codec = $1, audio_codec = $2, codec_detected_at = $3, updated_at = $3
          WHERE id = $4",
    )
    .bind(&video)
    .bind(&audio)
    .bind(now)
    .bind(camera_pk)
    .execute(&mut *tx)
    .await?;

    // The node inherits the first codec any of its cameras reports.
    // `not node.video_codec` is true for an empty string as well as
    // NULL — and when it is false, SQLAlchemy emits no UPDATE for the
    // node at all, so its updated_at must not move either.
    if node_video.as_deref().unwrap_or("").is_empty() {
        sqlx::query(
            "UPDATE camera_nodes
                SET video_codec = $1, audio_codec = $2, codec_detected_at = $3, updated_at = $3
              WHERE id = $4",
        )
        .bind(&video)
        .bind(&audio)
        .bind(now)
        .bind(node_pk)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    Ok(Json(json!({ "success": true, "message": "Codec updated" })))
}

// ---------------------------------------------------------------------
// POST /api/nodes/{node_id}/rotate-key
// ---------------------------------------------------------------------

pub async fn rotate_api_key(
    rate: PerMinute<5>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Path(node_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let node_id = path_segment(&node_id)?.to_string();
    rate.check().await?;
    let row: Option<(i32, String)> = sqlx::query_as(
        "SELECT id, name FROM camera_nodes WHERE node_id = $1 AND org_id = $2 LIMIT 1",
    )
    .bind(&node_id)
    .bind(&user.org_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((id, name)) = row else {
        return Err(ApiError::not_found("Node not found"));
    };

    // The old key is invalid from this commit on. There is no in-band
    // notice to the node: its next heartbeat or register simply 403s,
    // and the rotation modal walks the operator through re-running
    // setup with the new key.
    let new_key = uuid::Uuid::new_v4().to_string();
    let rotated_at = truncate_to_micros(now_naive());
    sqlx::query(
        "UPDATE camera_nodes SET api_key_hash = $1, key_rotated_at = $2, updated_at = $2 WHERE id = $3",
    )
    .bind(node_key_hash(new_key.as_bytes()))
    .bind(rotated_at)
    .bind(id)
    .execute(&state.pool)
    .await?;
    // The old key must stop working now, not when a cache entry ages out.
    crate::hls::invalidate_auth_cache();

    write_audit(
        &state.pool,
        &user.org_id,
        "node_key_rotated",
        &user.user_id,
        &audit_label(&user),
        Some(python_json(&[
            ("node_id", json!(node_id)),
            ("name", json!(name)),
        ])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    Ok(Json(json!({
        "success": true,
        "node_id": node_id,
        "api_key": new_key,
        "key_rotated_at": iso_naive(rotated_at),
        "warning": "Store this API key securely. It cannot be retrieved again. Update your CameraNode config immediately.",
    })))
}

/// Postgres keeps microseconds. A response that echoes a timestamp it
/// just wrote must echo the stored value, not the nanosecond one.
fn truncate_to_micros(ts: chrono::NaiveDateTime) -> chrono::NaiveDateTime {
    use chrono::Timelike;
    ts.with_nanosecond(ts.nanosecond() / 1_000 * 1_000)
        .unwrap_or(ts)
}

// ---------------------------------------------------------------------
// POST /api/nodes
// ---------------------------------------------------------------------

/// How many node ids to try before giving up.
///
/// A node id is the first eight hex characters of a uuid4 — 32 bits,
/// deliberately short because the operator types it into the installer
/// — and the column is unique across *every* org. A collision is one
/// customer's new node landing on an id another customer holds: rare
/// per request (1 in 429,496 at 10k nodes), certain in aggregate, and
/// free to handle by retrying. Three in a row is not bad luck.
const NODE_ID_ATTEMPTS: usize = 3;

pub async fn create_node(
    rate: PerHour<20>,
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    ModelBody(RequireActiveBilling(user), body): ModelBody<RequireActiveBilling>,
) -> Result<Json<Value>, ApiError> {
    let mut errors = BodyErrors::new();
    let requested_name = errors.optional_string(&body, "name", 100);
    errors.finish()?;
    rate.check().await?;

    // The JWT's plan, not the effective plan: this matches the Python,
    // which reads `user.plan` here.
    let limits = plans::get_plan_limits(&user.plan);
    // Count and insert under one per-org lock. Counted outside it, two
    // creates at once (a double-clicked button, a retried request) both
    // saw room and both inserted, one past the plan's cap — and nothing
    // reconciles nodes the way enforce_camera_cap does cameras.
    let mut tx = crate::db::begin_write(&state.pool).await?;
    crate::db::lock_for_update(&mut tx, &format!("node-cap:{}", user.org_id)).await?;
    let (current,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM camera_nodes WHERE org_id = $1")
        .bind(&user.org_id)
        .fetch_one(&mut *tx)
        .await?;
    if current >= limits.max_nodes {
        return Err(ApiError::forbidden(format!(
            "Node limit reached ({} on {} plan). Upgrade your plan to add more nodes.",
            limits.max_nodes,
            plans::get_plan_display_name(&user.plan)
        )));
    }

    let api_key = uuid::Uuid::new_v4().to_string();
    let api_key_hash = node_key_hash(api_key.as_bytes());

    let mut created: Option<(String, String)> = None;
    for attempt in 0..NODE_ID_ATTEMPTS {
        let node_id: String = uuid::Uuid::new_v4().to_string().chars().take(8).collect();
        // `data.name or f"Node-{node_id}"` — an empty name takes the
        // default too, and it is recomputed per attempt.
        let name = requested_name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("Node-{node_id}"));
        let now = now_naive();
        // Columns SQLAlchemy fills from Python-side defaults are written
        // explicitly: http_port and both timestamps. The side-effect
        // snapshot compares every column of the new row.
        // A savepoint per attempt: in PostgreSQL a failed statement
        // aborts the whole transaction, so retrying after a collision
        // needs something to roll back to.
        let mut attempt_tx = sqlx::Acquire::begin(&mut tx).await?;
        let inserted = sqlx::query(
            "INSERT INTO camera_nodes
                (node_id, org_id, name, api_key_hash, status, http_port, created_at, updated_at)
             VALUES ($1, $2, $3, $4, 'pending', 8080, $5, $5)",
        )
        .bind(&node_id)
        .bind(&user.org_id)
        .bind(&name)
        .bind(&api_key_hash)
        .bind(now)
        .execute(&mut *attempt_tx)
        .await;
        match inserted {
            Ok(_) => {
                attempt_tx.commit().await?;
                created = Some((node_id, name));
                break;
            }
            // The unique constraint is the arbiter, not a pre-check
            // SELECT, which would race two concurrent creates.
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                attempt_tx.rollback().await?;
                tracing::warn!(
                    node_id,
                    attempt = attempt + 1,
                    "node_id collision — regenerating"
                );
            }
            Err(err) => return Err(err.into()),
        }
    }
    tx.commit().await?;
    let Some((node_id, name)) = created else {
        tracing::error!("node creation failed after {NODE_ID_ATTEMPTS} id attempts");
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Could not allocate a node ID. Please try again.",
        ));
    };

    write_audit(
        &state.pool,
        &user.org_id,
        "node_created",
        &user.user_id,
        &audit_label(&user),
        Some(python_json(&[
            ("node_id", json!(node_id)),
            ("name", json!(name)),
        ])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    Ok(Json(json!({
        "success": true,
        "node_id": node_id,
        "name": name,
        "api_key": api_key,
        "warning": "Store this API key securely. It cannot be retrieved again.",
    })))
}

// ---------------------------------------------------------------------
// POST /api/settings/danger/wipe-logs
// ---------------------------------------------------------------------

/// `_require_active_paid_plan`: the JWT feature AND the resolved plan.
///
/// The `features` claim refreshes about once a minute, so a user who
/// just downgraded still carries `admin` in their token. The resolved
/// plan is the point of truth; the claim is the cheap pre-filter.
async fn require_active_paid_plan(
    state: &AppState,
    user: &crate::auth::claims::AuthUser,
) -> Result<(), ApiError> {
    if !user.features.iter().any(|f| f == "admin") {
        return Err(ApiError::forbidden(
            "Danger zone requires a Pro or Pro Plus plan.",
        ));
    }
    let ctx = PlanContext {
        pool: &state.pool,
        client: &state.http,
        clerk_base_url: &state.config.clerk_api_url,
        clerk_secret: &state.config.clerk_secret_key,
        local_auth: state.config.is_local_auth(),
    };
    let plan = plans::effective_plan_for_caps(&ctx, &user.org_id, true).await;
    if !PAID_PLAN_SLUGS.contains(&plan.as_str()) {
        return Err(ApiError::forbidden(
            "Danger zone requires an active paid plan; the current billing record for \
             this organization doesn't include this feature.  If you just upgraded, sign \
             out and back in to refresh your session.",
        ));
    }
    Ok(())
}

/// Permanently delete the org's stream-access and MCP activity logs.
///
/// Operator convenience on a paid plan, not the GDPR erasure path —
/// that is `/full-reset`, available on every plan.
pub async fn wipe_stream_logs(
    rate: PerHour<5>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    // `_require_active_paid_plan` is a call inside the Python function.
    rate.check().await?;
    require_active_paid_plan(&state, &user).await?;

    let mut tx = crate::db::begin_write(&state.pool).await?;
    let stream = sqlx::query("DELETE FROM stream_access_logs WHERE org_id = $1")
        .bind(&user.org_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let mcp = sqlx::query("DELETE FROM mcp_activity_logs WHERE org_id = $1")
        .bind(&user.org_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    tracing::warn!(
        stream,
        mcp,
        "admin wiped stream and MCP logs (org redacted)"
    );

    write_audit(
        &state.pool,
        &user.org_id,
        "logs_wiped",
        &user.user_id,
        &audit_label(&user),
        Some(python_json(&[
            ("stream_logs_deleted", json!(stream)),
            ("mcp_logs_deleted", json!(mcp)),
        ])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    Ok(Json(json!({
        "success": true,
        "deleted_logs": stream,
        "deleted_mcp_logs": mcp,
    })))
}

/// `DELETE /api/nodes/{node_id}` — remove a node and everything on it.
///
/// The node is asked to wipe its own local data first, and whether it
/// answered is recorded rather than required: a node that is offline,
/// or that never replies, is deleted anyway. Leaving a server-side
/// record behind because the hardware is unreachable would make
/// "remove this node" impossible exactly when it is most wanted.
///
/// The cameras are deleted explicitly. The foreign key carries no
/// `ON DELETE CASCADE`; what removes them in the Python is
/// SQLAlchemy's `cascade="all, delete-orphan"`, which issues the child
/// deletes itself — so a port that relied on the database would leave
/// every camera behind, pointing at a node that no longer exists.
pub async fn delete_node(
    rate: PerHour<20>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(node_id): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
) -> Result<Json<Value>, ApiError> {
    rate.check().await?;
    let node_id = crate::query::path_segment(&node_id)?;

    let node: Option<(i32, Option<String>)> =
        sqlx::query_as("SELECT id, name FROM camera_nodes WHERE node_id = $1 AND org_id = $2")
            .bind(node_id)
            .bind(&user.org_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some((node_pk, node_name)) = node else {
        return Err(ApiError::not_found("Node not found"));
    };

    // Ten seconds, and every failure is survivable: an offline node,
    // one that answers something other than success, or one that
    // answers nothing at all.
    let wiped = matches!(
        crate::ws::MANAGER
            .send_command(
                node_id,
                "wipe_data",
                json!({}),
                std::time::Duration::from_secs(10),
            )
            .await,
        Ok(result) if result.get("status") == Some(&Value::String("success".to_string()))
    );
    if wiped {
        tracing::info!(node_id, "Node acknowledged local data wipe");
    } else {
        tracing::warn!(node_id, "Could not wipe node (may be offline)");
    }

    let cameras: Vec<(String,)> =
        sqlx::query_as("SELECT camera_id FROM cameras WHERE node_id = $1")
            .bind(node_pk)
            .fetch_all(&state.pool)
            .await?;
    for (camera_id,) in &cameras {
        state.hls.cleanup_camera(camera_id);
    }

    let mut tx = crate::db::begin_write(&state.pool).await?;
    sqlx::query("DELETE FROM cameras WHERE node_id = $1")
        .bind(node_pk)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM camera_nodes WHERE id = $1")
        .bind(node_pk)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    // A deleted node's key and cameras must not authenticate from cache.
    crate::hls::invalidate_auth_cache();

    write_audit(
        &state.pool,
        &user.org_id,
        "node_deleted",
        &user.user_id,
        &audit_label(&user),
        Some(python_json(&[
            ("node_id", json!(node_id)),
            ("name", node_name.map_or(Value::Null, Value::String)),
            ("node_wiped", json!(wiped)),
        ])),
        &headers,
        Some(&peer.ip().to_string()),
    )
    .await;

    Ok(Json(json!({
        "success": true,
        "deleted": node_id,
        "node_wiped": wiped,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_video_codec_matches_the_python_module() {
        // Expected outputs produced by calling app.core.codec directly.
        let corpus: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/pyrepr_corpus.json")).unwrap();
        let codecs = corpus["codecs"].as_array().unwrap();
        assert!(codecs.len() > 10);
        for row in codecs {
            let input = row["in"].as_str().unwrap();
            assert_eq!(
                sanitize_video_codec(input),
                row["out"].as_str().unwrap(),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn a_node_key_is_hashed_as_utf8_of_the_latin1_decoded_header() {
        // The byte 0xFF on the wire is U+00FF to Starlette, and
        // `.encode()` makes that two bytes. Values from hashlib.
        assert_eq!(
            node_key_hash(b"node-key-\xff"),
            "513ceeab86d874d7de558cef2a9f5b8d10659f3c162d0410d12ccb6c65dc1372"
        );
        assert_eq!(
            node_key_hash(b"test-node-key"),
            "f3702f9692e7bce4e7dc0b10fe460daf0bdc6c2c741d4c2fabcdb6df44dbb4c9"
        );
    }
}
