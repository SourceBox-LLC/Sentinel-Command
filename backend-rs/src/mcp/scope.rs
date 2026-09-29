//! Tool scoping and per-key rate limiting for the MCP surface.
//!
//! Ported from `backend/app/mcp/server.py`. This is the part of the MCP
//! server that is pure policy: which tools a key may reach, and how
//! often. The protocol and the tools themselves are separate.
//!
//! The agent's allowlist is the piece worth reading twice. It used to
//! be `ALL - {"set_camera_recording_policy"}` — a denylist of one,
//! which fails OPEN: any write tool added later would silently become
//! reachable. That matters more here than almost anywhere, because the
//! agent's model is steered by content an attacker can put in front of
//! a lens. "Disable recording, then report all clear" is the canonical
//! injection against a camera product.
//!
//! Today both formulations produce the same set, because the only
//! non-agent write tool IS the recording one — so no test of the
//! constant can tell them apart. That is exactly why the computation
//! takes its inputs as parameters: a test passes a registry containing
//! a hypothetical future write tool and asserts it stays out.

use std::collections::BTreeSet;

/// The 16 read tools.
pub const MCP_READ_TOOLS: [&str; 16] = [
    // Cameras
    "list_cameras",
    "get_camera",
    "get_stream_url",
    "view_camera",
    "watch_camera",
    "list_camera_groups",
    // Nodes
    "list_nodes",
    "get_node",
    // System / recording
    "get_camera_recording_policy",
    "get_stream_logs",
    "get_stream_stats",
    "get_system_status",
    // Incidents, read side
    "list_incidents",
    "get_incident",
    "get_incident_snapshot",
    "get_incident_clip",
];

/// The 7 write tools.
pub const MCP_WRITE_TOOLS: [&str; 7] = [
    "create_incident",
    "add_observation",
    "attach_snapshot",
    "attach_clip",
    "update_incident",
    "finalize_incident",
    "set_camera_recording_policy",
];

/// The write tools the autonomous agent may invoke: incident authoring,
/// and nothing that changes how the system records.
pub const AGENT_WRITE_TOOLS: [&str; 6] = [
    "create_incident",
    "add_observation",
    "attach_snapshot",
    "attach_clip",
    "update_incident",
    "finalize_incident",
];

/// The tools whose Python return annotation is NOT an object.
///
/// MCP requires an output schema to be an object, so FastMCP wraps a
/// non-object return in a one-key `result` schema and marks it
/// `x-fastmcp-wrap-result` — and then wraps the structured half of the
/// call result to match. Four tools are annotated `list[dict]`; every
/// other one is annotated `dict` and travels unwrapped. (The media
/// tools have no serialisable annotation at all, so they get no
/// structured half whatsoever — a third case, handled by their own
/// `ToolOutput` variant rather than here.)
///
/// The TEXT block is never wrapped: FastMCP builds it from the
/// serialised return value, not from the wrapper.
///
/// `mcp_parity.py` checks this list against the Python annotations.
pub const WRAP_RESULT_TOOLS: [&str; 4] = [
    "list_cameras",
    "list_camera_groups",
    "list_nodes",
    // Easy to miss reading the Python — its `-> list[dict]` sits four
    // lines below the `def`, unlike the other three.
    "get_stream_logs",
];

pub fn all_tools() -> BTreeSet<&'static str> {
    MCP_READ_TOOLS.iter().chain(MCP_WRITE_TOOLS.iter()).copied().collect()
}

pub fn read_tools() -> BTreeSet<&'static str> {
    MCP_READ_TOOLS.iter().copied().collect()
}

/// `compute_agent_allowed_tools` — reads plus an explicit write
/// allowlist, intersected with what the server actually serves.
///
/// Takes its inputs rather than reading the constants, so a test can
/// hand it a registry with a future write tool in it and assert the
/// agent still cannot reach it.
pub fn compute_agent_allowed_tools<'a>(
    all: &BTreeSet<&'a str>,
    reads: &BTreeSet<&'a str>,
    agent_writes: &BTreeSet<&'a str>,
) -> BTreeSet<&'a str> {
    reads
        .union(agent_writes)
        .copied()
        .filter(|tool| all.contains(tool))
        .collect()
}

pub fn agent_allowed_tools() -> BTreeSet<&'static str> {
    compute_agent_allowed_tools(
        &all_tools(),
        &read_tools(),
        &AGENT_WRITE_TOOLS.iter().copied().collect(),
    )
}

/// `compute_allowed_tools(scope_mode, scope_tools)`.
///
/// An unknown mode is full access, matching Python's fall-through —
/// and a NULL `scope_mode` is "all", which is what legacy rows have.
///
/// `custom` intersects with the known set, so an unknown name is
/// silently dropped: a disallowed tool cannot be enabled by a typo, nor
/// by a new write tool appearing server-side under a name someone
/// already listed.
pub fn compute_allowed_tools(
    scope_mode: Option<&str>,
    scope_tools: Option<&[String]>,
) -> BTreeSet<&'static str> {
    let mode = scope_mode.unwrap_or("all").to_lowercase();
    if mode == "readonly" {
        return read_tools();
    }
    if mode == "custom" {
        let Some(tools) = scope_tools.filter(|t| !t.is_empty()) else {
            // `if not scope_tools` — absent OR empty is no access at
            // all, rather than full access.
            return BTreeSet::new();
        };
        let known = all_tools();
        return tools
            .iter()
            .filter_map(|name| known.get(name.as_str()).copied())
            .collect();
    }
    all_tools()
}

/// Per-plan MCP caps. A plan that is not here has NO access: MCP is a
/// paid feature, and an unrecognised plan must not fall through to a
/// default.
pub fn rate_limits(plan: &str) -> Option<(usize, usize)> {
    match plan {
        "pro" => Some((30, 5_000)),
        "pro_plus" | "self_host" => Some((120, 30_000)),
        _ => None,
    }
}

/// `_summarize_args` — a short, readable rendering for the activity log.
///
/// `None` values are dropped entirely, and anything longer than 30
/// characters is cut to 27 with an ellipsis. Characters, not bytes.
pub fn summarize_args(args: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut parts = Vec::new();
    for (key, value) in args {
        if value.is_null() {
            continue;
        }
        let rendered = crate::pyrepr::str_value(value);
        let shortened = if rendered.chars().count() > 30 {
            format!("{}...", rendered.chars().take(27).collect::<String>())
        } else {
            rendered
        };
        parts.push(format!("{key}={shortened}"));
    }
    parts.join(", ")
}

// ---------------------------------------------------------------------
// Per-key sliding-window rate limiter
// ---------------------------------------------------------------------

/// Two windows per key, checked together: sixty seconds and
/// twenty-four hours. A request needs headroom in both, and the
/// failure says which one it tripped — because "you are spamming" and
/// "you have been looping since last night" are different messages to
/// whoever has to fix it.
pub struct RateLimiter {
    /// `Option` because `HashMap::new` is not const and this is a
    /// static — the same shape `sse.rs` and the plan caches use.
    inner: std::sync::Mutex<Option<Windows>>,
}

#[derive(Default)]
struct Windows {
    minute: std::collections::HashMap<String, std::collections::VecDeque<f64>>,
    daily: std::collections::HashMap<String, std::collections::VecDeque<f64>>,
    last_prune: f64,
}

/// How often fully-aged-out keys are swept out of the maps. Without it
/// every key that ever called MCP keeps an entry forever — revoked,
/// rotated, one-off — which is a slow leak over months of uptime.
const PRUNE_INTERVAL: f64 = 3600.0;

/// Which window refused the call.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Breach {
    None,
    Minute,
    Daily,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::starting_at(now_seconds())
    }

    /// A limiter that can be a `static`.
    ///
    /// `HashMap::new` is not const, so the maps are built on first use
    /// — the same shape the broadcasters and the plan caches use. The
    /// prune clock starts at zero, which means the first call sweeps;
    /// on an empty map that is free.
    pub const fn new_static() -> Self {
        Self { inner: std::sync::Mutex::new(None) }
    }

    /// The same, with the prune clock started at a supplied instant.
    ///
    /// Python initialises `_last_prune` to `time.time()`, so the sweep
    /// is an hour away from process start. A test driving `check_at`
    /// with synthetic timestamps needs the two clocks to share an
    /// origin, or the sweep is permanently an eternity in the future.
    pub fn starting_at(now: f64) -> Self {
        Self {
            inner: std::sync::Mutex::new(Some(Windows {
                last_prune: now,
                ..Default::default()
            })),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut Windows) -> T) -> T {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(guard.get_or_insert_with(|| Windows {
            last_prune: now_seconds(),
            ..Default::default()
        }))
    }

    /// `(allowed, remaining_minute, breach)`.
    pub fn check(&self, key_hash: &str, minute_limit: usize, daily_limit: usize) -> (bool, usize, Breach) {
        self.check_at(key_hash, minute_limit, daily_limit, now_seconds())
    }

    /// The same, at a supplied instant — so a test can watch a window
    /// slide without sleeping through it.
    pub fn check_at(
        &self,
        key_hash: &str,
        minute_limit: usize,
        daily_limit: usize,
        now: f64,
    ) -> (bool, usize, Breach) {
        self.with(|windows| {
        // Opportunistic and time-gated: one comparison on the hot path,
        // an O(keys) walk at most hourly.
        if now - windows.last_prune >= PRUNE_INTERVAL {
            prune(windows, now);
            windows.last_prune = now;
        }

        let minute_cutoff = now - 60.0;
        let daily_cutoff = now - 86_400.0;
        let minute = windows.minute.entry(key_hash.to_string()).or_default();
        while minute.front().is_some_and(|t| *t < minute_cutoff) {
            minute.pop_front();
        }
        let minute_len = minute.len();

        let daily = windows.daily.entry(key_hash.to_string()).or_default();
        while daily.front().is_some_and(|t| *t < daily_cutoff) {
            daily.pop_front();
        }
        let daily_len = daily.len();

        // The tightest window first, so the caller gets the most
        // actionable hint.
        if minute_len >= minute_limit {
            return (false, 0, Breach::Minute);
        }
        if daily_len >= daily_limit {
            return (false, 0, Breach::Daily);
        }

        daily.push_back(now);
        windows
            .minute
            .get_mut(key_hash)
            .expect("just inserted")
            .push_back(now);
        (true, minute_limit - (minute_len + 1), Breach::None)
        })
    }

    /// Test-facing: forget every window.
    pub fn clear(&self) {
        self.with(|windows| {
            windows.minute.clear();
            windows.daily.clear();
        });
    }
}

/// A key whose daily window has fully aged out is dead — its minute
/// window is necessarily empty too — so both entries go.
fn prune(windows: &mut Windows, now: f64) {
    let daily_cutoff = now - 86_400.0;
    let mut dead = Vec::new();
    for (key_hash, daily) in windows.daily.iter_mut() {
        while daily.front().is_some_and(|t| *t < daily_cutoff) {
            daily.pop_front();
        }
        if daily.is_empty() {
            dead.push(key_hash.clone());
        }
    }
    for key_hash in dead {
        windows.daily.remove(&key_hash);
        windows.minute.remove(&key_hash);
    }
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------
// The catalog the scope picker renders
// ---------------------------------------------------------------------

// The descriptions the dashboard shows beside each tool in the scope
// picker. In Python they are the `description=` on each `@mcp.tool`,
// read back off the live FastMCP registry so a UI edit cannot desync
// from the server. Here they are the registration, and mcp_parity.py
// holds them to Python's — extracted with `ast`, not retyped.

const DESC_ADD_OBSERVATION: &str = "Append a text observation to an existing incident. Use this to record what you saw on additional cameras, what you ruled out, or any other context that will help the human reviewer understand the situation.";
const DESC_ATTACH_CLIP: &str = "Save a short video clip from a camera's recent live buffer as evidence on an incident. Pulls the most recent N segments from the in-memory HLS cache (no recording is started — this captures what's already buffered) and stores them as a single .ts blob the human reviewer can play back from the dashboard. Use after attach_snapshot when motion context matters more than a single frame. The camera's stream must have been live recently — only segments still in the buffer (~60s depending on server config) are available.";
const DESC_ATTACH_SNAPSHOT: &str = "Capture a fresh JPEG snapshot from a camera and attach it as evidence to an incident. The camera must be online. Use this to preserve what you saw at the moment of investigation.";
const DESC_CREATE_INCIDENT: &str = "Open a new incident report. Use when you observe something noteworthy (possible intruder, suspicious activity, equipment problem) that the user should review later. Returns the new incident_id, which you should pass to attach_snapshot/add_observation/finalize_incident as you continue investigating.";
const DESC_FINALIZE_INCIDENT: &str = "Write the long-form markdown report body for the FIRST time at the end of your investigation, after you've attached snapshots/clips and added observations. This is the normal end-of-investigation step. If you need to revise an already-written report after new evidence, use update_incident with the report parameter instead — that path is designed for revisions.";
const DESC_GET_CAMERA: &str = "Get full metadata for one camera by camera_id (status, codec, node, group, last seen). Use after list_cameras to inspect one closely. Returns text only — for the actual image, use view_camera.";
const DESC_GET_CAMERA_RECORDING_POLICY: &str = "Return the recording policy for a specific camera: whether 24/7 continuous recording is on, whether scheduled recording is on, and the scheduled start/end times (HH:MM, interpreted in the org's configured timezone — NOT UTC). Per-camera since v0.1.43 — replaces the previous org-level get_recording_settings. Use when the user asks 'is the garage cam recording right now?' or before filing an incident if it's relevant whether the moment was being recorded to disk on the CameraNode.";
const DESC_GET_INCIDENT: &str = "Get the full detail of a single incident: summary, full markdown report, all observations, and all evidence metadata (including evidence ids you can pass to get_incident_snapshot to see the attached images). Use this to read back a past report in full.";
const DESC_GET_INCIDENT_CLIP: &str = "Look up metadata about a video clip previously attached to an incident with attach_clip. Returns size, approximate duration, MIME, and the camera it came from. Note: this returns metadata only — the agent can't watch video, but a human reviewer can play the clip from the dashboard. Use this to confirm a clip was saved correctly.";
const DESC_GET_INCIDENT_SNAPSHOT: &str = "Fetch a snapshot image that was previously attached to an incident as evidence. Returns the stored JPEG so you can actually SEE what was captured. Pair with get_incident to discover evidence ids.";
const DESC_GET_NODE: &str = "Get full detail for one CameraNode by node_id (hostname, IP, port, status, camera count). Use after list_nodes when you need detail on one specific box — e.g. to confirm which physical device the user should power-cycle.";
const DESC_GET_STREAM_LOGS: &str = "Get recent stream-access log entries (one row per user × camera × ~5min window). Use to audit who watched a sensitive camera, check whether a user reviewed a feed during a time of interest, or investigate suspicious viewing activity. Filter by camera_id to scope to one feed.";
const DESC_GET_STREAM_STATS: &str = "Get aggregated stream-viewing stats over the last N days: totals, by-camera, and by-user. Use to find the most-watched cameras, build a usage summary, or establish a baseline before deciding whether a viewing pattern looks unusual. For per-event detail, use get_stream_logs.";
const DESC_GET_STREAM_URL: &str = "Return the authenticated HLS playlist URL for a camera. This is a URL a human or HLS player can open — YOU cannot watch video from it. Use only when you need to hand a stream URL back to the user. To see a frame yourself, use view_camera (single frame) or watch_camera (multi-frame burst).";
const DESC_GET_SYSTEM_STATUS: &str = "High-level snapshot of the org's Sentinel deployment: camera count with online/offline split, node count with online/offline split, and the active plan. Good first call to orient before drilling in. For per-camera detail, use list_cameras.";
const DESC_LIST_CAMERA_GROUPS: &str = "List the camera groups defined in the dashboard. A group is a user-defined zone (e.g. 'Front yard', 'Workshop') that bundles cameras together. Use when the user names a place and you need to find which cameras live there.";
const DESC_LIST_CAMERAS: &str = "List every camera in the organization with status, codec info, and group assignment. Start here when you don't yet know what cameras exist — most other camera tools take a camera_id from this output.";
const DESC_LIST_INCIDENTS: &str = "List incident reports for this organization, most recent first. Use this to check what incidents are already open before filing a duplicate, to follow up on past reports, or to look at activity patterns. Returns compact rows (id, title, severity, status, camera, timestamps, evidence count) without the full report body — call get_incident for the full detail of a specific one.";
const DESC_LIST_NODES: &str = "List every CameraNode (the physical box running cameras on the local network) for the org with status, hostname, and camera count. Use when troubleshooting at the box level — e.g. whether a whole node is offline vs whether one of its cameras is. For per-camera state, use list_cameras.";
const DESC_SET_CAMERA_RECORDING_POLICY: &str = "Set the recording policy for a specific camera. Any field omitted (or set to null) is left unchanged — pass only what you want to update. Use when the user asks 'turn on recording for the garage cam' or 'set scheduled recording on the front door cam from 18:00 to 06:00'. Times are HH:MM 24-hour, interpreted in the org's configured timezone (NOT UTC) — pass exactly what the user said, do not convert. Mutual-exclusion invariant: continuous_24_7 and scheduled_recording can't both be true; the call returns {error: 'modes_conflict'} if you try. Returns the new effective policy. Per-camera since v0.1.43.";
const DESC_UPDATE_INCIDENT: &str = "Edit fields on an existing incident. Use to escalate severity if the situation worsens, mark resolved/dismissed after confirming a false alarm, fix the short summary, or revise the long-form markdown report after new evidence. Pass only the fields you want to change — others are left alone. The report parameter REPLACES the existing body, so include the full revised text (the agent must already have it in context, e.g. from get_incident). For the very first report write, use finalize_incident instead.";
const DESC_VIEW_CAMERA: &str = "See what a camera sees RIGHT NOW — returns a single live JPEG you can actually look at. Use for a one-shot situational check ('is anyone in the workshop?'). For motion or change over time, use watch_camera instead. To preserve what you saw as evidence on an incident, follow up with attach_snapshot. The camera's node must be online.";
const DESC_WATCH_CAMERA: &str = "Take a burst of snapshots from one camera (count × interval_seconds wide). Use when a single view_camera frame isn't enough — to confirm whether a subject is moving, whether motion is sustained or fleeting, or whether something is returning to a scene. Each frame is a JPEG you can look at. The total window is short by design (max 10 frames × 30s); for longer evidence retention on an incident, use attach_clip.";

/// Every tool, with the description the picker shows. Sorted by name,
/// which is the order `/api/mcp/tools` returns within each category.
pub const TOOL_DESCRIPTIONS: [(&str, &str); 23] = [
    ("add_observation", DESC_ADD_OBSERVATION),
    ("attach_clip", DESC_ATTACH_CLIP),
    ("attach_snapshot", DESC_ATTACH_SNAPSHOT),
    ("create_incident", DESC_CREATE_INCIDENT),
    ("finalize_incident", DESC_FINALIZE_INCIDENT),
    ("get_camera", DESC_GET_CAMERA),
    ("get_camera_recording_policy", DESC_GET_CAMERA_RECORDING_POLICY),
    ("get_incident", DESC_GET_INCIDENT),
    ("get_incident_clip", DESC_GET_INCIDENT_CLIP),
    ("get_incident_snapshot", DESC_GET_INCIDENT_SNAPSHOT),
    ("get_node", DESC_GET_NODE),
    ("get_stream_logs", DESC_GET_STREAM_LOGS),
    ("get_stream_stats", DESC_GET_STREAM_STATS),
    ("get_stream_url", DESC_GET_STREAM_URL),
    ("get_system_status", DESC_GET_SYSTEM_STATUS),
    ("list_camera_groups", DESC_LIST_CAMERA_GROUPS),
    ("list_cameras", DESC_LIST_CAMERAS),
    ("list_incidents", DESC_LIST_INCIDENTS),
    ("list_nodes", DESC_LIST_NODES),
    ("set_camera_recording_policy", DESC_SET_CAMERA_RECORDING_POLICY),
    ("update_incident", DESC_UPDATE_INCIDENT),
    ("view_camera", DESC_VIEW_CAMERA),
    ("watch_camera", DESC_WATCH_CAMERA),
];

/// The description for one tool, or `""` for a name the server does
/// not serve — matching Python, where a name absent from the live
/// registry describes as empty rather than raising.
pub fn describe(name: &str) -> &'static str {
    TOOL_DESCRIPTIONS
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, description)| *description)
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_minute_window_refuses_the_call_past_its_limit() {
        let limiter = RateLimiter::new();
        let t = 1_000_000.0;
        for i in 0..3 {
            let (allowed, remaining, breach) = limiter.check_at("k", 3, 100, t);
            assert!(allowed, "call {i} should be allowed");
            assert_eq!(breach, Breach::None);
            assert_eq!(remaining, 2 - i);
        }
        let (allowed, remaining, breach) = limiter.check_at("k", 3, 100, t);
        assert!(!allowed);
        assert_eq!(remaining, 0);
        assert_eq!(breach, Breach::Minute);
    }

    /// The window slides rather than resetting: a call that has aged
    /// past sixty seconds stops counting, one at a time.
    #[test]
    fn the_minute_window_slides() {
        let limiter = RateLimiter::new();
        let t = 1_000_000.0;
        for _ in 0..3 {
            assert!(limiter.check_at("k", 3, 100, t).0);
        }
        assert!(!limiter.check_at("k", 3, 100, t + 59.9).0);
        // Strictly older than the cutoff drops out, so at exactly
        // sixty seconds the first call is still counted.
        assert!(!limiter.check_at("k", 3, 100, t + 60.0).0);
        assert!(limiter.check_at("k", 3, 100, t + 60.1).0);
    }

    /// The daily cap is reported separately, because it almost always
    /// means an agent stuck in a loop rather than a burst.
    #[test]
    fn the_daily_window_reports_its_own_breach() {
        let limiter = RateLimiter::new();
        let mut t = 1_000_000.0;
        // Spread out so the minute window never trips.
        for _ in 0..5 {
            assert!(limiter.check_at("k", 3, 5, t).0);
            t += 61.0;
        }
        let (allowed, _, breach) = limiter.check_at("k", 3, 5, t);
        assert!(!allowed);
        assert_eq!(breach, Breach::Daily);
        // And it clears a day later.
        assert!(limiter.check_at("k", 3, 5, t + 86_401.0).0);
    }

    /// A refused call is not recorded. Counting it would let a caller
    /// who keeps retrying hold their own window open forever.
    #[test]
    fn a_refused_call_does_not_extend_the_window() {
        let limiter = RateLimiter::new();
        let t = 1_000_000.0;
        assert!(limiter.check_at("k", 1, 100, t).0);
        for _ in 0..10 {
            assert!(!limiter.check_at("k", 1, 100, t + 30.0).0);
        }
        // Sixty seconds after the ONE recorded call, not after the
        // retries.
        assert!(limiter.check_at("k", 1, 100, t + 60.1).0);
    }

    #[test]
    fn keys_do_not_share_a_window() {
        let limiter = RateLimiter::new();
        let t = 1_000_000.0;
        assert!(limiter.check_at("a", 1, 100, t).0);
        assert!(!limiter.check_at("a", 1, 100, t).0);
        assert!(limiter.check_at("b", 1, 100, t).0);
    }

    /// A limit of zero admits nothing — an unrecognised plan is refused
    /// before it reaches here, but a zero must not read as unlimited.
    #[test]
    fn a_zero_limit_admits_nothing() {
        let limiter = RateLimiter::new();
        let (allowed, _, breach) = limiter.check_at("k", 0, 100, 1_000_000.0);
        assert!(!allowed);
        assert_eq!(breach, Breach::Minute);
    }

    /// The sweep is what stops every key that ever called from living
    /// in memory forever.
    #[test]
    fn keys_that_age_out_are_forgotten() {
        let t = 1_000_000.0;
        let limiter = RateLimiter::starting_at(t);
        assert!(limiter.check_at("old", 10, 100, t).0);
        limiter.with(|windows| assert!(windows.daily.contains_key("old")));
        // An hour past the prune interval AND a day past the call.
        assert!(limiter.check_at("new", 10, 100, t + 90_000.0).0);
        limiter.with(|windows| {
            assert!(!windows.daily.contains_key("old"), "aged-out key was kept");
            assert!(!windows.minute.contains_key("old"));
            assert!(windows.daily.contains_key("new"));
        });
    }

    fn owned(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn the_catalog_is_sixteen_reads_and_seven_writes() {
        assert_eq!(MCP_READ_TOOLS.len(), 16);
        assert_eq!(MCP_WRITE_TOOLS.len(), 7);
        assert_eq!(all_tools().len(), 23);
        // Disjoint: a tool is a read or a write, never both.
        for write in MCP_WRITE_TOOLS {
            assert!(!MCP_READ_TOOLS.contains(&write), "{write} is in both sets");
        }
    }

    #[test]
    fn scope_modes_resolve_the_way_python_resolves_them() {
        assert_eq!(compute_allowed_tools(None, None), all_tools());
        assert_eq!(compute_allowed_tools(Some("all"), None), all_tools());
        // An unknown mode is full access, matching the fall-through.
        assert_eq!(compute_allowed_tools(Some("nonsense"), None), all_tools());
        assert_eq!(compute_allowed_tools(Some("readonly"), None), read_tools());
        // Case-insensitive.
        assert_eq!(compute_allowed_tools(Some("READONLY"), None), read_tools());
    }

    #[test]
    fn a_custom_scope_drops_names_the_server_does_not_serve() {
        let picked = owned(&["list_cameras", "get_camera", "rm_minus_rf", ""]);
        let allowed = compute_allowed_tools(Some("custom"), Some(&picked));
        assert_eq!(
            allowed,
            ["list_cameras", "get_camera"].into_iter().collect::<BTreeSet<_>>()
        );
    }

    /// Custom with nothing listed is NO tools. Falling through to full
    /// access here would turn a half-finished scope picker into an
    /// unrestricted key.
    #[test]
    fn a_custom_scope_with_no_tools_allows_nothing() {
        assert!(compute_allowed_tools(Some("custom"), None).is_empty());
        assert!(compute_allowed_tools(Some("custom"), Some(&[])).is_empty());
    }

    /// The reason the computation takes parameters: a write tool added
    /// later must not become reachable by the agent just by existing.
    #[test]
    fn a_future_write_tool_is_not_reachable_by_the_agent() {
        let mut all = all_tools();
        all.insert("delete_everything");
        let allowed = compute_agent_allowed_tools(
            &all,
            &read_tools(),
            &AGENT_WRITE_TOOLS.iter().copied().collect(),
        );
        assert!(!allowed.contains("delete_everything"));
        // And the one write tool that exists today and is excluded.
        assert!(!allowed.contains("set_camera_recording_policy"));
        assert!(allowed.contains("create_incident"));
        assert!(allowed.contains("list_cameras"));
    }

    /// A name in the allowlist that the server does not serve grants
    /// nothing — the intersection is what stops a rename leaving a
    /// phantom permission behind.
    #[test]
    fn an_allowlisted_tool_the_server_lacks_grants_nothing() {
        let allowed = compute_agent_allowed_tools(
            &all_tools(),
            &read_tools(),
            &["create_incident", "renamed_away"].into_iter().collect(),
        );
        assert!(!allowed.contains("renamed_away"));
        // The reads, plus the ONE allowlisted write the server serves.
        assert_eq!(allowed.len(), read_tools().len() + 1);
    }

    #[test]
    fn mcp_is_a_paid_feature_and_an_unknown_plan_gets_nothing() {
        assert_eq!(rate_limits("pro"), Some((30, 5_000)));
        assert_eq!(rate_limits("pro_plus"), Some((120, 30_000)));
        assert_eq!(rate_limits("self_host"), Some((120, 30_000)));
        assert_eq!(rate_limits("free_org"), None);
        assert_eq!(rate_limits(""), None);
        assert_eq!(rate_limits("enterprise"), None);
    }

    #[test]
    fn argument_summaries_drop_nulls_and_cut_at_thirty_characters() {
        use serde_json::json;
        let args = json!({
            "camera_id": "cam-1",
            "absent": null,
            "long": "x".repeat(40),
            "count": 5,
            "flag": true,
        });
        let summary = summarize_args(args.as_object().unwrap());
        // Keys come out in the map's order, which for a JSON object
        // Python preserves as insertion order.
        assert!(summary.contains("camera_id=cam-1"));
        assert!(!summary.contains("absent"), "None values are dropped");
        assert!(summary.contains(&format!("long={}...", "x".repeat(27))));
        // Python's str(), so a bool is "True" and not "true".
        assert!(summary.contains("flag=True"));
        assert!(summary.contains("count=5"));
        // Exactly 30 characters is NOT truncated.
        let exact = json!({ "v": "y".repeat(30) });
        let summary = summarize_args(exact.as_object().unwrap());
        assert_eq!(summary, format!("v={}", "y".repeat(30)));
    }

    #[test]
    fn an_empty_argument_map_summarises_to_an_empty_string() {
        assert_eq!(summarize_args(&serde_json::Map::new()), "");
        let all_null = serde_json::json!({ "a": null, "b": null });
        assert_eq!(summarize_args(all_null.as_object().unwrap()), "");
    }
}
