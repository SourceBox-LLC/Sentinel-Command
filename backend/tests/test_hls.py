"""
Tests for the live-video proxy path — the backend-cached HLS pipeline.

Covers:

- Segment roundtrip: a segment pushed by the owning CloudNode over
  ``POST /push-segment`` is byte-exact on a subsequent
  ``GET /segment/<filename>`` — the cache never corrupts the payload.
- Playlist rewriting: raw playlist text pushed by the CloudNode is
  served back with segment filenames proxied through this backend
  (``segment/segment_00001.ts``) — no absolute URLs leak through.
- Codec stripping: ``#EXT-X-CODECS`` lines are removed from media
  playlists since they're only valid in master playlists and break
  hls.js parsing.
- Cache eviction bounds: pushing more than
  ``SEGMENT_CACHE_MAX_PER_CAMERA`` segments drops the oldest.
- ``stream.m3u8`` returns 404 when no playlist has ever been pushed.
- ``cleanup_camera_cache`` removes both segments and playlist for a
  camera (used by delete/cleanup paths).
"""

import hashlib
import uuid

from app.models.models import Camera, CameraNode

# ── Helpers ───────────────────────────────────────────────────────────


def _seed_node_with_camera(db, *, org_id="org_test123"):
    """Create one node+camera and return ``(raw_api_key, camera_id)``.

    We seed the hash (not the raw key) on the node row, same as production
    — so ``X-Node-API-Key: <raw>`` authenticates against it.
    """
    raw_key = "raw-key-" + uuid.uuid4().hex
    node = CameraNode(
        node_id="node_hls_" + uuid.uuid4().hex[:8],
        org_id=org_id,
        api_key_hash=hashlib.sha256(raw_key.encode()).hexdigest(),
        name="HlsTestNode",
    )
    db.add(node)
    db.commit()
    db.refresh(node)
    cam_id = "cam_hls_" + uuid.uuid4().hex[:8]
    db.add(
        Camera(
            camera_id=cam_id,
            org_id=org_id,
            node_id=node.id,
            name="HlsTestCam",
            video_codec="avc1.42e01e",
            audio_codec="mp4a.40.2",
        )
    )
    db.commit()
    return raw_key, cam_id


# ── Segment roundtrip ────────────────────────────────────────────────


def test_segment_roundtrip_bytes_match(admin_client, unauthenticated_client, db):
    """Push a segment with the owning node key; fetch it back as an
    authenticated user.  The payload must survive the cache unchanged —
    this is the contract MSE decoders rely on."""
    from app.api.hls import _segment_cache

    raw_key, cam_id = _seed_node_with_camera(db)
    # A realistic-ish TS sync byte payload so we know nothing is
    # reinterpreting bytes as text along the way.
    payload = b"\x47\x40\x00\x10" + bytes(range(256)) * 4

    push = unauthenticated_client.post(
        f"/api/cameras/{cam_id}/push-segment?filename=segment_00042.ts",
        content=payload,
        headers={"X-Node-API-Key": raw_key},
    )
    assert push.status_code == 200, push.text
    assert push.json()["success"] is True

    # Cache is populated server-side.
    assert cam_id in _segment_cache
    assert "segment_00042.ts" in _segment_cache[cam_id]

    fetch = admin_client.get(f"/api/cameras/{cam_id}/segment/segment_00042.ts")
    assert fetch.status_code == 200
    assert fetch.headers["content-type"] == "video/mp2t"
    assert fetch.content == payload


def test_segment_fetch_missing_returns_404(admin_client, db):
    """A filename that was never pushed must 404 — no accidental fallback
    to another camera's cache."""
    _raw_key, cam_id = _seed_node_with_camera(db)
    resp = admin_client.get(f"/api/cameras/{cam_id}/segment/segment_99999.ts")
    assert resp.status_code == 404


def test_segment_filename_rejects_path_traversal(
    admin_client, unauthenticated_client, db
):
    """The endpoint must reject anything that isn't ``segment_\\d+\\.ts``.
    A successful traversal here would let a node poison the cache for
    arbitrary keys or let a viewer request arbitrary files — both bad."""
    raw_key, cam_id = _seed_node_with_camera(db)

    bad_names = [
        "../secret.env",
        "segment_00001.ts/..",
        "seg.ts",
        "segment_a.ts",  # non-numeric
    ]
    for name in bad_names:
        push = unauthenticated_client.post(
            f"/api/cameras/{cam_id}/push-segment?filename={name}",
            content=b"\x00",
            headers={"X-Node-API-Key": raw_key},
        )
        assert push.status_code == 400, (name, push.text)

        fetch = admin_client.get(f"/api/cameras/{cam_id}/segment/{name}")
        # Either 400 (validation) or 404 (routing) is acceptable —
        # what matters is that we never 200 with real bytes.
        assert fetch.status_code in (400, 404), (name, fetch.status_code)


def test_push_segment_rejects_oversize(unauthenticated_client, db, monkeypatch):
    """SEGMENT_PUSH_MAX_BYTES is the safety valve keeping a malicious or
    buggy node from exhausting RAM — verify the cap actually fires."""
    from app.core.config import settings

    raw_key, cam_id = _seed_node_with_camera(db)
    # Pin to a tiny cap so the test doesn't have to ship a 2 MB blob.
    monkeypatch.setattr(settings, "SEGMENT_PUSH_MAX_BYTES", 128)

    resp = unauthenticated_client.post(
        f"/api/cameras/{cam_id}/push-segment?filename=segment_00001.ts",
        content=b"\x00" * 256,
        headers={"X-Node-API-Key": raw_key},
    )
    # 413 Payload Too Large — Content-Length on the request will land us
    # in the early-rejection path, but the assertion below tolerates the
    # belt-and-suspenders post-read path too (same status either way).
    assert resp.status_code == 413
    assert "max" in resp.json()["detail"].lower()


def test_push_segment_rejects_oversize_via_content_length_header(
    unauthenticated_client, db, monkeypatch
):
    """Early rejection — a request that DECLARES too many bytes is
    rejected before any body is read into memory.  This is the lever
    that prevents a 10 GB attempted upload from costing 10 GB of RAM
    even briefly.

    We can't directly observe "body wasn't read" through the test
    client, but we *can* prove the early path is reachable by sending
    a Content-Length that lies (declares 10 MB, sends 256 bytes).
    The early check fires on the declared size, the actual body never
    matters.
    """
    from app.core.config import settings

    raw_key, cam_id = _seed_node_with_camera(db)
    monkeypatch.setattr(settings, "SEGMENT_PUSH_MAX_BYTES", 128)

    # httpx (the test-client transport) computes Content-Length from
    # the body it sends, so we send 256 bytes and assert the declared
    # size triggers rejection.  An attacker would behave the same way
    # (real attack: lie about the body to inflate it past the cap).
    resp = unauthenticated_client.post(
        f"/api/cameras/{cam_id}/push-segment?filename=segment_00001.ts",
        content=b"\x00" * 256,
        headers={"X-Node-API-Key": raw_key},
    )
    assert resp.status_code == 413
    detail = resp.json()["detail"].lower()
    assert "declared" in detail
    assert "max is 128" in detail


def test_push_playlist_rejects_oversize(unauthenticated_client, db, monkeypatch):
    """Playlists are tiny in real life (a few hundred bytes); the cap
    is generous (64 KB default) but enforced — same Content-Length
    early-rejection path as push-segment."""
    from app.core.config import settings

    raw_key, cam_id = _seed_node_with_camera(db)
    monkeypatch.setattr(settings, "PLAYLIST_PUSH_MAX_BYTES", 64)

    resp = unauthenticated_client.post(
        f"/api/cameras/{cam_id}/playlist",
        content=b"#EXTM3U\n" + b"#EXTINF:1.0,\nseg.ts\n" * 50,
        headers={"X-Node-API-Key": raw_key},
    )
    assert resp.status_code == 413


# ── Plan-cap enforcement ─────────────────────────────────────────────
#
# When an org downgrades (or cancels) and ends up over its camera cap,
# `enforce_camera_cap` flips `Camera.disabled_by_plan = True` on the
# over-cap rows. Push-segment must then reject their uploads with
# HTTP 402 + a structured `plan_limit_hit` body so the CloudNode can
# surface the reason in its TUI instead of silently retrying.


def test_push_segment_rejects_disabled_by_plan_camera(
    unauthenticated_client, db
):
    """A camera flagged ``disabled_by_plan`` must return HTTP 402 with a
    ``plan_limit_hit`` body — not 200, not a silent drop."""
    raw_key, cam_id = _seed_node_with_camera(db)

    # Simulate the webhook / register path having flagged this camera.
    cam = db.query(Camera).filter_by(camera_id=cam_id).one()
    cam.disabled_by_plan = True
    db.commit()

    resp = unauthenticated_client.post(
        f"/api/cameras/{cam_id}/push-segment?filename=segment_00001.ts",
        content=b"\x47" + b"\x00" * 187,  # one TS packet
        headers={"X-Node-API-Key": raw_key},
    )
    assert resp.status_code == 402, resp.text
    body = resp.json()
    # FastAPI nests HTTPException.detail under the `detail` key.
    hit = body["detail"]["plan_limit_hit"]
    assert "plan" in hit and "max_cameras" in hit
    assert hit["skipped"] == ["HlsTestCam"]
    assert "Upgrade" in hit["detail"]


def test_push_segment_allows_enabled_camera_on_same_org(
    unauthenticated_client, db
):
    """Defensive sanity: a sibling camera on the same node that is NOT
    flagged still streams. Proves the gate is per-camera, not per-node
    or per-org."""
    raw_key, cam_enabled_id = _seed_node_with_camera(db)

    # Add a second camera on the same node, flag only the second.
    node = (
        db.query(CameraNode)
        .filter_by(api_key_hash=hashlib.sha256(raw_key.encode()).hexdigest())
        .one()
    )
    blocked_id = "cam_hls_" + uuid.uuid4().hex[:8]
    db.add(
        Camera(
            camera_id=blocked_id,
            org_id=node.org_id,
            node_id=node.id,
            name="Blocked Cam",
            status="online",
            disabled_by_plan=True,
        )
    )
    db.commit()

    ok = unauthenticated_client.post(
        f"/api/cameras/{cam_enabled_id}/push-segment?filename=segment_00001.ts",
        content=b"\x47" + b"\x00" * 187,
        headers={"X-Node-API-Key": raw_key},
    )
    assert ok.status_code == 200, ok.text

    blocked = unauthenticated_client.post(
        f"/api/cameras/{blocked_id}/push-segment?filename=segment_00001.ts",
        content=b"\x47" + b"\x00" * 187,
        headers={"X-Node-API-Key": raw_key},
    )
    assert blocked.status_code == 402, blocked.text


# ── Cache eviction ───────────────────────────────────────────────────


def test_segment_cache_evicts_oldest_when_over_limit(
    unauthenticated_client, db, monkeypatch
):
    """Push MAX+3 segments — only the newest MAX must remain.  This is
    the whole reason segments are keyed by monotonic filename prefix."""
    from app.api.hls import _segment_cache
    from app.core.config import settings

    raw_key, cam_id = _seed_node_with_camera(db)
    monkeypatch.setattr(settings, "SEGMENT_CACHE_MAX_PER_CAMERA", 5)

    for i in range(1, 9):  # 8 segments, cap is 5
        resp = unauthenticated_client.post(
            f"/api/cameras/{cam_id}/push-segment?filename=segment_{i:05d}.ts",
            content=bytes([i]),
            headers={"X-Node-API-Key": raw_key},
        )
        assert resp.status_code == 200

    cached = _segment_cache[cam_id]
    assert len(cached) == 5
    # Oldest three must be gone; newest five must remain.
    for i in range(1, 4):
        assert f"segment_{i:05d}.ts" not in cached
    for i in range(4, 9):
        assert f"segment_{i:05d}.ts" in cached


def test_global_cache_cap_evicts_across_cameras(db, monkeypatch):
    """Direct unit test for _evict_global_oldest — the eviction loop
    that keeps the SUM of all camera caches under
    SEGMENT_CACHE_MAX_TOTAL_BYTES.

    Per-camera eviction is already covered above; this pins the
    cross-camera behaviour because the policy choice (oldest-first
    globally, ignoring per-camera ownership) is the load-bearing
    decision.  A future refactor that switched to per-camera fairness
    would silently fail to recover memory under the pathological
    "many cameras, low per-camera segment count" scenario the global
    cap exists for.
    """
    import time

    import app.api.hls as hls
    from app.api.hls import _evict_global_oldest, _segment_cache

    # Reset the module-level cache so the test is hermetic.  Other
    # tests in this file may have left entries behind via the push
    # endpoint; we isolate by clearing.
    _segment_cache.clear()

    # Three cameras, three segments each, deterministic timestamps.
    # Bytes ascending so the budget math is easy to read.
    base_ts = time.monotonic()
    for cam_idx in range(3):
        cam_id = f"cam_{cam_idx}"
        for seg_idx in range(3):
            # Timestamps interleave across cameras so oldest-first
            # eviction crosses camera boundaries — that's the
            # behaviour we want to pin.  Order:
            #   cam_0/seg_0 < cam_1/seg_0 < cam_2/seg_0 < cam_0/seg_1 < ...
            ts = base_ts + (seg_idx * 3.0) + cam_idx
            body = b"x" * 100  # 100 bytes per segment
            _segment_cache.setdefault(cam_id, {})[f"segment_{seg_idx:05d}.ts"] = (body, ts)

    # This test populates _segment_cache directly (not via push_segment),
    # so the running byte counter that _evict_global_oldest reads isn't
    # maintained.  Reconcile it to ground truth before eviction.
    hls._segment_cache_byte_total = hls._recompute_segment_cache_bytes()

    # 9 segments × 100 bytes = 900 total.  Cap at 500 → must evict
    # the 4 oldest to land at 500.
    evicted = _evict_global_oldest(500)
    assert evicted == 4

    # The 4 oldest-by-ts evictees: cam_0/seg_0, cam_1/seg_0,
    # cam_2/seg_0, cam_0/seg_1.  The remaining 5 segments must all
    # have ts >= base_ts + 4.0 (cam_1/seg_1).
    remaining = [
        (cam_id, fname, ts)
        for cam_id, cache in _segment_cache.items()
        for fname, (_body, ts) in cache.items()
    ]
    assert len(remaining) == 5

    # cam_0 should have lost both seg_0 and seg_1; only seg_2 remains.
    assert "segment_00000.ts" not in _segment_cache.get("cam_0", {})
    assert "segment_00001.ts" not in _segment_cache.get("cam_0", {})
    assert "segment_00002.ts" in _segment_cache.get("cam_0", {})

    # cam_1 should have lost seg_0; seg_1 + seg_2 remain.
    assert "segment_00000.ts" not in _segment_cache.get("cam_1", {})
    assert "segment_00001.ts" in _segment_cache.get("cam_1", {})
    assert "segment_00002.ts" in _segment_cache.get("cam_1", {})


def test_global_cache_cap_no_op_when_under_budget(db):
    """When total bytes are already under cap, eviction must not
    touch anything.  Cheap-path correctness."""
    import time

    import app.api.hls as hls
    from app.api.hls import _evict_global_oldest, _segment_cache

    _segment_cache.clear()
    _segment_cache["cam_a"] = {"segment_00001.ts": (b"x" * 100, time.monotonic())}
    # Direct population bypasses the push path's counter maintenance —
    # reconcile so the O(1) under-budget fast path reads the truth.
    hls._segment_cache_byte_total = hls._recompute_segment_cache_bytes()

    evicted = _evict_global_oldest(10_000)  # cap way above 100 bytes
    assert evicted == 0
    assert _segment_cache["cam_a"] == {"segment_00001.ts": (b"x" * 100, _segment_cache["cam_a"]["segment_00001.ts"][1])}


# ── Playlist rewriting ───────────────────────────────────────────────

_RAW_PLAYLIST = (
    "#EXTM3U\n"
    "#EXT-X-VERSION:3\n"
    "#EXT-X-TARGETDURATION:2\n"
    "#EXT-X-MEDIA-SEQUENCE:42\n"
    "#EXTINF:2.0,\n"
    "segment_00042.ts\n"
    "#EXTINF:2.0,\n"
    "segment_00043.ts\n"
    "#EXTINF:2.0,\n"
    "segment_00044.ts\n"
)


def test_playlist_segments_use_relative_proxy_paths(
    admin_client,
    unauthenticated_client,
    db,
):
    """After a CloudNode pushes a raw playlist, the cached rewrite served
    to the browser must route every segment through this backend via a
    relative path — never an absolute URL.  A stray absolute URL would
    bypass our auth layer and point the browser at someone else's host."""
    raw_key, cam_id = _seed_node_with_camera(db)

    push = unauthenticated_client.post(
        f"/api/cameras/{cam_id}/playlist",
        content=_RAW_PLAYLIST,
        headers={"X-Node-API-Key": raw_key},
    )
    assert push.status_code == 200

    fetch = admin_client.get(f"/api/cameras/{cam_id}/stream.m3u8")
    assert fetch.status_code == 200
    body = fetch.text

    # Relative proxy paths — browser resolves these against stream.m3u8.
    assert "segment/segment_00042.ts" in body
    assert "segment/segment_00043.ts" in body
    assert "segment/segment_00044.ts" in body

    # No absolute URLs and no presigned-style query params should ever
    # appear in a served playlist.
    lowered = body.lower()
    for marker in ("https://", "http://", "x-amz-", "?expires=", "&expires="):
        assert marker not in lowered, (marker, body)


def test_playlist_does_not_inject_codec_header(
    admin_client, unauthenticated_client, db
):
    """#EXT-X-CODECS is only valid in Master Playlists per HLS spec.
    Injecting it into Media Playlists causes hls.js to fail parsing."""
    raw_key, cam_id = _seed_node_with_camera(db)

    unauthenticated_client.post(
        f"/api/cameras/{cam_id}/playlist",
        content=_RAW_PLAYLIST,
        headers={"X-Node-API-Key": raw_key},
    )
    body = admin_client.get(f"/api/cameras/{cam_id}/stream.m3u8").text

    # No CODECS line in media playlist — codec info is in the bitstream
    assert "#EXT-X-CODECS:" not in body


def test_playlist_rewrite_handles_path_prefixed_segment_uris(
    admin_client,
    unauthenticated_client,
    db,
):
    """FFmpeg's HLS muxer sometimes writes the ``-hls_segment_filename``
    verbatim into the playlist URIs — so on a node where the segment
    filename is given with a relative path prefix (the production shape,
    ``./data/hls/<cam>/segment_%05d.ts``), the playlist contains lines
    like ``./data/hls/<cam>/segment_00042.ts`` instead of bare basenames.

    The rewriter must still normalize these to ``segment/<basename>``;
    otherwise the browser tries to fetch the stale relative path against
    its own origin and 404s — segments-pushing-but-nothing-playing,
    which is exactly the symptom we hit on a real Pi deploy.
    """
    raw_key, cam_id = _seed_node_with_camera(db)
    prefixed_playlist = (
        "#EXTM3U\n"
        "#EXT-X-VERSION:3\n"
        "#EXT-X-TARGETDURATION:2\n"
        "#EXT-X-MEDIA-SEQUENCE:42\n"
        "#EXTINF:2.0,\n"
        "./data/hls/db2782d7_dev_video0/segment_00042.ts\n"
        "#EXTINF:2.0,\n"
        "./data/hls/db2782d7_dev_video0/segment_00043.ts\n"
    )

    push = unauthenticated_client.post(
        f"/api/cameras/{cam_id}/playlist",
        content=prefixed_playlist,
        headers={"X-Node-API-Key": raw_key},
    )
    assert push.status_code == 200

    body = admin_client.get(f"/api/cameras/{cam_id}/stream.m3u8").text
    # The path prefix must be stripped — we want the relative proxy URI
    # only, never the node-local filesystem path.
    assert "segment/segment_00042.ts" in body
    assert "segment/segment_00043.ts" in body
    assert "./data/hls/" not in body


def test_playlist_rewrite_handles_crlf_line_endings(
    admin_client,
    unauthenticated_client,
    db,
):
    """A CloudNode running on Windows can write the playlist with CRLF
    line endings.  The regex must treat ``\\r`` as trailing whitespace
    so the emitted URI is still the clean ``segment/<name>`` — a stray
    ``\\r`` in the middle of the URI would break the browser's fetch."""
    raw_key, cam_id = _seed_node_with_camera(db)
    crlf_playlist = (
        "#EXTM3U\r\n"
        "#EXT-X-VERSION:3\r\n"
        "#EXT-X-TARGETDURATION:2\r\n"
        "#EXT-X-MEDIA-SEQUENCE:10\r\n"
        "#EXTINF:2.0,\r\n"
        "segment_00010.ts\r\n"
        "#EXTINF:2.0,\r\n"
        "segment_00011.ts\r\n"
    )

    unauthenticated_client.post(
        f"/api/cameras/{cam_id}/playlist",
        content=crlf_playlist,
        headers={"X-Node-API-Key": raw_key},
    )

    body = admin_client.get(f"/api/cameras/{cam_id}/stream.m3u8").text
    # The URI line on its own — no stray \r glued onto the filename.
    assert "segment/segment_00010.ts\r" in body or "segment/segment_00010.ts\n" in body
    # The rewritten URI line should not contain ``\rsegment`` anywhere —
    # that'd mean a CR survived into the middle of the URI.
    assert "\rsegment" not in body.replace("\r\n", "\n")


def test_playlist_rewrite_is_idempotent_across_pushes(
    admin_client,
    unauthenticated_client,
    db,
):
    """Push the same raw playlist twice.  The served version should look
    the same — no doubled codec lines, no re-prefixed segment paths like
    ``segment/segment/segment_00042.ts``."""
    raw_key, cam_id = _seed_node_with_camera(db)
    headers = {"X-Node-API-Key": raw_key}

    unauthenticated_client.post(
        f"/api/cameras/{cam_id}/playlist",
        content=_RAW_PLAYLIST,
        headers=headers,
    )
    first = admin_client.get(f"/api/cameras/{cam_id}/stream.m3u8").text

    unauthenticated_client.post(
        f"/api/cameras/{cam_id}/playlist",
        content=_RAW_PLAYLIST,
        headers=headers,
    )
    second = admin_client.get(f"/api/cameras/{cam_id}/stream.m3u8").text

    assert first == second
    assert "segment/segment/" not in second
    # No CODECS line in media playlist
    assert "#EXT-X-CODECS:" not in second


# ── Viewer-hour cap enforcement ──────────────────────────────────────
#
# Free orgs get 30 viewer-hours/month, Pro 300, Pro Plus 1500.  When an
# org exceeds its cap, GET /segment must 429 — otherwise users stream
# unlimited live video on the Free tier and the whole plan ladder
# collapses.  Money-flavored: silently broken cap = silent revenue loss.


def test_get_viewer_seconds_used_warms_cache_from_db(db):
    """Regression: after a deploy / app restart the in-memory cache
    is empty.  ``get_viewer_seconds_used`` must lazy-load from the
    OrgMonthlyUsage row instead of returning 0.

    The dashboard's "viewer hours this month" widget calls this on
    every page load via /api/nodes/plan; if it returns 0 from a cold
    cache, the operator sees their counter "reset" after every
    deploy (which is what surfaced the bug)."""
    from app.api import hls as hls_mod
    from app.models.models import OrgMonthlyUsage

    org_id = "org_warm_test"
    ym = hls_mod._current_year_month()

    # Seed a real OrgMonthlyUsage row that pre-dates this process.
    db.add(OrgMonthlyUsage(
        org_id=org_id, year_month=ym, viewer_seconds=12345,
    ))
    db.commit()

    # Cache is empty for this org because the conftest reset wipes
    # release_cache (different module) — but viewer_seconds caches
    # also start empty in tests.  Be defensive: explicitly clear so
    # this test isn't influenced by other tests' leftover state.
    with hls_mod._viewer_usage_lock:
        hls_mod._cached_viewer_seconds.pop((org_id, ym), None)
        hls_mod._pending_viewer_seconds.pop((org_id, ym), None)

    # Cold-cache read should still return the DB value, not 0.
    used = hls_mod.get_viewer_seconds_used(org_id)
    assert used == 12345, (
        f"cold-cache read returned {used} — get_viewer_seconds_used "
        "must warm from DB before reading or the dashboard widget "
        "shows 0 after every deploy"
    )


def test_segment_delivery_blocks_when_over_viewer_hour_cap(
    admin_client, unauthenticated_client, db, monkeypatch
):
    """Push a segment, then make the org appear to have used 31 hours
    on the Free plan (cap = 30h).  The segment GET should 429."""
    raw_key, cam_id = _seed_node_with_camera(db)

    # Push so the cache has a segment to potentially serve.
    pushed_bytes = b"\xfa\xfa\xfa" * 64
    push = unauthenticated_client.post(
        f"/api/cameras/{cam_id}/push-segment?filename=segment_00001.ts",
        content=pushed_bytes,
        headers={"X-Node-API-Key": raw_key},
    )
    assert push.status_code == 200

    # Force the effective plan to free_org (admin_client fixture is on
    # pro, which has a much larger cap that we'd have to inject more
    # viewer-seconds to exceed).  effective_plan_for_caps is what the
    # endpoint reads, NOT user.plan from the JWT — the test mirrors
    # production behaviour by overriding the DB-resolved plan.
    from app.core import plans as plans_mod
    monkeypatch.setattr(
        plans_mod, "effective_plan_for_caps", lambda _db, _org: "free_org"
    )

    # Inject 31h of viewer-seconds (Free cap is 30h).  Bypassing the DB
    # warm-cache via the in-memory _cached_viewer_seconds dict is the
    # fastest way to put the org over cap.
    from app.api import hls as hls_mod
    over_cap_seconds = 31 * 3600
    ym = hls_mod._current_year_month()
    hls_mod._cached_viewer_seconds[("org_test123", ym)] = over_cap_seconds

    try:
        resp = admin_client.get(f"/api/cameras/{cam_id}/segment/segment_00001.ts")
        assert resp.status_code == 429
        body = resp.json()
        assert "viewer-hour" in body["detail"].lower()
    finally:
        # Clean up so the next test isn't influenced.
        hls_mod._cached_viewer_seconds.pop(("org_test123", ym), None)


def test_segment_delivery_allowed_when_under_viewer_hour_cap(
    admin_client, unauthenticated_client, db, monkeypatch
):
    """Same setup as above but at 29h — under the 30h Free cap.
    Segment must serve normally."""
    raw_key, cam_id = _seed_node_with_camera(db)

    pushed_bytes = b"\xab" * 256
    unauthenticated_client.post(
        f"/api/cameras/{cam_id}/push-segment?filename=segment_00001.ts",
        content=pushed_bytes,
        headers={"X-Node-API-Key": raw_key},
    )

    from app.core import plans as plans_mod
    monkeypatch.setattr(
        plans_mod, "effective_plan_for_caps", lambda _db, _org: "free_org"
    )

    from app.api import hls as hls_mod
    under_cap_seconds = 29 * 3600
    ym = hls_mod._current_year_month()
    hls_mod._cached_viewer_seconds[("org_test123", ym)] = under_cap_seconds

    try:
        resp = admin_client.get(f"/api/cameras/{cam_id}/segment/segment_00001.ts")
        assert resp.status_code == 200
        assert resp.content == pushed_bytes
    finally:
        hls_mod._cached_viewer_seconds.pop(("org_test123", ym), None)


# ── Playlist rewriter unit tests ─────────────────────────────────────
#
# The integration tests above exercise `_rewrite_playlist` end-to-end
# through the HTTP layer.  These hit the function directly so a regex
# regression lands a sharp local failure (the function is the heart of
# stream rewriting — every browser playlist fetch is gated on it).


def test_rewrite_playlist_strips_bare_basename_prefix():
    from app.api.hls import _rewrite_playlist
    out = _rewrite_playlist("#EXTM3U\n#EXTINF:2.0,\nsegment_00042.ts\n")
    assert "segment/segment_00042.ts" in out


def test_rewrite_playlist_strips_forward_slash_path_prefix():
    from app.api.hls import _rewrite_playlist
    out = _rewrite_playlist(
        "#EXTM3U\n#EXTINF:2.0,\n./data/hls/cam_id/segment_00042.ts\n"
    )
    assert "segment/segment_00042.ts" in out
    assert "./data/" not in out


def test_rewrite_playlist_strips_backslash_path_prefix():
    """Windows CloudNodes can write backslash-separated paths into the
    playlist (e.g. ``data\\hls\\cam_id\\segment_00042.ts``).  The
    rewriter must strip backslash prefixes too — otherwise the browser
    fetch URL contains an embedded backslash and 404s."""
    from app.api.hls import _rewrite_playlist
    out = _rewrite_playlist(
        "#EXTM3U\n#EXTINF:2.0,\n.\\data\\hls\\cam_id\\segment_00042.ts\n"
    )
    assert "segment/segment_00042.ts" in out
    assert "\\" not in out  # no stray backslash survives


def test_rewrite_playlist_does_not_rewrite_tag_lines_quoting_segments():
    """``#EXT-X-MAP:URI="segment_init.ts"`` is a tag line that
    references a segment-shaped name in a quoted attribute.  The
    rewriter must NOT touch it — only the bare URI lines should be
    proxied through ``segment/``.  The negative lookahead ``(?!#)``
    in _RE_SEGMENT_URI is what gates this."""
    from app.api.hls import _rewrite_playlist
    src = (
        "#EXTM3U\n"
        '#EXT-X-MAP:URI="segment_init.ts"\n'
        "#EXTINF:2.0,\n"
        "segment_00001.ts\n"
    )
    out = _rewrite_playlist(src)
    # Tag line untouched
    assert '#EXT-X-MAP:URI="segment_init.ts"' in out
    # Bare URI line rewritten
    assert "segment/segment_00001.ts" in out


def test_rewrite_playlist_strips_codecs_tag():
    """``#EXT-X-CODECS`` is only valid in master playlists; injecting it
    into a media playlist makes hls.js attempt master parsing and never
    fire MANIFEST_PARSED."""
    from app.api.hls import _rewrite_playlist
    out = _rewrite_playlist(
        "#EXTM3U\n"
        '#EXT-X-CODECS:"avc1.42e01e,mp4a.40.2"\n'
        "#EXTINF:2.0,\n"
        "segment_00001.ts\n"
    )
    assert "#EXT-X-CODECS" not in out


def test_rewrite_playlist_idempotent():
    """Already-rewritten ``segment/<name>`` lines must not double-prefix
    on a second pass — i.e. no ``segment/segment/segment_00001.ts``."""
    from app.api.hls import _rewrite_playlist
    src = "#EXTM3U\n#EXTINF:2.0,\nsegment_00001.ts\n"
    once = _rewrite_playlist(src)
    twice = _rewrite_playlist(once)
    assert once == twice
    assert "segment/segment/" not in twice


def test_rewrite_playlist_handles_trailing_whitespace():
    """CRLF line endings, trailing tabs/spaces — the regex tolerates
    them so the emitted URI is clean."""
    from app.api.hls import _rewrite_playlist
    # CRLF
    out = _rewrite_playlist("#EXTM3U\r\n#EXTINF:2.0,\r\nsegment_00001.ts\r\n")
    assert "segment/segment_00001.ts" in out
    # Trailing space + tab
    out = _rewrite_playlist("#EXTM3U\n#EXTINF:2.0,\nsegment_00001.ts \t\n")
    assert "segment/segment_00001.ts" in out


def test_rewrite_playlist_preserves_non_segment_lines():
    """Comments, EXTINF tags, and the EXTM3U header must pass through
    unchanged."""
    from app.api.hls import _rewrite_playlist
    src = (
        "#EXTM3U\n"
        "#EXT-X-VERSION:3\n"
        "#EXT-X-TARGETDURATION:2\n"
        "#EXT-X-MEDIA-SEQUENCE:42\n"
        "#EXTINF:2.000,\n"
        "segment_00042.ts\n"
    )
    out = _rewrite_playlist(src)
    for line in [
        "#EXTM3U",
        "#EXT-X-VERSION:3",
        "#EXT-X-TARGETDURATION:2",
        "#EXT-X-MEDIA-SEQUENCE:42",
        "#EXTINF:2.000,",
    ]:
        assert line in out


def test_stream_without_push_returns_404(admin_client, db):
    """No cached playlist → 404.  The viewer learns the stream hasn't
    started yet rather than getting a bogus empty playlist."""
    _raw_key, cam_id = _seed_node_with_camera(db)
    resp = admin_client.get(f"/api/cameras/{cam_id}/stream.m3u8")
    assert resp.status_code == 404
    assert "not started" in resp.json()["detail"].lower()


# ── cleanup_camera_cache ─────────────────────────────────────────────


def test_cleanup_camera_cache_drops_segments_and_playlist(
    unauthenticated_client,
    db,
):
    """``cleanup_camera_cache`` is the function called from every delete
    path (camera delete, node delete, org delete, stale-camera sweep).
    It must scrub BOTH segment and playlist state for the camera — a
    stale playlist referencing now-deleted segments would break the next
    camera that happens to reuse the same id."""
    from app.api.hls import _playlist_cache, _segment_cache, cleanup_camera_cache

    raw_key, cam_id = _seed_node_with_camera(db)
    headers = {"X-Node-API-Key": raw_key}

    unauthenticated_client.post(
        f"/api/cameras/{cam_id}/push-segment?filename=segment_00001.ts",
        content=b"\x01\x02\x03",
        headers=headers,
    )
    unauthenticated_client.post(
        f"/api/cameras/{cam_id}/playlist",
        content=_RAW_PLAYLIST,
        headers=headers,
    )
    assert cam_id in _segment_cache
    assert cam_id in _playlist_cache

    cleanup_camera_cache(cam_id)

    assert cam_id not in _segment_cache
    assert cam_id not in _playlist_cache
    # Idempotent — calling again on an already-cleaned cam must not raise.
    cleanup_camera_cache(cam_id)
