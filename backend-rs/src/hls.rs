//! The in-memory caches the live video path runs on.
//!
//! Ported from the module-level state in `backend/app/api/hls.py`. The
//! routes that use it are in `api/hls.rs`; this is the state itself,
//! because more than the routes touch it — deleting a camera or a node
//! clears it, the MCP `attach_clip` tool reads from it, and three
//! background loops sweep it.
//!
//! **One process may own this.** A cache is only correct if exactly one
//! process has it, and the moment Rust serves `push-segment` the
//! Python's copy stops being the one with the segments in it. Every
//! other call site has to move with it. `tests/differential/
//! in_process_state.md` lists them and what is left.
//!
//! Three things here are not obvious and all three are load-bearing.
//!
//! **The segment cache is keyed by `camera_id` alone**, not by
//! `(org_id, camera_id)`, which is safe only because `cameras.camera_id`
//! is globally unique in the schema — and every read path does its own
//! org-scoped lookup before touching the cache anyway.
//!
//! **Filenames sort as strings.** Python evicts with
//! `sorted(cam_cache.keys())`, so `segment_00010.ts` follows
//! `segment_00009.ts` because of the zero padding, not because anything
//! parses the number. A `BTreeMap` keyed by the same strings gives the
//! same order.
//!
//! **The byte total is maintained incrementally**, and the stale sweep
//! recomputes it from scratch as drift insurance — both deliberate in
//! the Python, for a hot path that runs up to 1200 times a minute per
//! camera.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::Utc;

/// `_PLAYLIST_CACHE_MAX_AGE` — comfortably longer than the gap between
/// a node's playlist pushes, so one or two dropped pushes do not show
/// the viewer "Stream not started yet" while segments are still
/// arriving.
pub const PLAYLIST_CACHE_MAX_AGE: Duration = Duration::from_secs(30);

/// `_CACHE_MAX_CAMERAS`.
const CACHE_MAX_CAMERAS: usize = 500;

/// `_ACCESS_LOG_INTERVAL` — one stream-access row per user and camera
/// per five minutes, however often the player polls.
pub const ACCESS_LOG_INTERVAL: Duration = Duration::from_secs(300);

/// `_ACCESS_LOG_MAX_ENTRIES`.
const ACCESS_LOG_MAX_ENTRIES: usize = 10_000;

/// How long a camera can go without a segment before its bucket is
/// dropped entirely.
const STALE_CAMERA_AGE: Duration = Duration::from_secs(60);

#[derive(Default)]
struct SegmentStore {
    /// `{camera_id: {filename: (bytes, monotonic)}}`.
    cameras: HashMap<String, BTreeMap<String, (Bytes, Instant)>>,
    /// `_segment_cache_byte_total`, kept in step at every insert and
    /// delete so the global cap check is O(1) on the push path.
    byte_total: i64,
}

#[derive(Default)]
struct ViewerUsage {
    /// `(org_id, "YYYY-MM")` → increments not yet written.
    pending: HashMap<(String, String), i64>,
    /// `(org_id, "YYYY-MM")` → the database total as last read.
    cached: HashMap<(String, String), i64>,
}

/// Everything `hls.py` holds at module level.
#[derive(Default)]
pub struct HlsCache {
    segments: Mutex<SegmentStore>,
    playlists: Mutex<HashMap<String, (String, Instant)>>,
    playlist_updates: Mutex<HashMap<String, u64>>,
    first_playlist_logged: Mutex<HashSet<String>>,
    first_stream_get_logged: Mutex<HashSet<String>>,
    access_logged: Mutex<HashMap<(String, String), Instant>>,
    viewer: Mutex<ViewerUsage>,
}

/// What a snapshot of the newest segments found.
#[derive(Debug, PartialEq, Eq)]
pub enum Snapshot {
    /// No bucket at all — the stream never went live, or it was evicted.
    NoCamera,
    /// A bucket that yielded these (possibly none, if a sweep emptied
    /// it between the two steps).
    Segments(Vec<Bytes>),
}

impl HlsCache {
    pub fn new() -> Self {
        Self::default()
    }

    // ── segments ─────────────────────────────────────────────────────

    /// Cache one pushed segment and run both eviction passes, as the
    /// push path does — under one lock, so nothing observes the cache
    /// between the insert and the trim.
    ///
    /// Returns the number of segments cached for the camera afterwards,
    /// which is what the response reports.
    pub fn push_segment(
        &self,
        camera_id: &str,
        filename: &str,
        body: Bytes,
        max_per_camera: usize,
        max_total_bytes: i64,
    ) -> usize {
        let mut store = self.segments.lock().expect("segment cache poisoned");
        let bucket = store.cameras.entry(camera_id.to_string()).or_default();
        // A re-push of the same filename overwrites, so the old size
        // comes off the total before the new one goes on. A flaky
        // network retry does exactly this.
        let previous = bucket.insert(filename.to_string(), (body.clone(), Instant::now()));
        let mut delta = body.len() as i64;
        if let Some((old, _)) = previous {
            delta -= old.len() as i64;
        }
        store.byte_total += delta;

        evict_per_camera(&mut store, camera_id, max_per_camera);
        evict_global_oldest(&mut store, max_total_bytes);
        store.cameras.get(camera_id).map_or(0, BTreeMap::len)
    }

    /// One segment's bytes, or `None`.
    pub fn segment(&self, camera_id: &str, filename: &str) -> Option<Bytes> {
        let store = self.segments.lock().expect("segment cache poisoned");
        store
            .cameras
            .get(camera_id)
            .and_then(|bucket| bucket.get(filename))
            .map(|(body, _)| body.clone())
    }

    /// How many segments a camera has cached — for the one-shot
    /// diagnostic lines, which report it.
    pub fn segment_count(&self, camera_id: &str) -> usize {
        let store = self.segments.lock().expect("segment cache poisoned");
        store.cameras.get(camera_id).map_or(0, BTreeMap::len)
    }

    /// `_segment_cache_total_bytes`.
    pub fn total_bytes(&self) -> i64 {
        self.segments.lock().expect("segment cache poisoned").byte_total
    }

    /// `snapshot_recent_segment_bytes`: the newest `count` segments,
    /// oldest first, taken under the lock so an eviction cannot run
    /// between choosing the filenames and reading them.
    pub fn snapshot_recent(&self, camera_id: &str, count: usize) -> Snapshot {
        let store = self.segments.lock().expect("segment cache poisoned");
        let Some(bucket) = store.cameras.get(camera_id).filter(|b| !b.is_empty()) else {
            return Snapshot::NoCamera;
        };
        if count == 0 {
            return Snapshot::Segments(Vec::new());
        }
        let skip = bucket.len().saturating_sub(count);
        Snapshot::Segments(bucket.values().skip(skip).map(|(body, _)| body.clone()).collect())
    }

    /// `cleanup_camera_cache`: everything held for one camera, across
    /// every cache. Called when a camera or its node goes away.
    pub fn cleanup_camera(&self, camera_id: &str) {
        {
            let mut store = self.segments.lock().expect("segment cache poisoned");
            if let Some(bucket) = store.cameras.remove(camera_id) {
                let freed: i64 = bucket.values().map(|(body, _)| body.len() as i64).sum();
                store.byte_total -= freed;
            }
        }
        self.playlists.lock().expect("playlist cache poisoned").remove(camera_id);
        self.playlist_updates.lock().expect("playlist counts poisoned").remove(camera_id);
        self.first_playlist_logged.lock().expect("log set poisoned").remove(camera_id);
        self.first_stream_get_logged.lock().expect("log set poisoned").remove(camera_id);
    }

    /// `_evict_stale_cameras`, including the counter reconciliation the
    /// Python does on the same sweep.
    pub fn evict_stale_cameras(&self) {
        let stale: Vec<String> = {
            let mut store = self.segments.lock().expect("segment cache poisoned");
            let cutoff = Instant::now() - STALE_CAMERA_AGE;
            let stale: Vec<String> = store
                .cameras
                .iter()
                .filter(|(_, bucket)| {
                    bucket.is_empty() || bucket.values().map(|(_, ts)| *ts).max().is_some_and(|newest| newest < cutoff)
                })
                .map(|(camera_id, _)| camera_id.clone())
                .collect();
            for camera_id in &stale {
                store.cameras.remove(camera_id);
            }
            // Drift insurance: if any mutation site ever forgets to
            // adjust the running total, this heals it once a minute.
            store.byte_total = recompute_bytes(&store);
            stale
        };
        for camera_id in &stale {
            self.playlists.lock().expect("playlist cache poisoned").remove(camera_id);
            self.playlist_updates.lock().expect("playlist counts poisoned").remove(camera_id);
            self.first_playlist_logged.lock().expect("log set poisoned").remove(camera_id);
            self.first_stream_get_logged.lock().expect("log set poisoned").remove(camera_id);
        }
    }

    /// `_evict_caches`: the sweep the playlist push runs every
    /// `CLEANUP_INTERVAL` pushes, and a background loop runs on a timer.
    pub fn evict_caches(&self) {
        {
            let mut playlists = self.playlists.lock().expect("playlist cache poisoned");
            if playlists.len() > CACHE_MAX_CAMERAS {
                let mut by_age: Vec<(String, Instant)> =
                    playlists.iter().map(|(k, (_, ts))| (k.clone(), *ts)).collect();
                by_age.sort_by_key(|(_, ts)| *ts);
                let drop_count = by_age.len() - CACHE_MAX_CAMERAS;
                let mut counts = self.playlist_updates.lock().expect("playlist counts poisoned");
                for (camera_id, _) in by_age.into_iter().take(drop_count) {
                    playlists.remove(&camera_id);
                    counts.remove(&camera_id);
                }
            }
        }
        {
            let mut logged = self.access_logged.lock().expect("access log poisoned");
            if logged.len() > ACCESS_LOG_MAX_ENTRIES {
                let cutoff = Instant::now() - ACCESS_LOG_INTERVAL * 2;
                logged.retain(|_, ts| *ts >= cutoff);
            }
        }
        self.evict_stale_cameras();
    }

    // ── playlists ────────────────────────────────────────────────────

    /// Cache the rewritten playlist a node pushed.
    pub fn set_playlist(&self, camera_id: &str, playlist: String) {
        self.playlists
            .lock()
            .expect("playlist cache poisoned")
            .insert(camera_id.to_string(), (playlist, Instant::now()));
    }

    /// The cached playlist and its age, whether or not it is still
    /// fresh — the caller decides, because the miss path logs whether
    /// there was one at all.
    pub fn playlist(&self, camera_id: &str) -> Option<(String, Duration)> {
        let playlists = self.playlists.lock().expect("playlist cache poisoned");
        playlists
            .get(camera_id)
            .map(|(text, ts)| (text.clone(), ts.elapsed()))
    }

    /// Count this playlist push and say whether the sweep is due.
    pub fn bump_playlist_count(&self, camera_id: &str, interval: u64) -> bool {
        let mut counts = self.playlist_updates.lock().expect("playlist counts poisoned");
        let count = counts.entry(camera_id.to_string()).or_insert(0);
        *count += 1;
        interval != 0 && (*count).is_multiple_of(interval)
    }

    /// Whether this is the first playlist push seen for a camera since
    /// the process started — the flag is set as a side effect, because
    /// that is what makes the log one line rather than one per second.
    pub fn first_playlist_push(&self, camera_id: &str) -> bool {
        self.first_playlist_logged
            .lock()
            .expect("log set poisoned")
            .insert(camera_id.to_string())
    }

    /// The same, for the first `stream.m3u8` fetch.
    pub fn first_stream_get(&self, camera_id: &str) -> bool {
        self.first_stream_get_logged
            .lock()
            .expect("log set poisoned")
            .insert(camera_id.to_string())
    }

    // ── stream access logging ────────────────────────────────────────

    /// Whether a stream-access row is due for this user and camera, and
    /// stamp it if so.
    pub fn access_log_due(&self, user_id: &str, camera_id: &str) -> bool {
        let mut logged = self.access_logged.lock().expect("access log poisoned");
        let key = (user_id.to_string(), camera_id.to_string());
        let now = Instant::now();
        match logged.get(&key) {
            Some(last) if now.duration_since(*last) < ACCESS_LOG_INTERVAL => false,
            _ => {
                logged.insert(key, now);
                true
            }
        }
    }

    // ── viewer hours ─────────────────────────────────────────────────

    /// `record_viewer_second` — one per segment actually served.
    pub fn record_viewer_second(&self, org_id: &str) {
        let key = (org_id.to_string(), current_year_month());
        let mut viewer = self.viewer.lock().expect("viewer usage poisoned");
        *viewer.pending.entry(key).or_insert(0) += 1;
    }

    /// The cached total plus pending, if this org has been warmed this
    /// month.
    fn cached_viewer_seconds(&self, org_id: &str) -> Option<i64> {
        let key = (org_id.to_string(), current_year_month());
        let viewer = self.viewer.lock().expect("viewer usage poisoned");
        viewer
            .cached
            .get(&key)
            .map(|total| total + viewer.pending.get(&key).copied().unwrap_or(0))
    }

    fn store_cached_viewer_seconds(&self, org_id: &str, seconds: i64) -> i64 {
        let key = (org_id.to_string(), current_year_month());
        let mut viewer = self.viewer.lock().expect("viewer usage poisoned");
        viewer.cached.insert(key.clone(), seconds);
        seconds + viewer.pending.get(&key).copied().unwrap_or(0)
    }

    /// `_warm_cached_viewer_seconds`: the authoritative total, reading
    /// the database once per org per process.
    ///
    /// A failed read is logged and counted as zero, as in the Python —
    /// the cap is not worth failing a segment over.
    pub async fn warm_viewer_seconds(&self, pool: &sqlx::PgPool, org_id: &str) -> i64 {
        if let Some(total) = self.cached_viewer_seconds(org_id) {
            return total;
        }
        let seconds: i64 = match sqlx::query_scalar(
            "SELECT viewer_seconds FROM org_monthly_usage WHERE org_id = $1 AND year_month = $2",
        )
        .bind(org_id)
        .bind(current_year_month())
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row.map(|v: i32| i64::from(v)).unwrap_or(0),
            Err(err) => {
                tracing::error!(error = %err, org_id, "[ViewerUsage] Failed to warm cache");
                0
            }
        };
        self.store_cached_viewer_seconds(org_id, seconds)
    }

    /// `flush_viewer_usage`: write the pending increments and return how
    /// many `(org, month)` rows were touched.
    ///
    /// The pending map is cleared before the write, as in the Python: a
    /// failed flush loses those increments rather than accumulating
    /// them through an outage.
    pub async fn flush_viewer_usage(&self, pool: &sqlx::PgPool) -> usize {
        let snapshot: Vec<((String, String), i64)> = {
            let mut viewer = self.viewer.lock().expect("viewer usage poisoned");
            if viewer.pending.is_empty() {
                return 0;
            }
            viewer.pending.drain().collect()
        };

        let touched = snapshot.len();
        for ((org_id, year_month), delta) in snapshot {
            if delta <= 0 {
                continue;
            }
            let updated: Result<Option<(i32,)>, _> = sqlx::query_as(
                "INSERT INTO org_monthly_usage (org_id, year_month, viewer_seconds, updated_at)
                      VALUES ($1, $2, $3, $4)
                 ON CONFLICT (org_id, year_month)
                   DO UPDATE SET viewer_seconds = org_monthly_usage.viewer_seconds + $3,
                                 updated_at = $4
                   RETURNING viewer_seconds",
            )
            .bind(&org_id)
            .bind(&year_month)
            .bind(i32::try_from(delta).unwrap_or(i32::MAX))
            .bind(crate::models::now_naive())
            .fetch_optional(pool)
            .await;

            match updated {
                Ok(Some((total,))) => {
                    let mut viewer = self.viewer.lock().expect("viewer usage poisoned");
                    viewer.cached.insert((org_id, year_month), i64::from(total));
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::error!(error = %err, "[ViewerUsage] Flush failed — pending increments lost");
                    return 0;
                }
            }
        }
        touched
    }
}

/// The two background loops `main.py` starts for these caches.
///
/// Both sleep first, as the Python's do, so a restart does not write on
/// boot. Their intervals are configurable where the Python's are
/// literals: the harness needs to be able to stretch them out of the
/// way of a comparison, which is exactly what it cannot do to the
/// Python side.
pub fn spawn_loops(state: crate::app::AppState) {
    let flush_every = interval_from_env("VIEWER_USAGE_FLUSH_INTERVAL_SECONDS", 60);
    let evict_every = interval_from_env("SEGMENT_CACHE_EVICT_INTERVAL_SECONDS", 60);

    // `_viewer_usage_flush_loop`: one upsert per active org per minute,
    // instead of one write per segment served.
    let flusher = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(flush_every).await;
            let touched = flusher.hls.flush_viewer_usage(&flusher.pool).await;
            if touched > 0 {
                tracing::debug!(orgs = touched, "[ViewerUsage] flushed");
            }
        }
    });

    // `_segment_cache_evict_loop`: the 60-second inactivity cutoff has
    // to hold whether or not anything is still pushing — otherwise a
    // fleet that goes quiet leaves its last segments in memory until
    // something else happens to sweep.
    let evictor = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(evict_every).await;
            evictor.hls.evict_caches();
        }
    });
}

fn interval_from_env(name: &str, default_secs: u64) -> Duration {
    let secs = std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default_secs);
    Duration::from_secs(secs.max(1))
}

/// `_current_year_month` — the bucket key, so a new month needs nothing
/// evicted.
fn current_year_month() -> String {
    Utc::now().format("%Y-%m").to_string()
}

/// `_evict_segment_cache`: keep the newest `max_per_camera`.
fn evict_per_camera(store: &mut SegmentStore, camera_id: &str, max_per_camera: usize) {
    let Some(bucket) = store.cameras.get_mut(camera_id) else {
        return;
    };
    if bucket.len() <= max_per_camera {
        return;
    }
    let drop_count = bucket.len() - max_per_camera;
    // Oldest by filename, which is how the sequence numbers sort.
    let doomed: Vec<String> = bucket.keys().take(drop_count).cloned().collect();
    for filename in doomed {
        if let Some((body, _)) = bucket.remove(&filename) {
            store.byte_total -= body.len() as i64;
        }
    }
}

/// `_evict_global_oldest`: drop the oldest segments across every camera
/// until the total fits.
///
/// Freshness over fairness, deliberately: when the ceiling is hit, the
/// live edge of every active camera is worth more than the tail of any
/// one of them. Eviction goes to 95% of the cap rather than exactly to
/// it, so a cache sitting at the ceiling does not re-run this walk on
/// every single push.
fn evict_global_oldest(store: &mut SegmentStore, max_total_bytes: i64) -> usize {
    if store.byte_total <= max_total_bytes {
        return 0;
    }
    let low_water = (max_total_bytes as f64 * 0.95) as i64;

    let mut candidates: Vec<(Instant, String, String, i64)> = store
        .cameras
        .iter()
        .flat_map(|(camera_id, bucket)| {
            bucket.iter().map(move |(filename, (body, ts))| {
                (*ts, camera_id.clone(), filename.clone(), body.len() as i64)
            })
        })
        .collect();
    candidates.sort();

    let mut evicted = 0;
    for (_, camera_id, filename, size) in candidates {
        if store.byte_total <= low_water {
            break;
        }
        let Some(bucket) = store.cameras.get_mut(&camera_id) else {
            continue;
        };
        if bucket.remove(&filename).is_some() {
            store.byte_total -= size;
            evicted += 1;
        }
        // Drop the empty bucket too, so the camera count stays honest.
        if bucket.is_empty() {
            store.cameras.remove(&camera_id);
        }
    }
    if evicted > 0 {
        tracing::warn!(
            evicted,
            total = store.byte_total,
            cap = max_total_bytes,
            "[HLS] Global cache cap hit — evicted oldest segments"
        );
    }
    evicted
}

/// `_recompute_segment_cache_bytes`: the authoritative total, walked.
fn recompute_bytes(store: &SegmentStore) -> i64 {
    store
        .cameras
        .values()
        .flat_map(BTreeMap::values)
        .map(|(body, _)| body.len() as i64)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(n: u32, size: usize) -> (String, Bytes) {
        (format!("segment_{n:05}.ts"), Bytes::from(vec![b'x'; size]))
    }

    #[test]
    fn the_per_camera_cap_keeps_the_newest_by_filename() {
        let cache = HlsCache::new();
        for n in 1..=5 {
            let (name, body) = seg(n, 10);
            cache.push_segment("cam", &name, body, 3, i64::MAX);
        }
        assert_eq!(cache.segment_count("cam"), 3);
        assert!(cache.segment("cam", "segment_00002.ts").is_none());
        assert!(cache.segment("cam", "segment_00005.ts").is_some());
        // Ten bytes each, three left.
        assert_eq!(cache.total_bytes(), 30);
    }

    #[test]
    fn a_repushed_filename_does_not_double_count() {
        let cache = HlsCache::new();
        let (name, _) = seg(1, 0);
        cache.push_segment("cam", &name, Bytes::from(vec![b'x'; 100]), 60, i64::MAX);
        cache.push_segment("cam", &name, Bytes::from(vec![b'y'; 40]), 60, i64::MAX);
        assert_eq!(cache.segment_count("cam"), 1);
        assert_eq!(cache.total_bytes(), 40);
    }

    #[test]
    fn the_global_cap_evicts_oldest_first_down_to_the_low_water_mark() {
        let cache = HlsCache::new();
        // One camera's segments are all older than the other's, and the
        // cap is hit while filling the second — so the first is the one
        // that loses, whatever its own recent activity.
        for n in 1..=15 {
            let (name, body) = seg(n, 100);
            cache.push_segment("old", &name, body, 60, 2000);
        }
        for n in 1..=15 {
            let (name, body) = seg(n, 100);
            cache.push_segment("new", &name, body, 60, 2000);
        }
        assert!(cache.total_bytes() <= 2000, "total {}", cache.total_bytes());
        // 95% of the cap, so the walk is not re-run on every push.
        assert!(cache.total_bytes() >= 1800, "total {}", cache.total_bytes());
        // The oldest camera lost more than the newest.
        assert!(cache.segment_count("old") < cache.segment_count("new"));
    }

    #[test]
    fn the_byte_total_survives_every_path() {
        let cache = HlsCache::new();
        for n in 1..=20 {
            let (name, body) = seg(n, 50);
            cache.push_segment("a", &name, body, 5, 400);
            let (name, body) = seg(n, 30);
            cache.push_segment("b", &name, body, 5, 400);
        }
        cache.cleanup_camera("a");
        let walked = {
            let store = cache.segments.lock().unwrap();
            recompute_bytes(&store)
        };
        assert_eq!(cache.total_bytes(), walked);
    }

    #[test]
    fn a_snapshot_takes_the_newest_oldest_first() {
        let cache = HlsCache::new();
        for n in 1..=5 {
            let (name, _) = seg(n, 0);
            cache.push_segment("cam", &name, Bytes::from(vec![n as u8]), 60, i64::MAX);
        }
        let Snapshot::Segments(got) = cache.snapshot_recent("cam", 2) else {
            panic!("expected segments");
        };
        assert_eq!(got, vec![Bytes::from(vec![4u8]), Bytes::from(vec![5u8])]);
        // More than there are is not an error.
        let Snapshot::Segments(all) = cache.snapshot_recent("cam", 99) else {
            panic!("expected segments");
        };
        assert_eq!(all.len(), 5);
        assert_eq!(cache.snapshot_recent("cam", 0), Snapshot::Segments(Vec::new()));
        // A camera with no bucket is distinguishable from an empty one,
        // because the caller says "stream must be live" for the first.
        assert_eq!(cache.snapshot_recent("nope", 2), Snapshot::NoCamera);
    }

    #[test]
    fn cleanup_drops_every_cache_for_one_camera() {
        let cache = HlsCache::new();
        let (name, body) = seg(1, 10);
        cache.push_segment("cam", &name, body, 60, i64::MAX);
        cache.set_playlist("cam", "#EXTM3U".into());
        cache.bump_playlist_count("cam", 20);
        assert!(cache.first_playlist_push("cam"), "the first push is the first");
        assert!(!cache.first_playlist_push("cam"), "and the second is not");

        cache.cleanup_camera("cam");
        assert_eq!(cache.segment_count("cam"), 0);
        assert_eq!(cache.total_bytes(), 0);
        assert!(cache.playlist("cam").is_none());
        // The one-shot log flags reset with it, so a reconnect relogs.
        assert!(cache.first_playlist_push("cam"));
    }

    #[test]
    fn the_sweep_due_flag_follows_the_interval() {
        let cache = HlsCache::new();
        let due: Vec<bool> = (1..=6).map(|_| cache.bump_playlist_count("cam", 3)).collect();
        assert_eq!(due, vec![false, false, true, false, false, true]);
    }

    #[test]
    fn an_access_log_is_due_once_per_interval() {
        let cache = HlsCache::new();
        assert!(cache.access_log_due("user", "cam"));
        assert!(!cache.access_log_due("user", "cam"));
        // Another camera, or another user, is its own budget.
        assert!(cache.access_log_due("user", "other"));
        assert!(cache.access_log_due("other", "cam"));
    }
}
