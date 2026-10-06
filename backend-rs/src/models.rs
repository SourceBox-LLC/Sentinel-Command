//! Row types and their wire shapes.
//!
//! Each `to_json` here reproduces a SQLAlchemy model's `to_dict()` from
//! `backend/app/models/models.py` exactly. The SPA and the MCP tools read
//! these keys, so a renamed or reshaped field is a client-visible break.

use chrono::{NaiveDateTime, Utc};

use crate::error::ApiError;
use serde_json::{json, Value};

/// Timestamps are stored and served naive (no offset) throughout, because
/// the Python service writes `datetime.now(tz=UTC).replace(tzinfo=None)`.
///
/// Truncated to MICROSECONDS, which is all a Python `datetime` holds and
/// all a Postgres `timestamp` stores — sqlx's Postgres encoder divides
/// the nanoseconds away on the way in, so there this changes nothing.
///
/// On SQLite it is the difference between a cursor that works and one
/// that does not. A timestamp is stored there as text, at whatever
/// precision it was formatted with, and chrono's clock reads
/// nanoseconds: the row would hold `…:07.123456789` while every reader
/// renders, and every cursor round-trips, `…:07.123456`. A row is then
/// always strictly greater than its own cursor, and the sync loop pushes
/// its last row again on every cycle, forever.
pub fn now_naive() -> NaiveDateTime {
    to_micros(Utc::now().naive_utc())
}

/// Drop sub-microsecond precision. See [`now_naive`].
pub fn to_micros(at: NaiveDateTime) -> NaiveDateTime {
    use chrono::Timelike;
    at.with_nanosecond(at.nanosecond() / 1_000 * 1_000)
        .unwrap_or(at)
}

/// `datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(<unit>=n)`,
/// with Python's limits rather than chrono's.
///
/// Several routes take a `hours` or `days` window that Python caps on
/// one side only, so a large negative value asks for a window reaching
/// into the future — and past the year 9999 that a `datetime` stops at,
/// where Python raised OverflowError and the request became a 500 (a 422
/// here).
/// `timedelta` gives out earlier still, at a magnitude of 10^9 days, and
/// an integer too large for i64 never reaches it at all. chrono would
/// answer all three happily (it reaches year 262143), and
/// `Duration::days` panics somewhere further out again — so the range
/// has to be applied deliberately, or the two stacks disagree exactly
/// where the Python breaks.
pub fn python_window_start(
    n: crate::pyint::PyInt,
    unit_seconds: i64,
) -> Result<NaiveDateTime, ApiError> {
    // The Python raised OverflowError here and answered 500. It is a
    // window the caller asked for, so it is the caller's 422.
    let overflow = || {
        ApiError::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "time window is out of range",
        )
    };
    let n = n.small().ok_or_else(overflow)?;

    let shift = i128::from(n) * i128::from(unit_seconds) * 1_000_000;
    let start = i128::from(now_naive().and_utc().timestamp_micros()) - shift;

    // datetime.min .. datetime.max, in microseconds from the epoch.
    const MIN: i128 = -62_135_596_800_000_000;
    const MAX: i128 = 253_402_300_799_999_999;
    if !(MIN..=MAX).contains(&start) {
        return Err(overflow());
    }
    chrono::DateTime::from_timestamp_micros(start as i64)
        .map(|dt| dt.naive_utc())
        .ok_or_else(overflow)
}

/// Format a timestamp the way Python's `datetime.isoformat()` does.
///
/// Not `%Y-%m-%dT%H:%M:%S%.f`: chrono's `%.f` drops trailing zeros, so
/// 100000 microseconds renders as `.100` where Python writes `.100000`.
/// Python omits the fraction entirely when it is zero and otherwise
/// writes exactly six digits.
pub fn iso_naive(ts: NaiveDateTime) -> String {
    use chrono::Timelike;
    let micros = ts.nanosecond() / 1_000;
    if micros == 0 {
        ts.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        format!("{}.{:06}", ts.format("%Y-%m-%dT%H:%M:%S"), micros)
    }
}

/// A camera joined to its node and group.
///
/// The join replaces SQLAlchemy's `selectinload`, which the Python needs
/// to avoid an N+1 on a route the dashboard polls every 5 seconds per
/// open tab.
#[derive(Debug, sqlx::FromRow)]
pub struct CameraRow {
    pub camera_id: String,
    pub name: String,
    /// The *node's* string id, not the integer FK — the frontend joins
    /// cameras to nodes on this without a second round trip.
    pub node_id: Option<String>,
    pub node_name: Option<String>,
    pub node_type: Option<String>,
    pub capabilities: Option<String>,
    pub group_id: Option<i32>,
    pub group_name: Option<String>,
    pub status: Option<String>,
    pub last_error: Option<String>,
    pub last_seen: Option<NaiveDateTime>,
    pub disabled_by_plan: bool,
    pub continuous_24_7: bool,
    pub scheduled_recording: bool,
    pub scheduled_start: Option<String>,
    pub scheduled_end: Option<String>,
}

/// The columns and joins every camera response needs, in one place so the
/// list and single-camera routes cannot drift apart.
pub const CAMERA_SELECT: &str = r#"
    SELECT c.camera_id, c.name,
           n.node_id AS node_id, n.name AS node_name,
           c.node_type, c.capabilities,
           c.group_id, g.name AS group_name,
           c.status, c.last_error, c.last_seen,
           c.disabled_by_plan, c.continuous_24_7, c.scheduled_recording,
           c.scheduled_start, c.scheduled_end
      FROM cameras c
      LEFT JOIN camera_nodes n ON n.id = c.node_id
      LEFT JOIN camera_groups g ON g.id = c.group_id
"#;

/// A camera is offline after three missed heartbeats.
const HEARTBEAT_GRACE_SECONDS: i64 = 90;

/// `Camera.effective_status`, as a function of the two columns it reads,
/// so every route that reports a camera's status computes it one way.
pub fn camera_effective_status(
    status: Option<&str>,
    last_seen: Option<NaiveDateTime>,
) -> Option<String> {
    let Some(last_seen) = last_seen else {
        return Some("offline".to_string());
    };
    if status == Some("offline") {
        return Some("offline".to_string());
    }
    let age = now_naive().signed_duration_since(last_seen);
    if age.num_seconds() > HEARTBEAT_GRACE_SECONDS {
        return Some("offline".to_string());
    }
    status.map(str::to_string)
}

impl CameraRow {
    /// Real-time status, derived from `last_seen` rather than trusted
    /// from the stored column — a node that dies without saying so
    /// leaves `status` reading "streaming" forever.
    ///
    /// Returns `None` only when the stored status is NULL and the camera
    /// is otherwise live, which is what Python's attribute access yields
    /// for such a row.
    pub fn effective_status(&self) -> Option<String> {
        camera_effective_status(self.status.as_deref(), self.last_seen)
    }

    pub fn to_json(&self) -> Value {
        let eff = self.effective_status();
        // Only surface last_error while the camera is actually broken.
        // Once it flips back to streaming the stale reason would just
        // confuse anyone reading the response.
        let err = match eff.as_deref() {
            Some("restarting") | Some("failed") | Some("error") => self.last_error.clone(),
            _ => None,
        };
        json!({
            "camera_id": self.camera_id,
            "name": self.name,
            "node_id": self.node_id,
            "node_name": self.node_name,
            "node_type": self.node_type,
            // Python splits on "," with no trimming, and an empty or
            // NULL column yields [] rather than [""].
            "capabilities": match self.capabilities.as_deref() {
                None | Some("") => Vec::new(),
                Some(c) => c.split(',').map(str::to_string).collect::<Vec<_>>(),
            },
            // `group_id` is the FK the dashboard filters on; `group` is
            // the human-readable name the MCP tools put in prose. Both
            // are part of the contract.
            "group_id": self.group_id,
            "group": self.group_name,
            "status": eff,
            "last_error": err,
            "last_seen": self.last_seen.map(iso_naive),
            "disabled_by_plan": self.disabled_by_plan,
            "recording_policy": {
                "continuous_24_7": self.continuous_24_7,
                "scheduled_recording": self.scheduled_recording,
                "scheduled_start": self.scheduled_start,
                "scheduled_end": self.scheduled_end,
            },
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
pub struct CameraGroupRow {
    pub id: i32,
    pub name: String,
    pub color: Option<String>,
    pub icon: Option<String>,
    /// Counted in SQL rather than by loading the group's cameras, which
    /// is what `len(self.cameras)` does in Python.
    pub camera_count: i64,
}

impl CameraGroupRow {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "color": self.color,
            "icon": self.icon,
            "camera_count": self.camera_count,
        })
    }
}

#[cfg(test)]
mod tests {
    /// The clock the service writes with holds nothing a Postgres
    /// `timestamp` would drop — see `now_naive`.
    #[test]
    fn the_service_clock_has_microsecond_precision() {
        use chrono::Timelike;
        for _ in 0..50 {
            assert_eq!(super::now_naive().nanosecond() % 1_000, 0);
        }
        let precise = chrono::NaiveDate::from_ymd_opt(2026, 9, 15)
            .unwrap()
            .and_hms_nano_opt(10, 0, 7, 123_456_789)
            .unwrap();
        assert_eq!(super::to_micros(precise).nanosecond(), 123_456_000);
        // And what sqlx writes to SQLite for it is what Postgres holds.
        assert_eq!(
            super::to_micros(precise).format("%F %T%.f").to_string(),
            "2026-09-15 10:00:07.123456"
        );
    }

    use super::*;
    use chrono::{Duration, NaiveDate};

    fn camera(status: Option<&str>, last_seen: Option<NaiveDateTime>) -> CameraRow {
        CameraRow {
            camera_id: "cam1".into(),
            name: "Front".into(),
            node_id: None,
            node_name: None,
            node_type: None,
            capabilities: None,
            group_id: None,
            group_name: None,
            status: status.map(str::to_string),
            last_error: Some("ffmpeg died".into()),
            last_seen,
            disabled_by_plan: false,
            continuous_24_7: false,
            scheduled_recording: false,
            scheduled_start: None,
            scheduled_end: None,
        }
    }

    #[test]
    fn a_camera_that_has_never_been_seen_is_offline() {
        assert_eq!(
            camera(Some("streaming"), None)
                .effective_status()
                .as_deref(),
            Some("offline")
        );
    }

    #[test]
    fn a_recent_heartbeat_keeps_the_stored_status() {
        let seen = now_naive() - Duration::seconds(10);
        assert_eq!(
            camera(Some("streaming"), Some(seen))
                .effective_status()
                .as_deref(),
            Some("streaming")
        );
    }

    #[test]
    fn three_missed_heartbeats_mean_offline() {
        // A node that dies without saying so leaves `status` reading
        // "streaming" forever; this is what catches it.
        let seen = now_naive() - Duration::seconds(91);
        assert_eq!(
            camera(Some("streaming"), Some(seen))
                .effective_status()
                .as_deref(),
            Some("offline")
        );
    }

    #[test]
    fn the_grace_boundary_is_exclusive() {
        // Python compares `age > timedelta(seconds=90)`, so exactly 90
        // is still live.
        let seen = now_naive() - Duration::seconds(90);
        assert_eq!(
            camera(Some("streaming"), Some(seen))
                .effective_status()
                .as_deref(),
            Some("streaming")
        );
    }

    #[test]
    fn last_error_is_hidden_unless_the_camera_is_broken() {
        let seen = now_naive() - Duration::seconds(5);
        for (status, expected) in [
            ("streaming", None),
            ("starting", None),
            ("restarting", Some("ffmpeg died")),
            ("failed", Some("ffmpeg died")),
            ("error", Some("ffmpeg died")),
        ] {
            let json = camera(Some(status), Some(seen)).to_json();
            assert_eq!(json["last_error"].as_str(), expected, "status {status}");
        }
    }

    #[test]
    fn a_stale_error_is_dropped_when_the_camera_goes_offline() {
        // eff becomes "offline", which is not a broken state, so the
        // reason is suppressed even though the column still holds it.
        let seen = now_naive() - Duration::seconds(500);
        let json = camera(Some("failed"), Some(seen)).to_json();
        assert_eq!(json["status"], "offline");
        assert!(json["last_error"].is_null());
    }

    #[test]
    fn capabilities_split_on_commas_without_trimming() {
        let mut c = camera(Some("streaming"), None);
        c.capabilities = Some("streaming, motion".into());
        assert_eq!(
            c.to_json()["capabilities"],
            json!(["streaming", " motion"]),
            "Python does not trim, and neither may we"
        );
    }

    #[test]
    fn an_empty_capability_column_is_an_empty_list_not_a_blank_entry() {
        for value in [None, Some("")] {
            let mut c = camera(Some("streaming"), None);
            c.capabilities = value.map(str::to_string);
            assert_eq!(c.to_json()["capabilities"], json!([]));
        }
    }

    #[test]
    fn timestamps_match_python_isoformat() {
        // Whole seconds carry no fraction at all...
        let whole = NaiveDate::from_ymd_opt(2026, 9, 15)
            .unwrap()
            .and_hms_opt(12, 30, 5)
            .unwrap();
        assert_eq!(iso_naive(whole), "2026-09-15T12:30:05");

        // ...and a fraction is always six digits, never truncated.
        // chrono's %.f would render this one as ".100".
        let tenth = NaiveDate::from_ymd_opt(2026, 9, 15)
            .unwrap()
            .and_hms_micro_opt(12, 30, 5, 100_000)
            .unwrap();
        assert_eq!(iso_naive(tenth), "2026-09-15T12:30:05.100000");

        let micro = NaiveDate::from_ymd_opt(2026, 9, 15)
            .unwrap()
            .and_hms_micro_opt(12, 30, 5, 123_456)
            .unwrap();
        assert_eq!(iso_naive(micro), "2026-09-15T12:30:05.123456");
    }
}
