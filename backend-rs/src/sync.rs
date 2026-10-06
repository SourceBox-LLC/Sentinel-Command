//! The one-way data mirror to Sentinel-Sync-Service.
//!
//! Ported from `backend/app/core/sync_client.py`. Local Postgres (or
//! SQLite, self-hosted) stays the source of truth and the install works
//! with no internet at all; this pushes changed rows so a dead disk is
//! recoverable.
//!
//! Two decisions in the Python are load-bearing and easy to undo by
//! accident:
//!
//! **The payload is RAW COLUMN VALUES, not `to_dict()`.** It reused
//! `to_dict()` once, on the reasoning that anything its author had not
//! chosen to expose over the API could not leak into the cloud either.
//! That held for secrecy and silently made the mirror *unrestorable*:
//! Camera lost 11 of 21 columns (the whole recording policy nests under
//! `recording_policy`; both codecs vanish) and `SentinelRun` never
//! carried `tool_trace`. A backup you cannot restore from is not a
//! backup. So the columns go raw and the exclusions are stated.
//!
//! **Deletions propagate only for the small identity tables.** Cameras,
//! groups and nodes send a full `known_ids` snapshot so the service can
//! tombstone what is gone. The log and event tables deliberately do not:
//! local retention prunes them *because* local disk is finite, and the
//! cloud copy exists to outlive that. A local delete must never delete
//! the cloud row.

use crate::app::AppState;

/// `_PUSH_TIMEOUT_SECONDS`.
const PUSH_TIMEOUT_SECONDS: u64 = 30;

/// How many 429s one request waits out before it is reported as failed.
pub const MAX_RATE_LIMIT_WAITS: u32 = 5;

/// How long a 429 asks to be left alone, or `None` for any other answer.
///
/// Sync-Service limits each route to 120 requests a minute per address.
/// A first sync of a large table, or a restore walking one, can go
/// faster than that; treating the 429 as a failure aborted the restore
/// part-way and pushed a backlog back by a whole 30-minute cycle. The
/// wait is the service's `Retry-After`, 60 s when absent, and capped so
/// a malformed header cannot park the caller for hours.
pub fn rate_limit_wait(response: &reqwest::Response) -> Option<std::time::Duration> {
    if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let seconds = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(60);
    Some(std::time::Duration::from_secs(seconds.clamp(1, 120)))
}
/// `_BATCH_SIZE`.
const BATCH_SIZE: i64 = 500;

/// One syncable table: what to page by, and whether deletes propagate.
pub struct SyncTableSpec {
    pub table: &'static str,
    /// The column pushes are filtered and ordered by. Three different
    /// ones across the nine tables, and they are not interchangeable —
    /// `notifications` has no `updated_at` at all.
    pub cursor: &'static str,
    pub reconcile_deletes: bool,
    /// The primary key is text (a uuid hex), not an integer. The page
    /// key's tiebreak compares ids in their own type, as the ORDER BY
    /// does — compared as text, integer ids order 10 before 9.
    pub text_id: bool,
}

/// The nine tables, in Python's order. The order matters only for which
/// partial progress a failing cycle makes, but it is compared.
pub const SYNC_TABLES: [SyncTableSpec; 9] = [
    SyncTableSpec {
        table: "cameras",
        cursor: "updated_at",
        reconcile_deletes: true,
        text_id: false,
    },
    SyncTableSpec {
        table: "camera_groups",
        cursor: "updated_at",
        reconcile_deletes: true,
        text_id: false,
    },
    SyncTableSpec {
        table: "camera_nodes",
        cursor: "updated_at",
        reconcile_deletes: true,
        text_id: false,
    },
    SyncTableSpec {
        table: "incidents",
        cursor: "updated_at",
        reconcile_deletes: false,
        text_id: false,
    },
    SyncTableSpec {
        table: "incident_evidence",
        cursor: "timestamp",
        reconcile_deletes: false,
        text_id: false,
    },
    SyncTableSpec {
        table: "motion_events",
        cursor: "timestamp",
        reconcile_deletes: false,
        text_id: false,
    },
    SyncTableSpec {
        table: "sentinel_config",
        cursor: "updated_at",
        reconcile_deletes: false,
        text_id: false,
    },
    SyncTableSpec {
        table: "sentinel_runs",
        cursor: "updated_at",
        reconcile_deletes: false,
        text_id: true,
    },
    SyncTableSpec {
        table: "notifications",
        cursor: "created_at",
        reconcile_deletes: false,
        text_id: false,
    },
];

/// Columns that must never leave this install.
///
/// `api_key_hash` authenticates a node to THIS Command Center — useless
/// to a restore, since a restored node re-registers and is issued a
/// fresh key, and actively dangerous sitting in a cloud mirror.
///
/// `data` is snapshot and clip bytes, tens of megabytes a row. The
/// metadata columns around it still sync, so a restore knows the
/// evidence existed and what it was. The column is `deferred()` on the
/// Python model and the denylist is checked BEFORE the attribute is
/// read, which is what stops iterating columns from lazy-loading every
/// blob — reproduced here by never naming the column in the projection
/// rather than by selecting it and dropping it afterwards.
pub fn denied_columns(table: &str) -> &'static [&'static str] {
    match table {
        "camera_nodes" => &["api_key_hash"],
        "incident_evidence" => &["data"],
        _ => &[],
    }
}

/// `sentinel_sync_cursor_<table>`.
pub fn cursor_setting_key(table: &str) -> String {
    format!("sentinel_sync_cursor_{table}")
}

/// The id of the last row pushed at the cursor's timestamp — the second
/// half of the page key. Absent for a cursor written before it existed,
/// which then behaves as it always did.
pub fn cursor_id_setting_key(table: &str) -> String {
    format!("sentinel_sync_cursor_id_{table}")
}

/// What one push cycle did, per table.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncSummary {
    pub enabled: bool,
    /// `(table, rows_pushed, batches)`.
    pub pushed: Vec<(String, i64, i64)>,
    pub failed: Vec<String>,
}

impl SyncSummary {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled,
            "pushed": self.pushed.iter()
                .map(|(t, rows, batches)| serde_json::json!([t, rows, batches]))
                .collect::<Vec<_>>(),
            "failed": self.failed,
        })
    }
}

/// `push_pending_changes` — every syncable table's changes since its
/// own cursor.
///
/// **Never raises**, the same fail-open contract as the licence
/// check-in: a push failure means this cycle's data waits for the next
/// tick, and cursors advance only on a confirmed success. One table's
/// failure must not block the others — the cursors are independent, so
/// partial progress is both safe and useful.
pub async fn push_pending_changes(state: &AppState) -> SyncSummary {
    let ctx = crate::license::LicenseContext {
        pool: &state.pool,
        org_id: &state.config.local_org_id,
        local_auth: state.config.is_local_auth(),
        license_key: state.config.sentinel_license_key.as_deref(),
    };
    if !crate::license::is_sync_enabled(&ctx).await {
        return SyncSummary::default();
    }

    let mut summary = SyncSummary {
        enabled: true,
        ..Default::default()
    };
    for spec in &SYNC_TABLES {
        match push_table(state, spec).await {
            Ok((rows, batches)) => summary.pushed.push((spec.table.to_string(), rows, batches)),
            Err(error) => {
                tracing::warn!(table = spec.table, %error, "[SentinelSync] push failed");
                summary.failed.push(spec.table.to_string());
            }
        }
    }
    summary
}

/// One table, paged until a short batch.
async fn push_table(
    state: &AppState,
    spec: &SyncTableSpec,
) -> Result<(i64, i64), Box<dyn std::error::Error + Send + Sync>> {
    let org = &state.config.local_org_id;
    let cursor_key = cursor_setting_key(spec.table);
    let mut cursor: Option<chrono::NaiveDateTime> =
        crate::settings::get(&state.pool, org, &cursor_key, Some(""))
            .await
            .ok()
            .flatten()
            .filter(|raw| !raw.is_empty())
            .and_then(|raw| crate::pydatetime::fromisoformat(&raw).ok().map(|t| t.naive));
    let cursor_id_key = cursor_id_setting_key(spec.table);
    let mut cursor_id: Option<String> =
        crate::settings::get(&state.pool, org, &cursor_id_key, Some(""))
            .await
            .ok()
            .flatten()
            .filter(|raw| !raw.is_empty());
    // Rows younger than this are left for the next cycle. A row is
    // stamped when its statement runs and visible only when its
    // transaction commits; pushing right up to "now" let a push advance
    // past a row stamped earlier but committed later, and that row was
    // never pushed at all.
    let settled = crate::models::now_naive() - chrono::Duration::seconds(SETTLE_SECONDS);

    // The projection, built from the live column list minus the denied
    // ones. Naming the allowed columns rather than selecting the row and
    // stripping afterwards is what reproduces Python's "check before
    // read" — a `to_jsonb(t)` would load every deferred blob to build a
    // payload that then drops it.
    let columns = allowed_columns(state, spec.table).await?;
    if columns.is_empty() {
        return Ok((0, 0));
    }
    let projection = columns
        .iter()
        .map(|(c, kind)| format!("'{c}', {}", json_value_sql(c, kind)))
        .collect::<Vec<_>>()
        .join(", ");

    let mut total = 0;
    let mut batches = 0;
    loop {
        // Paged by the key (cursor, id), in that order, and filtered past
        // the last key pushed. Filtering on the timestamp alone lost rows:
        // a full batch ending at T left any further rows stamped T behind
        // a `> T` that could never reach them — and bulk updates (the
        // offline sweep, a plan's camera cap) stamp many rows with one T.
        // A row whose cursor column is NULL sorts and compares as the
        // epoch: it is pushed on the first pass and again when it is next
        // updated, like any other row. See the note on PYTHON_BUGS #16
        // below — it used to stop the whole table.
        let effective = format!("COALESCE(t.\"{}\", {EPOCH_SQL})", spec.cursor);
        let sql = format!(
            "SELECT CAST(t.id AS TEXT),
                    {cursor_iso} AS cursor_iso,
                    {effective} AS cursor_raw,
                    {JSON_OBJECT}({projection}) AS data
               FROM {table} t
              WHERE (CAST($1 AS TIMESTAMP) IS NULL
                     OR {key} > $1
                     OR ({key} = $1 AND t.id > {id_param}))
                AND {key} < $3
              ORDER BY {key} ASC, t.id ASC
              LIMIT {BATCH_SIZE}",
            table = spec.table,
            cursor_iso = cursor_iso_sql(&effective),
            key = cursor_cmp_sql(&effective),
            id_param = if spec.text_id { "$2" } else { ID_PARAM_INT },
        );
        let rows: Vec<(
            String,
            Option<String>,
            Option<chrono::NaiveDateTime>,
            serde_json::Value,
        )> = sqlx::query_as(&sql)
            .bind(cursor_param(cursor))
            .bind(cursor_id.clone())
            .bind(cursor_param(Some(settled)))
            .fetch_all(&state.pool)
            .await?;
        if rows.is_empty() {
            break;
        }

        // PYTHON_BUGS #16, closed. The Python built the envelope with
        // `getattr(row, cursor).isoformat()`, so one row with a NULL
        // cursor raised, the per-table `except` swallowed it, and that
        // table sent nothing — ever: cameras, camera_groups, camera_nodes
        // and sentinel_runs, the four that held such rows, never reached
        // the mirror. The port reproduced it while the two stacks had to
        // agree. The COALESCE above means no row reaches here without a
        // cursor; this stays as the assertion that it holds.
        if rows.iter().any(|(_, iso, _, _)| iso.is_none()) {
            return Err(format!(
                "row with a NULL {} cannot be serialised for the sync envelope",
                spec.cursor
            )
            .into());
        }

        let mut payload = serde_json::json!({
            "table": spec.table,
            "rows": rows.iter().map(|(id, iso, _, data)| serde_json::json!({
                "id": id,
                "updated_at": iso,
                "data": data,
            })).collect::<Vec<_>>(),
        });
        if spec.reconcile_deletes {
            // The FULL current id set, not just this batch — cheap for
            // these small identity tables, and it is what lets the
            // service tombstone a row that is no longer here at all.
            let ids: Vec<(String,)> =
                sqlx::query_as(&format!("SELECT CAST(id AS TEXT) FROM {}", spec.table))
                    .fetch_all(&state.pool)
                    .await?;
            payload["known_ids"] =
                serde_json::json!(ids.into_iter().map(|(id,)| id).collect::<Vec<_>>());
        }

        let mut waits = 0;
        let response = loop {
            let response = state
                .http
                .post(format!(
                    "{}/v1/sync/push",
                    state.config.sentinel_sync_service_url.trim_end_matches('/')
                ))
                .bearer_auth(
                    state
                        .config
                        .sentinel_license_key
                        .clone()
                        .unwrap_or_default(),
                )
                .timeout(std::time::Duration::from_secs(PUSH_TIMEOUT_SECONDS))
                .json(&payload)
                .send()
                .await?;
            match rate_limit_wait(&response) {
                Some(wait) if waits < MAX_RATE_LIMIT_WAITS => {
                    waits += 1;
                    tracing::info!(table = spec.table, ?wait, "[Sync] rate limited; waiting");
                    tokio::time::sleep(wait).await;
                }
                _ => break response,
            }
        };
        // `raise_for_status()`: the cursor advances only past rows the
        // service CONFIRMED. A cursor moved on an unacknowledged push is
        // data silently missing from the mirror.
        response.error_for_status_ref()?;

        let count = rows.len() as i64;
        total += count;
        batches += 1;

        // Unreachable now that the batch is rejected above, and kept
        // as a guard rather than an `unwrap`: falling through with a
        // None cursor would leave `$1 IS NULL` matching every row on
        // the next iteration, and a table over one batch would spin
        // forever re-fetching the same five hundred.
        let Some(last_iso) = rows.last().and_then(|(_, iso, _, _)| iso.clone()) else {
            break;
        };
        cursor = rows.last().and_then(|(_, _, raw, _)| *raw);
        cursor_id = rows.last().map(|(id, _, _, _)| id.clone());
        crate::settings::set(&state.pool, org, &cursor_key, &last_iso)
            .await
            .ok();
        if let Some(id) = &cursor_id {
            crate::settings::set(&state.pool, org, &cursor_id_key, id)
                .await
                .ok();
        }

        if count < BATCH_SIZE {
            break;
        }
    }
    Ok((total, batches))
}

// ── The two dialects ─────────────────────────────────────────────────
//
// Everything about this query is shared except how a row becomes JSON,
// and that is where the two databases differ most: Postgres knows a
// column is a timestamp or a boolean and `json_build_object` renders it
// as one; SQLite stores both as text and integer and would hand them
// over as `"2026-09-15 10:00:00"` and `1`. The mirror has to hold the
// same thing whichever database pushed it — `sentinel-restore-from-cloud`
// reads it back into either — so the SQLite side restores the types by
// hand, from the declared column types.

#[cfg(not(feature = "sqlite"))]
const JSON_OBJECT: &str = "json_build_object";
#[cfg(feature = "sqlite")]
const JSON_OBJECT: &str = "json_object";

/// The cursor as `YYYY-MM-DDTHH:MM:SS.ffffff`, always six digits.
#[cfg(not(feature = "sqlite"))]
fn cursor_iso_sql(cursor: &str) -> String {
    format!("to_char({cursor}, 'YYYY-MM-DD\"T\"HH24:MI:SS.US')")
}

/// The epoch, as each engine stores a timestamp. Not `CAST(… AS
/// TIMESTAMP)`: SQLite gives that type name numeric affinity, so the cast
/// would turn the text into the number 1970.
#[cfg(not(feature = "sqlite"))]
const EPOCH_SQL: &str = "TIMESTAMP '1970-01-01 00:00:00'";
#[cfg(feature = "sqlite")]
const EPOCH_SQL: &str = "'1970-01-01 00:00:00'";

/// The same string, from stored text. sqlx writes `%F %T%.f` — no
/// fraction at all for a whole second, else three, six or nine digits —
/// so it is padded out and cut back to the 26 characters Postgres gives.
#[cfg(feature = "sqlite")]
fn cursor_iso_sql(cursor: &str) -> String {
    let c = cursor;
    format!(
        "CASE WHEN {c} IS NULL THEN NULL ELSE substr(replace({c}, ' ', 'T') || \
         CASE WHEN instr({c}, '.') = 0 THEN '.000000' ELSE '000000' END, 1, 26) END"
    )
}

/// How long a row must have existed before it is pushed. See `push_table`.
const SETTLE_SECONDS: i64 = 10;

/// The page key's timestamp half, as compared and ordered. PostgreSQL
/// compares the timestamp itself.
#[cfg(not(feature = "sqlite"))]
fn cursor_cmp_sql(effective: &str) -> String {
    effective.to_string()
}

/// SQLite compares text, and one instant can be stored as `…:07.12`,
/// `…:07.120000` or `…:07.120000000` — equal instants, unequal text, so
/// the tiebreak's `=` would miss them. Compared (and ordered) in one
/// normalised spelling instead: the 26 characters `cursor_iso_sql`
/// produces, against a parameter bound the same way.
#[cfg(feature = "sqlite")]
fn cursor_cmp_sql(effective: &str) -> String {
    cursor_iso_sql(effective)
}

/// The id tiebreak's parameter, cast to the integer the column holds.
#[cfg(not(feature = "sqlite"))]
const ID_PARAM_INT: &str = "CAST($2 AS BIGINT)";
#[cfg(feature = "sqlite")]
const ID_PARAM_INT: &str = "CAST($2 AS INTEGER)";

/// The cursor as bound for `t."cursor" > $1`.
#[cfg(not(feature = "sqlite"))]
fn cursor_param(cursor: Option<chrono::NaiveDateTime>) -> Option<chrono::NaiveDateTime> {
    cursor
}

/// SQLite compares the stored TEXT, so the bound value has to be text
/// that orders correctly against every spelling a row can have. sqlx
/// would write `%F %T%.f` — no fraction for a whole second, else 3, 6 or
/// 9 digits — while a row the Python tier wrote holds `%.6f` always. A
/// legacy `…:07.120000` is then strictly greater than its own cursor
/// bound as `…:07.120`, and is pushed again on every cycle. Six digits,
/// always, compares as "not greater" against both spellings of the
/// same instant and correctly against every other.
#[cfg(feature = "sqlite")]
fn cursor_param(cursor: Option<chrono::NaiveDateTime>) -> Option<String> {
    // The normalised spelling `cursor_cmp_sql` compares against.
    cursor.map(|at| at.format("%FT%T%.6f").to_string())
}

/// One column as a JSON value.
#[cfg(not(feature = "sqlite"))]
fn json_value_sql(column: &str, _declared_type: &str) -> String {
    format!("t.\"{column}\"")
}

#[cfg(feature = "sqlite")]
fn json_value_sql(column: &str, declared_type: &str) -> String {
    let c = format!("t.\"{column}\"");
    match declared_type.to_ascii_uppercase().as_str() {
        // ISO 8601 with a `T`, as Postgres renders a timestamp in JSON —
        // including its fraction, which Postgres writes with trailing
        // zeros trimmed (`.89395`, not `.893950`) and omits when it is
        // zero. The stored text may carry 3, 6 or 9 digits, so it is
        // trimmed the same way. Only inside a fraction: a bare rtrim
        // would eat the zero from `:40` too.
        "DATETIME" | "TIMESTAMP" => format!(
            "replace(CASE WHEN instr({c}, '.') > 0 \
             THEN rtrim(rtrim({c}, '0'), '.') ELSE {c} END, ' ', 'T')"
        ),
        // `json('true')` is a JSON boolean; a bare 1 would be a number.
        "BOOLEAN" => {
            format!("json(CASE {c} WHEN 1 THEN 'true' WHEN 0 THEN 'false' ELSE 'null' END)")
        }
        _ => c,
    }
}

/// The table's columns with their declared types, in ordinal order,
/// minus the denied ones.
async fn allowed_columns(
    state: &AppState,
    table: &str,
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(not(feature = "sqlite"))]
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, data_type FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = $1
          ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(&state.pool)
    .await?;
    #[cfg(feature = "sqlite")]
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT name, type FROM pragma_table_info($1) ORDER BY cid")
            .bind(table)
            .fetch_all(&state.pool)
            .await?;
    let denied = denied_columns(table);
    Ok(rows
        .into_iter()
        .filter(|(name, _)| !denied.contains(&name.as_str()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three tables whose deletions propagate, and only those.
    ///
    /// Widening this is the dangerous direction: a log table that
    /// reconciled deletes would have the cloud copy pruned every time
    /// local retention ran, which is the exact opposite of why the
    /// mirror exists.
    #[test]
    fn only_the_identity_tables_reconcile_deletes() {
        let reconciled: Vec<&str> = SYNC_TABLES
            .iter()
            .filter(|s| s.reconcile_deletes)
            .map(|s| s.table)
            .collect();
        assert_eq!(reconciled, ["cameras", "camera_groups", "camera_nodes"]);
    }

    /// Three different cursor columns across nine tables, and a table
    /// paged by a column it does not have would fail every cycle.
    #[test]
    fn each_table_pages_by_a_column_that_suits_it() {
        for spec in &SYNC_TABLES {
            assert!(
                matches!(spec.cursor, "updated_at" | "timestamp" | "created_at"),
                "{}: {}",
                spec.table,
                spec.cursor
            );
        }
        let notifications = SYNC_TABLES
            .iter()
            .find(|s| s.table == "notifications")
            .unwrap();
        // The one that is neither: `notifications` has no `updated_at`.
        assert_eq!(notifications.cursor, "created_at");
    }

    #[test]
    fn the_denylist_covers_the_credential_and_the_blob() {
        assert_eq!(denied_columns("camera_nodes"), ["api_key_hash"]);
        assert_eq!(denied_columns("incident_evidence"), ["data"]);
        assert!(denied_columns("cameras").is_empty());
    }

    #[test]
    fn a_cursor_key_names_its_table() {
        assert_eq!(
            cursor_setting_key("cameras"),
            "sentinel_sync_cursor_cameras"
        );
    }
}
