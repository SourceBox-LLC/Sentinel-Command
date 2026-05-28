import hashlib
import logging
import re
import threading
import time
from datetime import UTC, datetime

from fastapi import APIRouter, Depends, HTTPException, Request
from fastapi.responses import Response
from sqlalchemy.orm import Session

from app.core.auth import get_current_user
from app.core.config import settings
from app.core.database import SessionLocal, get_db
from app.core.limiter import limiter
from app.models import Camera, CameraNode, StreamAccessLog
from app.models.models import OrgMonthlyUsage, Setting

router = APIRouter(prefix="/api/cameras/{camera_id}", tags=["streaming"])
logger = logging.getLogger(__name__)

# Pre-compiled regex patterns — avoids recompilation on every request.
#
# _RE_SEGMENT_URI matches any non-comment playlist line whose URI ends in
# ``segment_NNNNN.ts``, with or without a leading path.  FFmpeg's HLS
# muxer sometimes writes bare basenames into the playlist (when the
# ``-hls_segment_filename`` argument and the playlist are in the same
# dir) and sometimes writes the path verbatim (when they aren't, or on
# older FFmpeg versions).  Matching both forms and dropping the prefix
# means the browser always gets a relative ``segment/<file>`` URI
# regardless of how FFmpeg decided to format the line.  The negative
# lookahead skips ``#EXTINF`` and similar tag lines.  Trailing
# whitespace (``\r`` on CRLF playlists from Windows CloudNodes) is
# tolerated so the URI is emitted cleanly.
_RE_SEGMENT_URI = re.compile(
    r"^(?!#)(?:.*[/\\])?(segment_\d+\.ts)[ \t\r]*$",
    re.MULTILINE,
)
_RE_CODECS = re.compile(r"^#EXT-X-CODECS:.*$", re.MULTILINE)
_RE_SEGMENT_FILENAME = re.compile(r"^segment_\d+\.ts$")


async def _read_capped_body(request: Request, max_bytes: int) -> bytes:
    """Read the request body with an upper-bound size enforcement.

    Two layers of protection:
      1. **Pre-read** check on the ``Content-Length`` header.  An
         honest client (CloudNode pushes via reqwest, which always
         sets Content-Length on non-chunked bodies) gets rejected
         with HTTP 413 BEFORE any bytes land in memory.  This is
         the lever that makes a 10 GB attempted upload cost zero
         memory at the server.
      2. **Post-read** check on the actual body length.  Belt-and-
         suspenders for chunked-transfer requests that omit
         Content-Length, or for clients that lie.  Bytes are read
         (Starlette has no streaming-cap primitive available here)
         but the cap still fires before the body is forwarded into
         the cache.

    Returns the validated body bytes.  Raises HTTPException(413) on
    either path; HTTPException(400) if Content-Length is malformed.
    """
    declared = request.headers.get("content-length")
    if declared is not None:
        try:
            declared_int = int(declared)
        except ValueError:
            raise HTTPException(
                status_code=400, detail="Invalid Content-Length header"
            ) from None
        if declared_int > max_bytes:
            raise HTTPException(
                status_code=413,
                detail=(
                    f"Body declared {declared_int} bytes; max is {max_bytes}"
                ),
            )

    body = await request.body()
    if len(body) > max_bytes:
        raise HTTPException(
            status_code=413,
            detail=f"Body is {len(body)} bytes; max is {max_bytes}",
        )
    return body

# ── Rewritten playlist cache ──────────────────────────────────────────
# Populated by POST /playlist (CloudNode push). Browser GET requests
# serve the cached string instantly — no I/O per poll.
#
# {camera_id: (rewritten_playlist_text, timestamp)}
_playlist_cache: dict[str, tuple[str, float]] = {}
# 30 seconds.  With 1s segments and hls_list_size=15 the real segment window
# is ~15s, so we want the cache TTL comfortably larger than the gap between
# CloudNode playlist pushes — otherwise one or two dropped pushes expires
# the cache and the browser gets 404 "Stream not started yet" even though
# fresh segments are still being uploaded.  Segment cache eviction runs on
# its own 60s inactivity cutoff so stale-ref risk is bounded.
_PLAYLIST_CACHE_MAX_AGE = 30.0
_CACHE_MAX_CAMERAS = 500

# ── In-memory segment cache ──────────────────────────────────────────
# CloudNode pushes segments via POST /push-segment. Browser fetches
# them via GET /segment/{filename}.
#
# {camera_id: {filename: (bytes_data, monotonic_timestamp)}}
#
# **Multi-tenant safety**: keyed by `camera_id` alone (not by
# `(org_id, camera_id)`).  This is safe because `Camera.camera_id`
# has a DB-level `unique=True` constraint (`models.py:25`) — no two
# cameras across the entire system can share an id, so the keyspace
# is implicitly org-namespaced.  Every read path also performs an
# org-scoped Camera lookup before touching the cache (e.g.
# `attach_clip` at `mcp/server.py:_resolve_via_agent_key` →
# `Camera.filter_by(org_id=, camera_id=)`), so even if uniqueness
# were ever weakened to a per-org scope, an attacker would have to
# create a Camera row with the colliding id in their own org first
# (which is a separate org-scoped insert path that we do gate).
_segment_cache: dict[str, dict[str, tuple[bytes, float]]] = {}

# Guards every iteration-or-mutation of `_segment_cache`.
#
# Why a lock when the HTTP handlers are async (and therefore serialized
# on the event loop)?  Because `attach_clip` (an MCP `@mcp.tool`) is a
# *sync* function — FastMCP runs sync tools in an AnyIO worker thread,
# off the event loop.  That worker can be mid-`sorted(cam_cache.keys())`
# while the event loop's push→evict path is doing `del cam_cache[k]`.
# CPython raises `RuntimeError: dictionary changed size during
# iteration` when a dict is mutated while another thread iterates it —
# the GIL makes single `.get()`/`.pop()` atomic but NOT a multi-bytecode
# iteration.  The race window opens exactly when the cache is full
# (eviction active) and an agent grabs a clip — i.e. under load.
#
# RLock (not Lock) because the push path acquires the lock and then
# calls `_evict_segment_cache` / `_evict_global_oldest`, which acquire
# it again on the same thread.  Every critical section is pure dict
# work (microseconds) and never spans an `await` or I/O, so holding it
# on the event-loop thread can't stall the loop meaningfully.
_segment_cache_lock = threading.RLock()

# Running total of bytes held in `_segment_cache`, maintained
# incrementally at every insert/delete site (all of which hold
# `_segment_cache_lock`).  The global byte-cap check on the push hot
# path reads this in O(1) instead of re-walking all ~30K cached
# segments (500 cameras × 60) on every one of up to 1200 pushes/min
# per camera — the previous `sum(len(body) for ...)` recompute.
# `_recompute_segment_cache_bytes()` is the authoritative re-derivation,
# kept for the reconciliation assert in tests; the hot path trusts the
# counter.
_segment_cache_byte_total: int = 0

# Track playlist update count per camera — used to throttle cache eviction sweeps.
_playlist_update_count: dict[str, int] = {}

# ── One-shot diagnostic logging ───────────────────────────────────────
# On a fresh backend, every new camera id logs its first playlist push
# and first stream.m3u8 fetch exactly once.  This gives an operator
# ground truth about what the pipeline is doing ("did CloudNode reach
# /playlist at all?  what's the first segment URI look like?") without
# drowning Fly logs in per-segment spam.  Cleared when the camera is
# evicted from the cache so a reconnect relogs.
_first_playlist_logged: set[str] = set()
_first_stream_get_logged: set[str] = set()

# ── Stream access logging (rate-limited) ─────────────────────────────
_ACCESS_LOG_INTERVAL = 300.0  # 5 minutes
_last_access_logged: dict[tuple[str, str], float] = {}
_ACCESS_LOG_MAX_ENTRIES = 10000

# ── Viewer-hour usage tracking ──────────────────────────────────────
# Each cached HLS segment we serve is ~1 second of video, so we increment a
# per-org counter by 1 for every successful segment delivery. The hot path
# touches only an in-memory dict protected by a lock; a periodic flush task
# (see ``flush_viewer_usage``) is what actually writes to the DB, so we
# never pay SQLite latency on every segment request.
#
# The cached DB total is also held in memory so cap-enforcement reads are
# O(1) instead of a per-request SELECT; the flush task keeps it fresh.
_viewer_usage_lock = threading.Lock()
_pending_viewer_seconds: dict[tuple[str, str], int] = {}  # (org_id, ym) → pending
_cached_viewer_seconds: dict[tuple[str, str], int] = {}   # (org_id, ym) → DB total


def _current_year_month() -> str:
    """UTC-month bucket key — ``YYYY-MM``. Caches get reset when a new
    month starts simply by the key changing; nothing to evict explicitly."""
    return datetime.now(tz=UTC).strftime("%Y-%m")


def record_viewer_second(org_id: str) -> None:
    """Increment the in-memory viewer-second counter for an org. O(1) under
    the lock — safe to call on every successful segment serve."""
    key = (org_id, _current_year_month())
    with _viewer_usage_lock:
        _pending_viewer_seconds[key] = _pending_viewer_seconds.get(key, 0) + 1


def get_viewer_seconds_used(org_id: str) -> int:
    """Return the current month's total viewer-seconds for an org
    (cached DB total + pending in-memory delta).

    Used by the cap-enforcement check in the segment route and by the
    ``/api/nodes/plan`` response so the dashboard can show live usage.

    Delegates to ``_warm_cached_viewer_seconds`` so a cold cache (e.g.
    right after a deploy / Fly machine restart) lazily reads the real
    DB total instead of returning 0.  Without that warm step the
    dashboard would show "0 hours used" until the operator clicked a
    camera (which triggers the segment-serve hot path that does its
    own warm), looking exactly like the counter had been reset.
    """
    return _warm_cached_viewer_seconds(org_id)


def _warm_cached_viewer_seconds(org_id: str) -> int:
    """Populate the cache for ``org_id`` from the DB if we don't have it
    yet, then return the authoritative total. Called on the first segment
    of the month for each org so the cap check has real data."""
    ym = _current_year_month()
    key = (org_id, ym)
    with _viewer_usage_lock:
        if key in _cached_viewer_seconds:
            return _cached_viewer_seconds[key] + _pending_viewer_seconds.get(key, 0)

    db = SessionLocal()
    try:
        row = (
            db.query(OrgMonthlyUsage)
            .filter_by(org_id=org_id, year_month=ym)
            .first()
        )
        seconds = int(row.viewer_seconds) if row else 0
    except Exception:
        logger.exception("[ViewerUsage] Failed to warm cache for %s", org_id)
        seconds = 0
    finally:
        db.close()

    with _viewer_usage_lock:
        _cached_viewer_seconds[key] = seconds
        return seconds + _pending_viewer_seconds.get(key, 0)


def flush_viewer_usage() -> int:
    """Flush pending in-memory viewer-seconds to the DB with UPSERTs. Called
    by a background task every ~60s. Returns the number of (org, month)
    rows touched so the caller can log activity.

    Stale pending counts from previous months are flushed too so the
    caller's cross-month accounting is accurate — the key is
    ``(org_id, year_month)`` so a segment served at 23:59:59 on the last
    day of a month increments that month's row, not the next one.
    """
    with _viewer_usage_lock:
        if not _pending_viewer_seconds:
            return 0
        snapshot = dict(_pending_viewer_seconds)
        _pending_viewer_seconds.clear()

    db = SessionLocal()
    try:
        for (org_id, ym), delta in snapshot.items():
            if delta <= 0:
                continue
            row = (
                db.query(OrgMonthlyUsage)
                .filter_by(org_id=org_id, year_month=ym)
                .first()
            )
            if row:
                row.viewer_seconds = int(row.viewer_seconds or 0) + delta
            else:
                row = OrgMonthlyUsage(
                    org_id=org_id,
                    year_month=ym,
                    viewer_seconds=delta,
                )
                db.add(row)
            # Update the in-memory cache so the next read sees the new DB total.
            with _viewer_usage_lock:
                _cached_viewer_seconds[(org_id, ym)] = int(row.viewer_seconds)
        db.commit()
        return len(snapshot)
    except Exception:
        logger.exception("[ViewerUsage] Flush failed — pending increments lost")
        # Pending increments are already cleared; a cross-flush window
        # where a handful of segments go uncounted is an acceptable loss
        # compared to accumulating indefinitely through a DB outage.
        db.rollback()
        return 0
    finally:
        db.close()


# ── Cache management ─────────────────────────────────────────────────


def cleanup_camera_cache(camera_id: str):
    """Remove all cached segments and playlist for a camera.
    Called when a camera or node is deleted."""
    global _segment_cache_byte_total
    with _segment_cache_lock:
        removed = _segment_cache.pop(camera_id, None)
        if removed:
            _segment_cache_byte_total -= sum(len(body) for body, _ts in removed.values())
    _playlist_cache.pop(camera_id, None)
    _playlist_update_count.pop(camera_id, None)
    _first_playlist_logged.discard(camera_id)
    _first_stream_get_logged.discard(camera_id)


def _evict_segment_cache(camera_id: str):
    """Keep only the newest SEGMENT_CACHE_MAX_PER_CAMERA segments for a camera."""
    global _segment_cache_byte_total
    with _segment_cache_lock:
        cam_cache = _segment_cache.get(camera_id)
        if not cam_cache or len(cam_cache) <= settings.SEGMENT_CACHE_MAX_PER_CAMERA:
            return
        # Sort by filename (monotonically increasing sequence numbers)
        sorted_keys = sorted(cam_cache.keys())
        to_remove = sorted_keys[: len(sorted_keys) - settings.SEGMENT_CACHE_MAX_PER_CAMERA]
        for key in to_remove:
            body, _ts = cam_cache.pop(key)
            _segment_cache_byte_total -= len(body)


def _segment_cache_total_bytes() -> int:
    """O(1) read of the running byte total held in `_segment_cache`.

    Reads the incrementally-maintained `_segment_cache_byte_total`
    counter instead of re-walking every cached segment.  This is the
    value the global byte-cap check reads on every push, so O(1) here
    is the difference between the cap check being free vs. a ~30K-entry
    walk per push at scale.  `_recompute_segment_cache_bytes()` is the
    authoritative re-derivation if you ever need to distrust the
    counter.
    """
    with _segment_cache_lock:
        return _segment_cache_byte_total


def _recompute_segment_cache_bytes() -> int:
    """Authoritative O(N) re-derivation of the cache byte total by
    walking every cached segment.

    NOT on the hot path — used by the periodic stale-camera sweep to
    reconcile the running counter against ground truth (cheap drift
    insurance), and as the oracle in the counter-correctness test.
    Holds the lock so a worker-thread reader can't mutate a per-camera
    dict underneath the nested `.values()` iteration.
    """
    with _segment_cache_lock:
        return sum(
            len(body)
            for cam_cache in _segment_cache.values()
            for body, _ts in cam_cache.values()
        )


def _evict_global_oldest(max_total_bytes: int) -> int:
    """Drop the oldest segments globally until the total fits the cap.

    Called after each push.  The fast exit (total <= cap) is an O(1)
    counter read; the O(N) candidate-build + sort only runs on the rare
    push that actually pushes us over the global ceiling.  Returns the
    number of segments evicted (mostly for tests / observability).

    Policy choice: prefer freshness.  When the cap is hit, the live
    edge of every active camera is more valuable than the tail of
    any single camera — so global oldest-first beats per-camera
    fairness.  A camera that hasn't received a segment in a while is
    the natural sacrifice; an active stream's recent segments stay.

    Note on overshoot: because the push path holds `_segment_cache_lock`
    across insert+evict, there's no multi-push overshoot — the only
    transient over-cap state is the single segment just inserted, which
    this call immediately trims.  (Pre-lock, N concurrent pushes could
    each insert before any evicted, overshooting by N segments.)
    """
    global _segment_cache_byte_total
    with _segment_cache_lock:
        if _segment_cache_byte_total <= max_total_bytes:
            return 0

        # Build a flat list of (ts, camera_id, filename, byte_size).
        # Snapshot the keys we're about to mutate so we don't iterate
        # the dict while modifying it (CPython would raise RuntimeError).
        candidates: list[tuple[float, str, str, int]] = []
        for cam_id, cam_cache in _segment_cache.items():
            for fname, (body, ts) in cam_cache.items():
                candidates.append((ts, cam_id, fname, len(body)))
        candidates.sort()

        evicted = 0
        for _ts, cam_id, fname, size in candidates:
            if _segment_cache_byte_total <= max_total_bytes:
                break
            cam_cache = _segment_cache.get(cam_id)
            if cam_cache is None:
                continue
            if cam_cache.pop(fname, None) is not None:
                _segment_cache_byte_total -= size
                evicted += 1
            # If the camera's cache is now empty, drop the empty bucket
            # too so _segment_cache size stays accurate for monitoring.
            if cam_cache is not None and not cam_cache:
                _segment_cache.pop(cam_id, None)
        total_now = _segment_cache_byte_total

    if evicted:
        logger.warning(
            "[HLS] Global cache cap hit — evicted %d oldest segments "
            "(total now %d bytes, cap %d bytes)",
            evicted, total_now, max_total_bytes,
        )
    return evicted


def _evict_stale_cameras():
    """Remove segment caches for cameras that haven't received data recently."""
    global _segment_cache_byte_total
    cutoff = time.monotonic() - 60.0  # 1 minute
    with _segment_cache_lock:
        stale = []
        for camera_id, segments in _segment_cache.items():
            if not segments:
                stale.append(camera_id)
                continue
            newest_ts = max(ts for _, ts in segments.values())
            if newest_ts < cutoff:
                stale.append(camera_id)
        for camera_id in stale:
            del _segment_cache[camera_id]
        # Reconcile the running byte counter against ground truth on this
        # background sweep.  Drift insurance: if any mutation site ever
        # forgets to adjust the counter, this 60s-cadence recompute
        # self-heals it.  O(N) is fine here — it's off the push hot path.
        _segment_cache_byte_total = _recompute_segment_cache_bytes()
    # Sibling caches aren't iterated cross-thread (event-loop only), so
    # they don't need the segment lock — clear them outside the critical
    # section to keep it tight.
    for camera_id in stale:
        _playlist_cache.pop(camera_id, None)
        _playlist_update_count.pop(camera_id, None)
        _first_playlist_logged.discard(camera_id)
        _first_stream_get_logged.discard(camera_id)


def snapshot_recent_segment_bytes(camera_id: str, count: int) -> list[bytes] | None:
    """Atomically snapshot the newest `count` cached segments' bytes for a
    camera, oldest-first.

    The single safe entry point for an off-event-loop reader (currently
    `attach_clip`, which runs in a FastMCP worker thread) to pull from
    `_segment_cache`.  Holds `_segment_cache_lock` across both the
    `sorted(keys())` snapshot and the per-segment byte reads, so the
    event loop's eviction path can't mutate the per-camera dict mid-read
    (the `RuntimeError: dictionary changed size during iteration` this
    whole lock exists to prevent).

    Returns:
      - ``None``  if the camera has no cache bucket at all (stream never
                  went live, or its bucket was already fully evicted) —
                  caller surfaces "stream must be live".
      - ``[]``    if the bucket existed but yielded no readable segments
                  (raced an eviction that emptied it) — caller surfaces
                  "try again".
      - ``list``  the selected segments' bytes, oldest-first.

    Bytes objects are immutable, so the caller may use the returned list
    freely after the lock releases.
    """
    with _segment_cache_lock:
        cam_cache = _segment_cache.get(camera_id)
        if not cam_cache:
            return None
        if count <= 0:
            return []
        selected = sorted(cam_cache.keys())[-count:]
        out: list[bytes] = []
        for fname in selected:
            entry = cam_cache.get(fname)
            if entry:
                out.append(entry[0])
        return out


def _evict_caches():
    """Evict stale entries from module-level caches to prevent unbounded growth."""
    now = time.monotonic()

    if len(_playlist_cache) > _CACHE_MAX_CAMERAS:
        sorted_entries = sorted(_playlist_cache.items(), key=lambda x: x[1][1])
        for camera_id, _ in sorted_entries[: len(sorted_entries) - _CACHE_MAX_CAMERAS]:
            del _playlist_cache[camera_id]
            _playlist_update_count.pop(camera_id, None)

    if len(_last_access_logged) > _ACCESS_LOG_MAX_ENTRIES:
        cutoff = now - (_ACCESS_LOG_INTERVAL * 2)
        stale_keys = [k for k, ts in _last_access_logged.items() if ts < cutoff]
        for k in stale_keys:
            del _last_access_logged[k]

    _evict_stale_cameras()


def _maybe_log_access(
    db: Session,
    user_id: str,
    user_email: str,
    org_id: str,
    camera_id: str,
    node_id: str,
    ip_address: str,
    user_agent: str,
) -> None:
    """Create a StreamAccessLog entry if enough time has passed."""
    now = time.monotonic()
    key = (user_id, camera_id)
    last = _last_access_logged.get(key, 0.0)
    if now - last < _ACCESS_LOG_INTERVAL:
        return

    _last_access_logged[key] = now

    try:
        from datetime import datetime

        log_entry = StreamAccessLog(
            user_id=user_id,
            user_email=user_email,
            org_id=org_id,
            camera_id=camera_id,
            node_id=node_id,
            ip_address=ip_address,
            user_agent=user_agent,
            accessed_at=datetime.now(tz=UTC).replace(tzinfo=None),
        )
        db.add(log_entry)
        db.commit()
    except Exception as e:
        logger.warning("Failed to log stream access: %s", e)
        db.rollback()


def _rewrite_playlist(raw_playlist: str) -> str:
    """
    Rewrite raw HLS playlist: replace segment URIs with relative proxy
    URLs (``segment/<filename>``) and remove invalid ``#EXT-X-CODECS``
    lines.  Pure string manipulation — no I/O.

    The incoming URI can be either a bare basename or a path-prefixed
    name (see ``_RE_SEGMENT_URI``).  We strip any prefix and emit the
    canonical ``segment/<basename>`` so the browser always resolves to
    ``/api/cameras/{id}/segment/<basename>`` — the endpoint backed by
    the in-memory cache.

    ``#EXT-X-CODECS`` is only valid in Master Playlists; injecting it
    into a Media Playlist causes hls.js to attempt master-playlist
    parsing and never fire ``MANIFEST_PARSED``, leaving the player
    stuck at "Connecting…".
    """
    # Normalize segment URIs to relative proxy paths.  The capture
    # group is the basename only — prefix is discarded.
    playlist_text = _RE_SEGMENT_URI.sub(r"segment/\1", raw_playlist)

    # Remove any existing CODECS line.  #EXT-X-CODECS is only valid in
    # Master Playlists — injecting it into a Media Playlist causes
    # hls.js to attempt master-playlist parsing and fail to fire
    # MANIFEST_PARSED, locking the player at "Connecting…".
    playlist_text = _RE_CODECS.sub("", playlist_text)

    return playlist_text


# ── Endpoints ─────────────────────────────────────────────────────────


@router.get("/stream.m3u8")
async def get_hls_playlist(
    request: Request,
    camera_id: str,
    user=Depends(get_current_user),
    db: Session = Depends(get_db),
):
    """
    Get HLS playlist for a camera stream.
    Served from the in-memory cache populated by POST /playlist.
    """
    camera = db.query(Camera).filter_by(camera_id=camera_id, org_id=user.org_id).first()
    if not camera:
        raise HTTPException(status_code=404, detail="Camera not found")

    node = db.query(CameraNode).filter_by(id=camera.node_id, org_id=user.org_id).first()
    if not node:
        raise HTTPException(status_code=404, detail="Camera node not found")

    _maybe_log_access(
        db=db,
        user_id=user.user_id,
        user_email=user.email,
        org_id=user.org_id,
        camera_id=camera_id,
        node_id=str(node.id),
        ip_address=request.client.host if request.client else "unknown",
        user_agent=request.headers.get("user-agent", "")[:500],
    )

    headers = {
        "Cache-Control": "no-store, no-cache, must-revalidate, max-age=0",
        "Pragma": "no-cache",
    }

    # Serve from cache (populated by POST /playlist from CloudNode).
    cached = _playlist_cache.get(camera_id)
    if cached and (time.monotonic() - cached[1]) < _PLAYLIST_CACHE_MAX_AGE:
        if camera_id not in _first_stream_get_logged:
            _first_stream_get_logged.add(camera_id)
            logger.info(
                "hls: first stream.m3u8 HIT for cam=%s (playlist_age=%.1fs, bytes=%d, cached_segments=%d)",
                camera_id,
                time.monotonic() - cached[1],
                len(cached[0]),
                len(_segment_cache.get(camera_id, {})),
            )
        return Response(
            content=cached[0],
            media_type="application/vnd.apple.mpegurl",
            headers=headers,
        )

    # No cached playlist — CloudNode hasn't pushed one yet (or it went
    # stale).  Log once per camera so operators can tell the "CloudNode
    # isn't pushing playlists" case apart from "stream.m3u8 never called".
    # With hls.js retrying every 400ms, unmuted INFO would be a flood —
    # the one-shot flag keeps it to one line per camera per restart.
    if camera_id not in _first_stream_get_logged:
        _first_stream_get_logged.add(camera_id)
        cached_bytes = len(_segment_cache.get(camera_id, {}))
        logger.warning(
            "hls: first stream.m3u8 MISS for cam=%s (playlist_cached=%s, segment_cache_entries=%d) — "
            "CloudNode hasn't POST /playlist for this camera yet",
            camera_id,
            cached is not None,
            cached_bytes,
        )
    raise HTTPException(status_code=404, detail="Stream not started yet")


@router.get("/segment/{filename}")
async def get_hls_segment(
    request: Request,
    camera_id: str,
    filename: str,
    user=Depends(get_current_user),
    db: Session = Depends(get_db),
):
    """Serve an HLS segment from the in-memory cache.

    Also enforces the monthly viewer-hours cap for the org's plan. Each
    served segment is ~1 second of video, so we check the running counter
    against ``PLAN_LIMITS[plan]["max_viewer_hours_per_month"]`` before
    returning bytes. The cap-enforcement read hits the in-memory counter,
    not the DB, so it's O(1) on the hot path.
    """
    camera = db.query(Camera).filter_by(camera_id=camera_id, org_id=user.org_id).first()
    if not camera:
        raise HTTPException(status_code=404, detail="Camera not found")

    if not _RE_SEGMENT_FILENAME.match(filename):
        raise HTTPException(status_code=400, detail="Invalid segment filename")

    # Check the monthly viewer-hour cap before serving. Warm the cache on the
    # first segment we see for this org (same call amortizes one DB read per
    # org per process lifetime).
    #
    # Use ``effective_plan_for_caps`` instead of ``user.plan`` (the JWT claim)
    # so a stale token can't keep buying paid-tier viewer-hours after the
    # 7-day grace window expires — the JWT only refreshes once a minute, and
    # a Clerk webhook propagation hiccup can delay it further. Reading the
    # DB-resolved plan here keeps this enforcement point consistent with the
    # push-segment / camera-cap path, which already uses the effective plan.
    from app.core.plans import effective_plan_for_caps, get_plan_limits
    effective_plan = effective_plan_for_caps(db, user.org_id)
    limits = get_plan_limits(effective_plan)
    max_hours = limits.get("max_viewer_hours_per_month")
    if max_hours is not None and max_hours > 0:
        used_seconds = _warm_cached_viewer_seconds(user.org_id)
        if used_seconds >= max_hours * 3600:
            raise HTTPException(
                status_code=429,
                detail=(
                    f"Monthly viewer-hour cap reached ({max_hours}h on your "
                    f"current plan). Live playback will resume on the 1st of "
                    f"next month, or upgrade your plan for more viewing time."
                ),
                headers={"Retry-After": "3600"},
            )

    # Snapshot the bytes under the lock so the worker-thread reader vs
    # event-loop eviction race can't surface here either.  `.get()` is
    # individually GIL-atomic, but taking the lock keeps every cache
    # access on the same discipline and the bytes ref we pull out is
    # immutable, so it's safe to use after release.
    with _segment_cache_lock:
        cam_cache = _segment_cache.get(camera_id)
        entry = cam_cache.get(filename) if cam_cache else None
        body = entry[0] if entry else None

    if body is not None:
        # Count this segment against the org's monthly viewer-second budget.
        # Only count on successful serves — a 404 or cap-block never charges.
        record_viewer_second(user.org_id)
        return Response(
            content=body,
            media_type="video/mp2t",
            headers={"Cache-Control": "public, max-age=3600"},
        )

    raise HTTPException(status_code=404, detail="Segment not found")


@router.post("/push-segment")
@limiter.limit("1200/minute")
async def push_segment(
    request: Request,
    camera_id: str,
    filename: str,
    db: Session = Depends(get_db),
):
    """
    Receive an HLS segment pushed by CloudNode.
    Stores in memory for the browser to fetch via GET /segment/{filename}.
    """
    node_api_key = request.headers.get("X-Node-API-Key")
    if not node_api_key:
        raise HTTPException(status_code=401, detail="Missing API key")

    api_key_hash = hashlib.sha256(node_api_key.encode()).hexdigest()
    node = db.query(CameraNode).filter_by(api_key_hash=api_key_hash).first()
    if not node:
        raise HTTPException(status_code=401, detail="Invalid API key")

    # Enforce both node ownership AND org match — defense-in-depth so a
    # future schema drift can't let a node in org A touch a camera in org B.
    camera = (
        db.query(Camera)
        .filter_by(camera_id=camera_id, node_id=node.id, org_id=node.org_id)
        .first()
    )
    if not camera:
        raise HTTPException(status_code=404, detail="Camera not found")

    # Plan-cap enforcement. When the org is over its camera cap (downgrade
    # or cancellation), `enforce_camera_cap` has marked the over-cap
    # cameras as `disabled_by_plan`. Reject their uploads with HTTP 402
    # (Payment Required) and a `plan_limit_hit` body so the CloudNode can
    # surface the reason in its TUI instead of silently filling the log
    # with non-retryable push failures.
    if camera.disabled_by_plan:
        from app.core.plans import (
            get_plan_display_name,
            get_plan_limits_for_org,
        )
        limits = get_plan_limits_for_org(db, node.org_id)
        plan_name = get_plan_display_name(limits.get("_plan", "free_org"))
        raise HTTPException(
            status_code=402,
            detail={
                "message": "Camera suspended by plan limit",
                "plan_limit_hit": {
                    "plan": plan_name,
                    "max_cameras": limits["max_cameras"],
                    "skipped": [camera.name],
                    "detail": (
                        f"Camera '{camera.name}' is over the "
                        f"{plan_name} plan limit ({limits['max_cameras']} cameras). "
                        f"Upgrade to resume streaming."
                    ),
                },
            },
        )

    if not _RE_SEGMENT_FILENAME.match(filename):
        raise HTTPException(status_code=400, detail="Invalid segment filename")

    body = await _read_capped_body(request, settings.SEGMENT_PUSH_MAX_BYTES)

    # Cache the segment + run both eviction passes under the cache lock
    # so a worker-thread reader (attach_clip) never observes a partially
    # mutated cache.  The lock is an RLock, so the nested acquires inside
    # `_evict_segment_cache` / `_evict_global_oldest` are free.  `body`
    # was already `await`-read above, so nothing in this critical section
    # blocks on I/O — it's pure dict work, microseconds, safe to hold on
    # the event-loop thread.
    global _segment_cache_byte_total
    with _segment_cache_lock:
        cam_bucket = _segment_cache.setdefault(camera_id, {})
        # A re-push of an existing filename (same camera+seq pushed twice —
        # rare, but a flaky-network retry can do it) overwrites the entry,
        # so subtract the old size before adding the new one to keep the
        # running counter exact.
        prev = cam_bucket.get(filename)
        if prev is not None:
            _segment_cache_byte_total -= len(prev[0])
        cam_bucket[filename] = (body, time.monotonic())
        _segment_cache_byte_total += len(body)
        _evict_segment_cache(camera_id)
        # Global byte ceiling — bounds the SUM of all camera caches.
        # The fast path is now an O(1) counter comparison; the eviction
        # loop only walks the cache on the rare push that exceeds the
        # global cap.  See SEGMENT_CACHE_MAX_TOTAL_BYTES in config.py.
        _evict_global_oldest(settings.SEGMENT_CACHE_MAX_TOTAL_BYTES)
        cached_count = len(_segment_cache.get(camera_id, {}))

    return {"success": True, "cached_segments": cached_count}


@router.post("/playlist")
@limiter.limit("600/minute")
async def update_hls_playlist(
    request: Request,
    camera_id: str,
    db: Session = Depends(get_db),
):
    """
    Update the HLS playlist for a camera.
    Called by CloudNode when new segments are generated.
    Expects playlist content in request body (text/plain).

    Rewrites segment filenames to relative proxy URLs and caches the
    result so browser GET requests are served instantly.
    """
    node_api_key = request.headers.get("X-Node-API-Key")
    if not node_api_key:
        raise HTTPException(status_code=401, detail="Missing API key")

    api_key_hash = hashlib.sha256(node_api_key.encode()).hexdigest()
    node = db.query(CameraNode).filter_by(api_key_hash=api_key_hash).first()
    if not node:
        raise HTTPException(status_code=401, detail="Invalid API key")

    # Enforce both node ownership AND org match — defense-in-depth so a
    # future schema drift can't let a node in org A touch a camera in org B.
    camera = (
        db.query(Camera)
        .filter_by(camera_id=camera_id, node_id=node.id, org_id=node.org_id)
        .first()
    )
    if not camera:
        raise HTTPException(status_code=404, detail="Camera not found")

    body = await _read_capped_body(request, settings.PLAYLIST_PUSH_MAX_BYTES)
    try:
        playlist_content = body.decode("utf-8")
    except UnicodeDecodeError as e:
        raise HTTPException(
            status_code=400, detail=f"Invalid playlist content: {e}"
        ) from e

    # Pre-compute the rewritten playlist with proxy segment URLs
    # and cache it. Browser polls will serve this instantly.
    rewritten = _rewrite_playlist(playlist_content)
    _playlist_cache[camera_id] = (rewritten, time.monotonic())

    # First-push diagnostic log — capture the first raw segment URI so
    # we can see how FFmpeg is shaping it in production (bare basename
    # vs path-prefixed vs something else entirely).  Lives under INFO
    # so it shows up in Fly's default log view, one line per camera per
    # backend restart — not the kind of thing you want to see at 1 Hz.
    if camera_id not in _first_playlist_logged:
        _first_playlist_logged.add(camera_id)
        # Sample the first non-comment, non-blank line — that's the
        # first segment URI (ground truth for the rewriter).
        sample_uri = next(
            (
                ln.strip()
                for ln in playlist_content.splitlines()
                if ln.strip() and not ln.lstrip().startswith("#")
            ),
            "<none>",
        )
        logger.info(
            "hls: first playlist push cam=%s raw_bytes=%d rewritten_bytes=%d "
            "first_segment_uri=%r",
            camera_id,
            len(playlist_content),
            len(rewritten),
            sample_uri[:200],
        )

    # Periodic cache eviction.
    count = _playlist_update_count.get(camera_id, 0) + 1
    _playlist_update_count[camera_id] = count
    if count % settings.CLEANUP_INTERVAL == 0:
        _evict_caches()

    return {"success": True, "message": "Playlist updated"}


@router.post("/motion")
@limiter.limit("120/minute")
async def push_motion_event(
    request: Request,
    camera_id: str,
    db: Session = Depends(get_db),
):
    """
    Receive a motion detection event pushed by CloudNode via HTTP.
    This is a reliable fallback that works even when WebSocket is not connected.
    """
    node_api_key = request.headers.get("X-Node-API-Key")
    if not node_api_key:
        raise HTTPException(status_code=401, detail="Missing API key")

    api_key_hash = hashlib.sha256(node_api_key.encode()).hexdigest()
    node = db.query(CameraNode).filter_by(api_key_hash=api_key_hash).first()
    if not node:
        raise HTTPException(status_code=401, detail="Invalid API key")

    # Enforce both node ownership AND org match — defense-in-depth so a
    # future schema drift can't let a node in org A touch a camera in org B.
    camera = (
        db.query(Camera)
        .filter_by(camera_id=camera_id, node_id=node.id, org_id=node.org_id)
        .first()
    )
    if not camera:
        raise HTTPException(status_code=404, detail="Camera not found")

    # Per-org kill switch.  When an admin disables ingestion (e.g. a
    # misbehaving sensor is flooding events and you need a server-side
    # stop without reaching the node), short-circuit before recording
    # anything.  Returns 200 + ingested:false so the CloudNode treats
    # this as a successful "by design" rejection and doesn't burn its
    # retry budget — same behaviour as the plan-cap suspension path.
    # Default "true" so orgs that never touch the toggle keep the
    # original always-ingest behaviour.
    if Setting.get(db, node.org_id, "motion_ingestion_enabled", "true").lower() != "true":
        return {"success": True, "ingested": False, "reason": "ingestion_disabled"}

    body = await request.json()

    from app.api.ws import _handle_motion_event

    await _handle_motion_event(
        node.node_id,
        node.org_id,
        {
            "camera_id": camera_id,
            "score": body.get("score"),
            "segment_seq": body.get("segment_seq"),
            "timestamp": body.get("timestamp"),
        },
    )

    return {"success": True, "ingested": True}
