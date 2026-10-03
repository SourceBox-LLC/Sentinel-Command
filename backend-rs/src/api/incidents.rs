//! Incident reports — reads, plus the two writes that touch nothing but
//! the database.
//!
//! Ported from `backend/app/api/incidents.py`.
//!
//! `POST /api/incidents` is **not** ported. It fires an inbox + email
//! notification through `create_notification`, which is a side effect
//! beyond this table and belongs with the email work in slice 7. Porting
//! the response without the notification would look correct in a
//! differential and silently stop telling operators that an incident was
//! filed.

use axum::extract::{Path, Request, State};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::NaiveDateTime;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::models::{iso_naive, now_naive};
use crate::query::{int4, path_int, BodyErrors, ModelBody, Query};
use crate::ratelimit::PerMinute;

const SEVERITIES: [&str; 4] = ["low", "medium", "high", "critical"];
const STATUSES: [&str; 4] = ["open", "acknowledged", "resolved", "dismissed"];

#[derive(Debug, sqlx::FromRow)]
pub struct IncidentRow {
    pub id: i32,
    pub camera_id: Option<String>,
    pub title: String,
    pub summary: String,
    pub report: Option<String>,
    pub severity: String,
    pub status: String,
    pub created_by: String,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
    pub resolved_at: Option<NaiveDateTime>,
    pub resolved_by: Option<String>,
    pub evidence_count: i64,
}

impl IncidentRow {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "camera_id": self.camera_id,
            "title": self.title,
            "summary": self.summary,
            // `self.report or ""` — a NULL report is an empty string on
            // the wire, not null.
            "report": self.report.clone().unwrap_or_default(),
            "severity": self.severity,
            "status": self.status,
            "created_by": self.created_by,
            "created_at": self.created_at.map(iso_naive),
            "updated_at": self.updated_at.map(iso_naive),
            "resolved_at": self.resolved_at.map(iso_naive),
            "resolved_by": self.resolved_by,
            "evidence_count": self.evidence_count,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
pub struct EvidenceRow {
    pub id: i32,
    pub incident_id: i32,
    pub kind: String,
    pub text: Option<String>,
    pub camera_id: Option<String>,
    pub data_mime: Option<String>,
    pub timestamp: Option<NaiveDateTime>,
}

impl EvidenceRow {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "incident_id": self.incident_id,
            "kind": self.kind,
            "text": self.text,
            "camera_id": self.camera_id,
            // Derived from data_mime, never from the blob: `data` is a
            // deferred column and touching it would load megabytes per
            // row, which is the whole reason it is deferred.
            "has_data": self.data_mime.is_some(),
            "data_mime": self.data_mime,
            "timestamp": self.timestamp.map(iso_naive),
        })
    }
}

pub const INCIDENT_SELECT: &str = r#"
    SELECT i.id, i.camera_id, i.title, i.summary, i.report, i.severity, i.status,
           i.created_by, i.created_at, i.updated_at, i.resolved_at, i.resolved_by,
           (SELECT COUNT(*) FROM incident_evidence e WHERE e.incident_id = i.id)
               AS evidence_count
      FROM incidents i
"#;

async fn evidence_for(pool: &crate::db::Pool, incident_id: i32) -> Result<Vec<Value>, ApiError> {
    // Ordered by timestamp, matching the relationship's `order_by`.
    // Never selects `data`.
    let rows: Vec<EvidenceRow> = sqlx::query_as(
        "SELECT id, incident_id, kind, text, camera_id, data_mime, timestamp \
           FROM incident_evidence WHERE incident_id = $1 ORDER BY timestamp NULLS LAST",
    )
    .bind(incident_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(EvidenceRow::to_json).collect())
}

async fn owned_incident(
    pool: &crate::db::Pool,
    org_id: &str,
    incident_id: i32,
) -> Result<IncidentRow, ApiError> {
    sqlx::query_as(&format!(
        "{INCIDENT_SELECT} WHERE i.id = $1 AND i.org_id = $2"
    ))
    .bind(incident_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    // 404 for "not yours" as well as "not there" — distinguishing them
    // would confirm another tenant's incident ids.
    .ok_or_else(|| ApiError::not_found("Incident not found"))
}

/// A required string with Pydantic's length bounds.
///
/// `min` and `max` cannot both fire — a string is not at once too short
/// and too long — so the order between them is not a decision. The
/// order that *is* one is across fields: Pydantic reports them in
/// declaration order, and the envelope's `message` names the first.
fn bounded_string(
    errors: &mut BodyErrors,
    body: &Value,
    field: &str,
    min: usize,
    max: Option<usize>,
) -> String {
    match body.get(field) {
        None => {
            errors.missing(field, body);
            String::new()
        }
        Some(Value::String(s)) => {
            let length = s.chars().count();
            if length < min {
                errors.too_short(field, s, min);
            } else if max.is_some_and(|max| length > max) {
                errors.too_long(field, s, max.unwrap());
            }
            s.clone()
        }
        Some(other) => {
            errors.string_type(field, other);
            String::new()
        }
    }
}

/// `POST /api/incidents` — a human files one.
///
/// Mirrors the MCP `create_incident` tool's validation and fires the
/// same `incident_created` notification, so the inbox and email
/// channels do not care which author wrote the row. The difference is
/// `created_by`: `user:<clerk id>` here where the agent writes
/// `mcp:<key name>`, which is what the dashboard badges off.
///
/// The `meta` sent with the notification deliberately carries no
/// `created_by`, and that is load-bearing rather than an omission: the
/// Sentinel dispatcher refuses to re-trigger on an incident an agent
/// filed, so leaving the key out is what lets a *human*-filed incident
/// wake the agent.
pub async fn create_incident(
    rate: PerMinute<60>,
    State(state): State<AppState>,
    ModelBody(RequireAdmin(user), body): ModelBody<RequireAdmin>,
) -> Result<Response, ApiError> {
    let mut errors = BodyErrors::new();
    let title = bounded_string(&mut errors, &body, "title", 1, Some(200));
    // No upper bound on the summary: an incident narrative is a `Text`
    // column and the model sets only `min_length`.
    let summary = bounded_string(&mut errors, &body, "summary", 1, None);
    // `severity: str = Field(default="medium")` — a plain `str` with a
    // default, so an absent key takes the default but an explicit null
    // is a type error.
    let severity = match body.get("severity") {
        None => "medium".to_string(),
        Some(Value::String(value)) => value.clone(),
        Some(other) => {
            errors.string_type("severity", other);
            String::new()
        }
    };
    let camera_id = errors.optional_string(&body, "camera_id", usize::MAX);
    errors.finish()?;
    // After validation, before anything this function raises — which is
    // where slowapi's decorator sits. FastAPI resolves the body first,
    // so a 422 costs the org nothing, while the 400s below have already
    // spent their slot.
    rate.check().await?;

    // Pydantic passes anything of the right type; the enum is the
    // handler's own check, and it runs before the emptiness checks.
    if !SEVERITIES.contains(&severity.as_str()) {
        return Err(ApiError::bad_request(format!(
            "Invalid severity: {severity}"
        )));
    }

    let title = title.trim().to_string();
    let summary = summary.trim().to_string();
    if title.is_empty() {
        return Err(ApiError::bad_request("title is required"));
    }
    if summary.is_empty() {
        return Err(ApiError::bad_request("summary is required"));
    }

    // `if body.camera_id:` — the empty string is falsy, so it skips the
    // lookup. It is still *stored*, because Python passes the original
    // value to the model and not the one the guard tested. Filtering it
    // here instead wrote NULL where Python writes '', in the incident
    // row, in the notification beside it and in the response.
    if let Some(camera_id) = camera_id.as_deref().filter(|id| !id.is_empty()) {
        let known: Option<(String,)> =
            sqlx::query_as("SELECT camera_id FROM cameras WHERE org_id = $1 AND camera_id = $2")
                .bind(&user.org_id)
                .bind(camera_id)
                .fetch_optional(&state.pool)
                .await?;
        if known.is_none() {
            return Err(ApiError::bad_request(format!(
                "Camera '{camera_id}' not found"
            )));
        }
    }

    // `title[:200]` again after the strip. Pydantic already refused a
    // longer one, so this only ever matters if that bound moves.
    let stored_title: String = title.chars().take(200).collect();
    // SQLAlchemy evaluates the two `default=` callables separately, so
    // the row's two timestamps differ by a few microseconds.
    let created_at = now_naive();
    let updated_at = now_naive();
    let (incident_id,): (i32,) = sqlx::query_as(
        // `report` is not left to the column: the model declares
        // `default=""`, so SQLAlchemy writes an empty string where an
        // unbound column would be NULL — and the read path returns
        // `self.report or ""`, which hides the difference in the
        // response while the stored row still differs.
        "INSERT INTO incidents
            (org_id, camera_id, title, summary, report, severity, status, created_by,
             created_at, updated_at)
         VALUES ($1, $2, $3, $4, '', $5, 'open', $6, $7, $8)
         RETURNING id",
    )
    .bind(&user.org_id)
    .bind(camera_id.as_deref())
    .bind(&stored_title)
    .bind(&summary)
    .bind(&severity)
    .bind(format!("user:{}", user.user_id))
    .bind(created_at)
    .bind(updated_at)
    .fetch_one(&state.pool)
    .await?;

    // The notification is best-effort: the row is already committed and
    // the operator has clicked submit, so a failure here must not turn
    // a filed incident into an error.
    let notification = crate::notifications::NewNotification::new(
        "incident_created",
        format!("Incident #{incident_id}: {stored_title}"),
    )
    .body(format!("[{}] {summary}", severity.to_uppercase()))
    .severity(if matches!(severity.as_str(), "high" | "critical") {
        "critical"
    } else {
        "warning"
    })
    .audience("all")
    .link(format!("/incidents/{incident_id}"))
    .meta(json!({"incident_id": incident_id, "severity": severity}));
    let notification = match camera_id.as_deref() {
        Some(camera_id) => notification.camera(camera_id),
        None => notification,
    };
    crate::notifications::create_notification(&state, &user.org_id, notification).await;

    let row = owned_incident(&state.pool, &user.org_id, incident_id).await?;
    let mut out = row.to_json();
    out["evidence"] = Value::Array(Vec::new());
    Ok((axum::http::StatusCode::CREATED, Json(out)).into_response())
}

/// `GET /api/incidents`.
pub async fn list_incidents(
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let mut q = Query::parse(request.uri().query());
    let status = q.optional_str("status");
    let severity = q.optional_str("severity");
    let camera_id = q.optional_str("camera_id");
    let limit = q.int("limit", 50, 1, 200);
    let offset = q.int("offset", 0, 0, 1_000_000);
    q.finish()?;

    // These two are validated in the handler body rather than by
    // Pydantic, so an unknown value is a 400 with a plain message — not
    // the 422 envelope the numeric bounds produce.
    if let Some(ref s) = status {
        if !STATUSES.contains(&s.as_str()) {
            return Err(ApiError::bad_request(format!("Invalid status: {s}")));
        }
    }
    if let Some(ref s) = severity {
        if !SEVERITIES.contains(&s.as_str()) {
            return Err(ApiError::bad_request(format!("Invalid severity: {s}")));
        }
    }

    let mut where_sql = String::from(" WHERE i.org_id = $1");
    let mut n = 1;
    for (present, column) in [
        (status.is_some(), "i.status"),
        (severity.is_some(), "i.severity"),
        (camera_id.is_some(), "i.camera_id"),
    ] {
        if present {
            n += 1;
            where_sql.push_str(&format!(" AND {column} = ${n}"));
        }
    }

    let count_sql = format!("SELECT COUNT(*) FROM incidents i{where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql).bind(&user.org_id);
    for v in [&status, &severity, &camera_id].into_iter().flatten() {
        cq = cq.bind(v.clone());
    }
    let total: i64 = cq.fetch_one(&state.pool).await?;

    let page_sql = format!(
        "{INCIDENT_SELECT}{where_sql} ORDER BY i.created_at DESC NULLS FIRST LIMIT {limit} OFFSET {offset}"
    );
    let mut pq = sqlx::query_as::<_, IncidentRow>(&page_sql).bind(&user.org_id);
    for v in [&status, &severity, &camera_id].into_iter().flatten() {
        pq = pq.bind(v.clone());
    }
    let rows: Vec<IncidentRow> = pq.fetch_all(&state.pool).await?;

    Ok(Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "incidents": rows.iter().map(IncidentRow::to_json).collect::<Vec<_>>(),
    })))
}

/// `GET /api/incidents/counts` — the stat bar.
pub async fn incident_counts(
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
) -> Result<Json<Value>, ApiError> {
    let row: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE status = 'open'),
                COUNT(*) FILTER (WHERE status = 'open' AND severity = 'critical'),
                COUNT(*) FILTER (WHERE status = 'open' AND severity = 'high'),
                COUNT(*)
           FROM incidents WHERE org_id = $1",
    )
    .bind(&user.org_id)
    .fetch_one(&state.pool)
    .await?;

    Ok(Json(json!({
        "open": row.0,
        "open_critical": row.1,
        "open_high": row.2,
        "total": row.3,
    })))
}

/// `GET /api/incidents/{incident_id}`.
pub async fn get_incident(
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(incident_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let incident_id = int4(path_int("incident_id", &incident_id)?)?;
    let row = owned_incident(&state.pool, &user.org_id, incident_id).await?;
    let mut out = row.to_json();
    out["evidence"] = Value::Array(evidence_for(&state.pool, incident_id).await?);
    Ok(Json(out))
}

/// Starlette's `Response.init_headers` appends `; charset=utf-8` to any
/// `media_type` that starts with lowercase `text/` and does not already
/// name a charset. Reproduced here because `data_mime` comes out of the
/// database, so the branch is data-dependent rather than decided at the
/// call site.
fn starlette_content_type(media_type: &str) -> String {
    if media_type.starts_with("text/") && !media_type.to_ascii_lowercase().contains("charset=") {
        format!("{media_type}; charset=utf-8")
    } else {
        media_type.to_string()
    }
}

#[derive(Debug, sqlx::FromRow)]
struct EvidenceBlobRow {
    data: Option<Vec<u8>>,
    data_mime: Option<String>,
    kind: String,
}

/// The evidence row for `{incident_id}/{evidence_id}`, after the parent
/// incident's ownership has been checked.
///
/// `Ok(None)` means the incident is the caller's but that evidence row
/// is not there; the two callers word their own 404 for that case. An
/// `Err` is either the parent incident's own 404 — which carries
/// **"Incident not found"**, not the evidence message, because Python
/// raises out of `_get_owned_incident` before it ever queries the
/// evidence table — or a database failure, which must stay a 500 rather
/// than being flattened into a 404.
async fn owned_evidence(
    pool: &crate::db::Pool,
    org_id: &str,
    incident_id: i32,
    evidence_id: i32,
) -> Result<Option<EvidenceBlobRow>, ApiError> {
    // Ownership of the *parent* is the org check; the evidence row
    // carries no org_id of its own. Checking it first means a foreign
    // incident id 404s before any blob leaves the database.
    owned_incident(pool, org_id, incident_id).await?;

    Ok(sqlx::query_as(
        "SELECT data, data_mime, kind FROM incident_evidence
          WHERE id = $1 AND incident_id = $2",
    )
    .bind(evidence_id)
    .bind(incident_id)
    .fetch_optional(pool)
    .await?)
}

/// `GET /api/incidents/{incident_id}/evidence/{evidence_id}` — the
/// snapshot or clip bytes.
///
/// Rate limited because it serves arbitrary-size video out of the
/// database and bypasses the viewer-hour cap that gates the live HLS
/// endpoints; without a cap it is a bandwidth tap.
pub async fn get_evidence_blob(
    rate: PerMinute<120>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path((incident_id, evidence_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let incident_id = path_int("incident_id", &incident_id)?;
    let evidence_id = path_int("evidence_id", &evidence_id)?;
    rate.check().await?;
    let (incident_id, evidence_id) = (int4(incident_id)?, int4(evidence_id)?);

    // `not evidence.data` in Python is falsy for zero bytes as well as
    // for NULL, so an observation row and an empty blob are both 404.
    let evidence = owned_evidence(&state.pool, &user.org_id, incident_id, evidence_id).await?;
    let Some(EvidenceBlobRow {
        data: Some(data),
        data_mime,
        ..
    }) = evidence
    else {
        return Err(ApiError::not_found("Evidence blob not found"));
    };
    if data.is_empty() {
        return Err(ApiError::not_found("Evidence blob not found"));
    }

    // MIME parameters are stripped: clips are stored as
    // `video/mp2t;duration=N` to carry length without a schema
    // migration, and browsers do not need it.
    let raw = data_mime.unwrap_or_default();
    let media_type = match raw.split(';').next().unwrap_or("").trim() {
        "" => "application/octet-stream",
        m => m,
    };

    let Ok(content_type) = HeaderValue::from_str(&starlette_content_type(media_type)) else {
        // A `data_mime` carrying control characters would be rejected by
        // the Python's HTTP writer rather than sent, which surfaces as a
        // 500. Nothing in the API can set one — `data_mime` is written
        // only by the MCP capture tools — so this is a guard, not a path.
        tracing::error!(media_type, "evidence data_mime is not a valid header value");
        return Err(ApiError::internal("Internal Server Error"));
    };

    Ok(blob_response(content_type, data))
}

/// A single-segment VOD playlist so the dashboard can play a clip
/// through hls.js, with the same auth as the live player.
///
/// `#EXT-X-TARGETDURATION` must be >= every `#EXTINF` (RFC 8216 §4.3.3.1),
/// which is why the fallback duration is generous rather than zero.
pub async fn get_evidence_playlist(
    rate: PerMinute<120>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path((incident_id, evidence_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let incident_id = path_int("incident_id", &incident_id)?;
    let evidence_id = path_int("evidence_id", &evidence_id)?;
    rate.check().await?;
    let (incident_id, evidence_id) = (int4(incident_id)?, int4(evidence_id)?);

    let evidence = owned_evidence(&state.pool, &user.org_id, incident_id, evidence_id).await?;
    let Some(EvidenceBlobRow {
        data: Some(data),
        data_mime,
        kind,
    }) = evidence
    else {
        return Err(ApiError::not_found("Clip not found"));
    };
    if data.is_empty() || kind != "clip" {
        return Err(ApiError::not_found("Clip not found"));
    }

    let duration = clip_duration(data_mime.as_deref().unwrap_or(""));
    let Some(target_duration) = target_duration(duration) else {
        // Python's `int(float("inf"))` raises OverflowError and
        // `int(float("nan"))` raises ValueError; either way the request
        // ends as an unhandled 500.
        tracing::error!(duration, "clip duration is not representable");
        return Err(ApiError::internal("Internal Server Error"));
    };

    // An absolute segment URL, not a relative one: the playlist lives at
    // `.../playlist.m3u8`, so relative resolution in hls.js would land a
    // path segment too deep.
    let playlist = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:3\n\
         #EXT-X-TARGETDURATION:{target_duration}\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXTINF:{duration:.3},\n\
         /api/incidents/{incident_id}/evidence/{evidence_id}\n\
         #EXT-X-ENDLIST\n"
    );

    Ok(blob_response(
        HeaderValue::from_static("application/vnd.apple.mpegurl"),
        playlist.into_bytes(),
    ))
}

/// Both evidence routes answer with the same private cache policy: the
/// bytes are tenant data, so a shared cache must not hold them, but a
/// browser replaying a clip should not refetch it every seek.
fn blob_response(content_type: HeaderValue, body: Vec<u8>) -> Response {
    use axum::response::IntoResponse;

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=300"),
    );
    headers.insert(axum::http::header::CONTENT_TYPE, content_type);
    (headers, body).into_response()
}

/// Pull `duration=` back out of a stored `video/mp2t;duration=N`.
///
/// Python reads it with bare `float()`, which is looser than it looks —
/// it accepts surrounding whitespace, a leading sign, digit-group
/// underscores and `inf`/`nan`. A parameter that fails to parse is
/// skipped (Python catches ValueError and moves on), and a later
/// `duration=` wins over an earlier one because the loop keeps going.
fn clip_duration(raw_mime: &str) -> f64 {
    // The default is well above any real clip: `#EXT-X-TARGETDURATION`
    // has to be >= the `#EXTINF` it covers, so guessing low would emit
    // an invalid playlist while guessing high only costs buffering.
    let mut duration = 60.0_f64;
    if !raw_mime.contains(';') {
        return duration;
    }
    for param in raw_mime.split(';').skip(1) {
        let param = param.trim();
        if let Some(value) = param.strip_prefix("duration=") {
            if let Some(parsed) = crate::pyrepr::python_float(value) {
                duration = parsed;
            }
        }
    }
    duration
}

/// `max(1, int(duration) + 1)` — `int()` truncates toward zero, so a
/// negative duration lands on the floor of 1 rather than going negative.
///
/// `None` where Python raises: `int()` rejects infinities and NaN.
fn target_duration(duration: f64) -> Option<i64> {
    if !duration.is_finite() {
        return None;
    }
    let truncated = duration.trunc();
    // Python integers are unbounded, so a duration past i64 has no exact
    // answer here. It is not reachable through the API — `data_mime` is
    // written only by the MCP capture tools — and saturating keeps a
    // hand-written value from wrapping into a negative target.
    if truncated >= i64::MAX as f64 {
        return Some(i64::MAX);
    }
    Some((truncated as i64).saturating_add(1).max(1))
}

#[derive(Debug, Deserialize, Default)]
pub struct IncidentPatch {
    status: Option<String>,
    severity: Option<String>,
    summary: Option<String>,
    report: Option<String>,
}

/// `PATCH /api/incidents/{incident_id}` — acknowledge, resolve, dismiss.
pub async fn update_incident(
    rate: PerMinute<120>,
    State(state): State<AppState>,
    Path(incident_id): Path<String>,
    ModelBody(RequireAdmin(user), body): ModelBody<RequireAdmin>,
) -> Result<Json<Value>, ApiError> {
    let patch: IncidentPatch = serde_json::from_value(body).unwrap_or_default();
    let incident_id = path_int("incident_id", &incident_id)?;
    rate.check().await?;
    let incident_id = int4(incident_id)?;
    let incident = owned_incident(&state.pool, &user.org_id, incident_id).await?;

    let mut status = incident.status.clone();
    let mut resolved_at = incident.resolved_at;
    let mut resolved_by = incident.resolved_by.clone();

    if let Some(ref new_status) = patch.status {
        if !STATUSES.contains(&new_status.as_str()) {
            return Err(ApiError::bad_request(format!(
                "Invalid status: {new_status}"
            )));
        }
        let terminal = matches!(new_status.as_str(), "resolved" | "dismissed");
        let was_terminal = matches!(incident.status.as_str(), "resolved" | "dismissed");
        if terminal && !was_terminal {
            // Stamped only on the *transition* into a terminal state, so
            // re-resolving an already-resolved incident keeps the
            // original resolver and time.
            resolved_at = Some(now_naive());
            resolved_by = Some(format!("user:{}", user.user_id));
        } else if new_status == "open" {
            // Re-opening clears the resolution outright.
            resolved_at = None;
            resolved_by = None;
        }
        status = new_status.clone();
    }

    let mut severity = incident.severity.clone();
    if let Some(ref new_severity) = patch.severity {
        if !SEVERITIES.contains(&new_severity.as_str()) {
            return Err(ApiError::bad_request(format!(
                "Invalid severity: {new_severity}"
            )));
        }
        severity = new_severity.clone();
    }

    let summary = patch
        .summary
        .clone()
        .unwrap_or_else(|| incident.summary.clone());
    let report = match patch.report {
        Some(ref r) => Some(r.clone()),
        None => incident.report.clone(),
    };

    // SQLAlchemy emits no UPDATE at all when nothing actually changed,
    // so `updated_at` — an `onupdate` column — keeps its old value. An
    // unconditional UPDATE here would bump it on every no-op PATCH, and
    // the dashboard sorts and badges on that field.
    //
    // This also covers patching a field to the value it already holds:
    // the session's dirty check compares old against new, so setting
    // severity="high" on an already-high incident is not a change
    // either. Comparing computed values against the current row
    // reproduces both cases.
    let unchanged = status == incident.status
        && severity == incident.severity
        && summary == incident.summary
        && report == incident.report
        && resolved_at == incident.resolved_at
        && resolved_by == incident.resolved_by;

    if !unchanged {
        sqlx::query(
            "UPDATE incidents
                SET status = $1, severity = $2, summary = $3, report = $4,
                    resolved_at = $5, resolved_by = $6, updated_at = $7
              WHERE id = $8 AND org_id = $9",
        )
        .bind(&status)
        .bind(&severity)
        .bind(&summary)
        .bind(&report)
        .bind(resolved_at)
        .bind(&resolved_by)
        .bind(now_naive())
        .bind(incident_id)
        .bind(&user.org_id)
        .execute(&state.pool)
        .await?;
    }

    let updated = owned_incident(&state.pool, &user.org_id, incident_id).await?;
    let mut out = updated.to_json();
    out["evidence"] = Value::Array(evidence_for(&state.pool, incident_id).await?);
    Ok(Json(out))
}

/// `DELETE /api/incidents/{incident_id}`.
///
/// Evidence rows go with it via `ON DELETE CASCADE`, which is on the
/// foreign key in the schema — SQLAlchemy's `cascade="all, delete-orphan"`
/// would otherwise do it in Python, and only one of the two needs to.
pub async fn delete_incident(
    rate: PerMinute<60>,
    State(state): State<AppState>,
    RequireAdmin(user): RequireAdmin,
    Path(incident_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let incident_id = path_int("incident_id", &incident_id)?;
    rate.check().await?;
    let incident_id = int4(incident_id)?;
    // Fetched first so a missing or other-tenant incident 404s before
    // anything is deleted.
    owned_incident(&state.pool, &user.org_id, incident_id).await?;

    sqlx::query("DELETE FROM incidents WHERE id = $1 AND org_id = $2")
        .bind(incident_id)
        .bind(&user.org_id)
        .execute(&state.pool)
        .await?;

    Ok(Json(json!({ "deleted": incident_id })))
}

#[cfg(test)]
mod evidence_tests {
    use super::*;

    #[test]
    fn a_missing_duration_parameter_falls_back_to_sixty() {
        // #EXT-X-TARGETDURATION must be >= every #EXTINF it covers, so
        // the fallback deliberately guesses high.
        assert_eq!(clip_duration(""), 60.0);
        assert_eq!(clip_duration("video/mp2t"), 60.0);
        assert_eq!(clip_duration("video/mp2t;codecs=avc1"), 60.0);
        // Case matters: Python tests `startswith("duration=")`.
        assert_eq!(clip_duration("video/mp2t;Duration=5"), 60.0);
    }

    #[test]
    fn the_duration_parameter_is_read_the_way_python_reads_it() {
        assert_eq!(clip_duration("video/mp2t;duration=12.5"), 12.5);
        assert_eq!(clip_duration("video/mp2t; duration=12.5"), 12.5);
        assert_eq!(clip_duration("video/mp2t;duration= 12.5"), 12.5);
        // A later parameter wins: Python's loop does not break.
        assert_eq!(clip_duration("video/mp2t;duration=5;duration=9"), 9.0);
        // An unparseable value is skipped, leaving whatever came before.
        assert_eq!(clip_duration("video/mp2t;duration=5;duration=x"), 5.0);
        assert_eq!(clip_duration("video/mp2t;duration=oops"), 60.0);
    }

    #[test]
    fn target_duration_truncates_toward_zero_and_floors_at_one() {
        // Cross-checked against `max(1, int(d) + 1)` in CPython.
        assert_eq!(target_duration(12.5), Some(13));
        assert_eq!(target_duration(59.9996), Some(60));
        assert_eq!(target_duration(0.0), Some(1));
        assert_eq!(target_duration(0.4), Some(1));
        // int() truncates toward zero, so -3.7 -> -3 -> -2 -> floored.
        assert_eq!(target_duration(-3.7), Some(1));
    }

    #[test]
    fn a_non_finite_duration_is_a_five_hundred_not_a_playlist() {
        // Python: int(inf) raises OverflowError, int(nan) raises
        // ValueError. Either one leaves the request as an unhandled 500,
        // so emitting a playlist here would be a divergence, not a fix.
        assert_eq!(target_duration(f64::INFINITY), None);
        assert_eq!(target_duration(f64::NEG_INFINITY), None);
        assert_eq!(target_duration(f64::NAN), None);
    }

    #[test]
    fn extinf_is_formatted_to_three_places_like_python() {
        // Both languages round the exact binary value half-to-even, so
        // 1.0005 goes down and 2.0005 goes up. These are the values
        // CPython's "%.3f" produces.
        for (value, expected) in [
            (12.5, "12.500"),
            (59.9996, "60.000"),
            (1.0005, "1.000"),
            (2.0005, "2.001"),
            (-3.7, "-3.700"),
        ] {
            assert_eq!(format!("{value:.3}"), expected, "value {value}");
        }
    }

    #[test]
    fn charset_is_appended_only_to_lowercase_text_types() {
        // Starlette's exact rule, and it is case-sensitive on the
        // prefix: "TEXT/plain" goes out unchanged.
        assert_eq!(starlette_content_type("image/jpeg"), "image/jpeg");
        assert_eq!(starlette_content_type("video/mp2t"), "video/mp2t");
        assert_eq!(
            starlette_content_type("text/plain"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(starlette_content_type("TEXT/plain"), "TEXT/plain");
        assert_eq!(
            starlette_content_type("text/html; charset=iso-8859-1"),
            "text/html; charset=iso-8859-1"
        );
    }
}
