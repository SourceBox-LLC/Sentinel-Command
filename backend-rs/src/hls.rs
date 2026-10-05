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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
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

/// A segment filename, ordered by its sequence number.
///
/// The cache is a `BTreeMap` so "oldest" and "newest" are its two ends.
/// Keyed by the bare filename, that order was the STRING order, which
/// matches the sequence only while every number has the same width:
/// CameraNode writes `segment_%05d.ts`, so after `segment_99999.ts` —
/// about 28 hours of continuous streaming at one-second segments — comes
/// `segment_100000.ts`, which sorts first. From then on every new
/// segment was the "oldest" and was evicted the moment it arrived
/// (live video froze), and clips and agent snapshots took the newest
/// segments from the wrong end. The Python sorted the same way.
///
/// Compared by the digits after `segment_` as a number of any width
/// (length first, then value), with the full name as the tiebreak, so
/// the order is total and a name with no digits still sorts somewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentName(String);

impl SegmentName {
    pub fn new(filename: &str) -> Self {
        Self(filename.to_string())
    }

    fn sequence(&self) -> &str {
        let digits = self.0.strip_prefix("segment_").unwrap_or(&self.0);
        let end = digits
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(digits.len());
        digits[..end].trim_start_matches('0')
    }
}

impl Ord for SegmentName {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let (a, b) = (self.sequence(), other.sequence());
        a.len()
            .cmp(&b.len())
            .then_with(|| a.cmp(b))
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for SegmentName {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
struct SegmentStore {
    /// `{camera_id: {filename: (bytes, monotonic)}}`, each camera's
    /// segments in sequence order — see [`SegmentName`].
    cameras: HashMap<String, BTreeMap<SegmentName, (Bytes, Instant)>>,
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

// ── the auth cache ───────────────────────────────────────────────────
//
// The live path's per-request database work, held for a few seconds.
//
// A CameraNode pushes a segment and a playlist every second per camera,
// and each push used to cost two queries — "which node has this key",
// "does that node own this camera" — before a byte was cached. A viewer
// costs one more per segment ("does this org have this camera"). On the
// hosted deployment the database is across a network, so that was three
// to five round trips per camera per second spent re-learning facts that
// change a handful of times a month. The plan for the rewrite named this
// as the one inefficiency in the Python not to carry over; it was
// carried over, and this is the fix.
//
// Correctness rests on two rules:
//
// * **Only positive answers are cached.** A new node or camera works on
//   its first push; nothing is ever refused from cache.
// * **Every write that changes a cached fact calls
//   [`invalidate_auth_cache`] AFTER it commits**, and a reader tags what
//   it caches with the generation it saw BEFORE it queried. A lookup that
//   raced a key rotation therefore cannot re-cache the old key: its tag
//   is stale by the time it tries, and the entry is dropped. The sites:
//   key rotation, node delete and decommission, the GDPR reset, stale-
//   camera removal and rename on register, and `enforce_camera_cap`.
//
// The TTL is the backstop for a write this process did not make — an
// operator in `psql`. `AUTH_CACHE_SECONDS=0` turns caching off.

static AUTH_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Forget every cached auth answer. Call after the write has committed.
pub fn invalidate_auth_cache() {
    AUTH_GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// The generation to tag a lookup with. Read it BEFORE the query.
pub fn auth_generation() -> u64 {
    AUTH_GENERATION.load(Ordering::Acquire)
}

fn auth_ttl() -> Duration {
    static TTL: OnceLock<Duration> = OnceLock::new();
    *TTL.get_or_init(|| {
        let secs = std::env::var("AUTH_CACHE_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(10);
        Duration::from_secs(secs)
    })
}

/// A cap, not an LRU: past it the map is simply emptied. Entries are a
/// few seconds old at most, so the cost is one round of re-learning.
const AUTH_CACHE_MAX_ENTRIES: usize = 10_000;

/// The node a push key belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushNode {
    pub pk: i32,
    pub org_id: String,
    pub node_id: String,
}

/// What a push needs to know about the camera it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushCamera {
    pub name: String,
    pub disabled_by_plan: bool,
}

#[derive(Default)]
struct AuthCache {
    generation: u64,
    /// `sha256(key)` → node.
    nodes: HashMap<String, (PushNode, Instant)>,
    /// `(node pk, camera_id)` → camera.
    cameras: HashMap<(i32, String), (PushCamera, Instant)>,
    /// `(org_id, camera_id)` the org can view → the public id of the
    /// node it is on.
    viewable: HashMap<(String, String), (String, Instant)>,
}

impl AuthCache {
    /// Empty the maps if anything was invalidated since they were filled.
    fn sync(&mut self) {
        let now = auth_generation();
        if self.generation != now {
            self.nodes.clear();
            self.cameras.clear();
            self.viewable.clear();
            self.generation = now;
        }
    }
}

fn fresh(at: Instant) -> bool {
    at.elapsed() < auth_ttl()
}

/// Insert unless the world moved on since `seen` was read.
fn put<K: std::hash::Hash + Eq, V>(
    map: &mut HashMap<K, V>,
    current: u64,
    seen: u64,
    key: K,
    value: V,
) {
    if seen != current || auth_ttl().is_zero() {
        return;
    }
    if map.len() >= AUTH_CACHE_MAX_ENTRIES {
        map.clear();
    }
    map.insert(key, value);
}

/// Everything `hls.py` holds at module level.
#[derive(Default)]
pub struct HlsCache {
    auth: Mutex<AuthCache>,
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

    // ── auth ─────────────────────────────────────────────────────────

    pub fn cached_push_node(&self, key_hash: &str) -> Option<PushNode> {
        let mut auth = lock(&self.auth);
        auth.sync();
        auth.nodes
            .get(key_hash)
            .filter(|(_, at)| fresh(*at))
            .map(|(node, _)| node.clone())
    }

    pub fn cache_push_node(&self, seen: u64, key_hash: &str, node: &PushNode) {
        let mut auth = lock(&self.auth);
        auth.sync();
        let current = auth.generation;
        put(
            &mut auth.nodes,
            current,
            seen,
            key_hash.to_string(),
            (node.clone(), Instant::now()),
        );
    }

    pub fn cached_push_camera(&self, node_pk: i32, camera_id: &str) -> Option<PushCamera> {
        let mut auth = lock(&self.auth);
        auth.sync();
        auth.cameras
            .get(&(node_pk, camera_id.to_string()))
            .filter(|(_, at)| fresh(*at))
            .map(|(camera, _)| camera.clone())
    }

    pub fn cache_push_camera(&self, seen: u64, node_pk: i32, camera_id: &str, camera: &PushCamera) {
        let mut auth = lock(&self.auth);
        auth.sync();
        let current = auth.generation;
        put(
            &mut auth.cameras,
            current,
            seen,
            (node_pk, camera_id.to_string()),
            (camera.clone(), Instant::now()),
        );
    }

    /// The public id of the node a viewable camera is on, if cached.
    pub fn cached_viewable(&self, org_id: &str, camera_id: &str) -> Option<String> {
        let mut auth = lock(&self.auth);
        auth.sync();
        auth.viewable
            .get(&(org_id.to_string(), camera_id.to_string()))
            .filter(|(_, at)| fresh(*at))
            .map(|(node_id, _)| node_id.clone())
    }

    pub fn cache_viewable(&self, seen: u64, org_id: &str, camera_id: &str, node_id: &str) {
        let mut auth = lock(&self.auth);
        auth.sync();
        let current = auth.generation;
        put(
            &mut auth.viewable,
            current,
            seen,
            (org_id.to_string(), camera_id.to_string()),
            (node_id.to_string(), Instant::now()),
        );
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
        let mut store = lock(&self.segments);
        let bucket = store.cameras.entry(camera_id.to_string()).or_default();
        // A re-push of the same filename overwrites, so the old size
        // comes off the total before the new one goes on. A flaky
        // network retry does exactly this.
        let previous = bucket.insert(SegmentName::new(filename), (body.clone(), Instant::now()));
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
        let store = lock(&self.segments);
        store
            .cameras
            .get(camera_id)
            .and_then(|bucket| bucket.get(&SegmentName::new(filename)))
            .map(|(body, _)| body.clone())
    }

    /// How many segments a camera has cached — for the one-shot
    /// diagnostic lines, which report it.
    pub fn segment_count(&self, camera_id: &str) -> usize {
        let store = lock(&self.segments);
        store.cameras.get(camera_id).map_or(0, BTreeMap::len)
    }

    /// `_segment_cache_total_bytes`.
    /// `(len(_playlist_cache), len(_segment_cache))` — what
    /// `/api/health/detailed` reports as cache occupancy.
    ///
    /// Both are counts of CAMERAS, not of segments: the segment figure
    /// is how many cameras have a bucket, however full.
    pub fn cache_occupancy(&self) -> (usize, usize) {
        let playlists = lock(&self.playlists).len();
        let segments = lock(&self.segments).cameras.len();
        (playlists, segments)
    }

    /// `sum(_pending_viewer_seconds.values())` — seconds counted but
    /// not yet flushed to `org_monthly_usage`.
    pub fn pending_viewer_seconds(&self) -> i64 {
        lock(&self.viewer).pending.values().sum()
    }

    pub fn total_bytes(&self) -> i64 {
        lock(&self.segments).byte_total
    }

    /// `snapshot_recent_segment_bytes`: the newest `count` segments,
    /// oldest first, taken under the lock so an eviction cannot run
    /// between choosing the filenames and reading them.
    pub fn snapshot_recent(&self, camera_id: &str, count: usize) -> Snapshot {
        let store = lock(&self.segments);
        let Some(bucket) = store.cameras.get(camera_id).filter(|b| !b.is_empty()) else {
            return Snapshot::NoCamera;
        };
        if count == 0 {
            return Snapshot::Segments(Vec::new());
        }
        let skip = bucket.len().saturating_sub(count);
        Snapshot::Segments(
            bucket
                .values()
                .skip(skip)
                .map(|(body, _)| body.clone())
                .collect(),
        )
    }

    /// `cleanup_camera_cache`: everything held for one camera, across
    /// every cache. Called when a camera or its node goes away.
    pub fn cleanup_camera(&self, camera_id: &str) {
        {
            let mut store = lock(&self.segments);
            if let Some(bucket) = store.cameras.remove(camera_id) {
                let freed: i64 = bucket.values().map(|(body, _)| body.len() as i64).sum();
                store.byte_total -= freed;
            }
        }
        lock(&self.playlists).remove(camera_id);
        lock(&self.playlist_updates).remove(camera_id);
        lock(&self.first_playlist_logged).remove(camera_id);
        lock(&self.first_stream_get_logged).remove(camera_id);
    }

    /// Age every one of a camera's segments, for tests that need the
    /// stale sweep to have something to sweep. The sweep's cutoff is a
    /// minute, which is longer than any test should take.
    #[cfg(test)]
    fn backdate(&self, camera_id: &str, by: Duration) {
        let mut store = lock(&self.segments);
        if let Some(bucket) = store.cameras.get_mut(camera_id) {
            for (_, ts) in bucket.values_mut() {
                *ts -= by;
            }
        }
    }

    /// `_evict_stale_cameras`, including the counter reconciliation the
    /// Python does on the same sweep.
    pub fn evict_stale_cameras(&self) {
        let stale: Vec<String> = {
            let mut store = lock(&self.segments);
            let cutoff = Instant::now() - STALE_CAMERA_AGE;
            let stale: Vec<String> = store
                .cameras
                .iter()
                .filter(|(_, bucket)| {
                    bucket.is_empty()
                        || bucket
                            .values()
                            .map(|(_, ts)| *ts)
                            .max()
                            .is_some_and(|newest| newest < cutoff)
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
            lock(&self.playlists).remove(camera_id);
            lock(&self.playlist_updates).remove(camera_id);
            lock(&self.first_playlist_logged).remove(camera_id);
            lock(&self.first_stream_get_logged).remove(camera_id);
        }
    }

    /// `_evict_caches`: the sweep the playlist push runs every
    /// `CLEANUP_INTERVAL` pushes, and a background loop runs on a timer.
    pub fn evict_caches(&self) {
        {
            let mut playlists = lock(&self.playlists);
            if playlists.len() > CACHE_MAX_CAMERAS {
                let mut by_age: Vec<(String, Instant)> = playlists
                    .iter()
                    .map(|(k, (_, ts))| (k.clone(), *ts))
                    .collect();
                by_age.sort_by_key(|(_, ts)| *ts);
                let drop_count = by_age.len() - CACHE_MAX_CAMERAS;
                let mut counts = lock(&self.playlist_updates);
                for (camera_id, _) in by_age.into_iter().take(drop_count) {
                    playlists.remove(&camera_id);
                    counts.remove(&camera_id);
                }
            }
        }
        {
            let mut logged = lock(&self.access_logged);
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
        lock(&self.playlists).insert(camera_id.to_string(), (playlist, Instant::now()));
    }

    /// The cached playlist and its age, whether or not it is still
    /// fresh — the caller decides, because the miss path logs whether
    /// there was one at all.
    pub fn playlist(&self, camera_id: &str) -> Option<(String, Duration)> {
        let playlists = lock(&self.playlists);
        playlists
            .get(camera_id)
            .map(|(text, ts)| (text.clone(), ts.elapsed()))
    }

    /// Count this playlist push and say whether the sweep is due.
    pub fn bump_playlist_count(&self, camera_id: &str, interval: u64) -> bool {
        let mut counts = lock(&self.playlist_updates);
        let count = counts.entry(camera_id.to_string()).or_insert(0);
        *count += 1;
        interval != 0 && (*count).is_multiple_of(interval)
    }

    /// Whether this is the first playlist push seen for a camera since
    /// the process started — the flag is set as a side effect, because
    /// that is what makes the log one line rather than one per second.
    pub fn first_playlist_push(&self, camera_id: &str) -> bool {
        lock(&self.first_playlist_logged).insert(camera_id.to_string())
    }

    /// The same, for the first `stream.m3u8` fetch.
    pub fn first_stream_get(&self, camera_id: &str) -> bool {
        lock(&self.first_stream_get_logged).insert(camera_id.to_string())
    }

    // ── stream access logging ────────────────────────────────────────

    /// Whether a stream-access row is due for this user and camera, and
    /// stamp it if so.
    pub fn access_log_due(&self, user_id: &str, camera_id: &str) -> bool {
        let mut logged = lock(&self.access_logged);
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
        let mut viewer = lock(&self.viewer);
        *viewer.pending.entry(key).or_insert(0) += 1;
    }

    /// The cached total plus pending, if this org has been warmed this
    /// month.
    fn cached_viewer_seconds(&self, org_id: &str) -> Option<i64> {
        let key = (org_id.to_string(), current_year_month());
        let viewer = lock(&self.viewer);
        viewer
            .cached
            .get(&key)
            .map(|total| total + viewer.pending.get(&key).copied().unwrap_or(0))
    }

    fn store_cached_viewer_seconds(&self, org_id: &str, seconds: i64) -> i64 {
        let key = (org_id.to_string(), current_year_month());
        let mut viewer = lock(&self.viewer);
        viewer.cached.insert(key.clone(), seconds);
        seconds + viewer.pending.get(&key).copied().unwrap_or(0)
    }

    /// `_warm_cached_viewer_seconds`: the authoritative total, reading
    /// the database once per org per process.
    ///
    /// A failed read is logged and counted as zero, as in the Python —
    /// the cap is not worth failing a segment over.
    pub async fn warm_viewer_seconds(&self, pool: &crate::db::Pool, org_id: &str) -> i64 {
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
    pub async fn flush_viewer_usage(&self, pool: &crate::db::Pool) -> usize {
        let snapshot: Vec<((String, String), i64)> = {
            let mut viewer = lock(&self.viewer);
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
                    let mut viewer = lock(&self.viewer);
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

/// Take a lock, through poisoning.
///
/// Every critical section here is a few map operations with no code that
/// can panic halfway through an invariant, so a poisoned lock means a
/// panic happened somewhere ELSE while it was held — and the data is as
/// good as it was. `expect` would turn that one panic into every later
/// request panicking: all live video, for every org, until a restart.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    // Oldest by sequence number — see `SegmentName`.
    let doomed: Vec<SegmentName> = bucket.keys().take(drop_count).cloned().collect();
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

    let mut candidates: Vec<(Instant, String, SegmentName, i64)> = store
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

    /// The generation is process-wide, so tests that invalidate it run
    /// one at a time — otherwise one test's invalidation empties
    /// another's cache mid-assertion.
    static AUTH_TESTS: Mutex<()> = Mutex::new(());

    fn node(pk: i32) -> PushNode {
        PushNode {
            pk,
            org_id: "org".into(),
            node_id: format!("node-{pk}"),
        }
    }

    /// A key looked up once is answered from memory, and a rotation
    /// (any invalidation) takes it back out at once.
    #[test]
    fn a_cached_push_key_is_forgotten_the_moment_it_is_invalidated() {
        let _serial = lock(&AUTH_TESTS);
        let cache = HlsCache::new();
        let seen = auth_generation();
        cache.cache_push_node(seen, "hash-a", &node(1));
        assert_eq!(cache.cached_push_node("hash-a"), Some(node(1)));
        invalidate_auth_cache();
        assert_eq!(cache.cached_push_node("hash-a"), None);
    }

    /// The race the generation tag exists for: a lookup reads the OLD
    /// key, a rotation commits and invalidates, and only then does the
    /// lookup try to cache what it read. It must not stick.
    #[test]
    fn a_lookup_that_raced_an_invalidation_is_not_cached() {
        let _serial = lock(&AUTH_TESTS);
        let cache = HlsCache::new();
        let seen = auth_generation();
        invalidate_auth_cache(); // the rotation lands mid-lookup
        cache.cache_push_node(seen, "hash-old", &node(2));
        assert_eq!(cache.cached_push_node("hash-old"), None);

        cache.cache_push_camera(
            seen,
            2,
            "cam",
            &PushCamera {
                name: "Driveway".into(),
                disabled_by_plan: false,
            },
        );
        assert_eq!(cache.cached_push_camera(2, "cam"), None);
        cache.cache_viewable(seen, "org", "cam", "node-1");
        assert_eq!(cache.cached_viewable("org", "cam"), None);
    }

    /// A plan-cap flip has to reach the next push.
    #[test]
    fn a_camera_entry_carries_its_plan_flag_until_invalidated() {
        let _serial = lock(&AUTH_TESTS);
        let cache = HlsCache::new();
        let camera = PushCamera {
            name: "Gate".into(),
            disabled_by_plan: false,
        };
        cache.cache_push_camera(auth_generation(), 3, "gate", &camera);
        assert_eq!(cache.cached_push_camera(3, "gate"), Some(camera));
        assert_eq!(cache.cached_push_camera(3, "other"), None);
        assert_eq!(
            cache.cached_push_camera(4, "gate"),
            None,
            "another node's camera"
        );
        invalidate_auth_cache();
        assert_eq!(cache.cached_push_camera(3, "gate"), None);
    }

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
        assert_eq!(
            cache.snapshot_recent("cam", 0),
            Snapshot::Segments(Vec::new())
        );
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
        assert!(
            cache.first_playlist_push("cam"),
            "the first push is the first"
        );
        assert!(!cache.first_playlist_push("cam"), "and the second is not");

        cache.cleanup_camera("cam");
        assert_eq!(cache.segment_count("cam"), 0);
        assert_eq!(cache.total_bytes(), 0);
        assert!(cache.playlist("cam").is_none());
        // The one-shot log flags reset with it, so a reconnect relogs.
        assert!(cache.first_playlist_push("cam"));
    }

    #[test]
    fn the_stale_sweep_drops_a_camera_that_stopped_pushing() {
        let cache = HlsCache::new();
        let (name, body) = seg(1, 10);
        cache.push_segment("quiet", &name, body, 60, i64::MAX);
        cache.set_playlist("quiet", "#EXTM3U".into());
        let (name, body) = seg(1, 10);
        cache.push_segment("busy", &name, body, 60, i64::MAX);

        // Still inside the minute: nothing goes.
        cache.backdate("quiet", Duration::from_secs(59));
        cache.evict_stale_cameras();
        assert_eq!(cache.segment_count("quiet"), 1);

        cache.backdate("quiet", Duration::from_secs(2));
        cache.evict_stale_cameras();
        assert_eq!(cache.segment_count("quiet"), 0);
        // Its siblings go with it, and the byte total follows.
        assert!(cache.playlist("quiet").is_none());
        assert_eq!(cache.total_bytes(), 10);
        // The camera still pushing is untouched.
        assert_eq!(cache.segment_count("busy"), 1);
    }

    #[test]
    fn the_sweep_due_flag_follows_the_interval() {
        let cache = HlsCache::new();
        let due: Vec<bool> = (1..=6)
            .map(|_| cache.bump_playlist_count("cam", 3))
            .collect();
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

    /// Past `segment_99999.ts` the names grow a digit. Ordered as strings,
    /// `segment_100000.ts` came first, so every new segment was evicted
    /// as the oldest and live video froze about 28 hours into a stream.
    #[test]
    fn the_cache_keeps_the_newest_segments_across_the_six_digit_boundary() {
        let cache = HlsCache::new();
        for n in 99_998..100_003 {
            cache.push_segment(
                "cam",
                &format!("segment_{n:05}.ts"),
                Bytes::from(n.to_string()),
                3,
                i64::MAX,
            );
        }
        // The three newest survive and the oldest went.
        for n in 100_000..100_003 {
            assert!(
                cache.segment("cam", &format!("segment_{n}.ts")).is_some(),
                "{n} evicted"
            );
        }
        assert!(cache.segment("cam", "segment_99998.ts").is_none());
        // And "the most recent two" are the two highest, oldest first.
        let Snapshot::Segments(recent) = cache.snapshot_recent("cam", 2) else {
            panic!("no segments");
        };
        assert_eq!(recent, vec![Bytes::from("100001"), Bytes::from("100002")]);
    }

    #[test]
    fn segment_names_order_by_number_of_any_width() {
        let mut names: Vec<SegmentName> = [
            "segment_100000.ts",
            "segment_00009.ts",
            "segment_99999.ts",
            "segment_10.ts",
        ]
        .iter()
        .map(|n| SegmentName::new(n))
        .collect();
        names.sort();
        let order: Vec<&str> = names.iter().map(|n| n.0.as_str()).collect();
        assert_eq!(
            order,
            [
                "segment_00009.ts",
                "segment_10.ts",
                "segment_99999.ts",
                "segment_100000.ts"
            ]
        );
    }
}
