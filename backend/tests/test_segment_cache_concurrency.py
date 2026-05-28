"""Regression test for the _segment_cache cross-thread race.

`attach_clip` is a sync MCP tool — FastMCP runs it in an AnyIO worker
thread, off the event loop.  It reads `_segment_cache` (snapshot keys,
read bytes) while the event-loop push path mutates the same dict via
`_evict_segment_cache` / `_evict_global_oldest` (del / pop).  Before the
`_segment_cache_lock` was added, the worker thread's
`sorted(cam_cache.keys())` could be interrupted mid-iteration by a
delete from the other thread, raising:

    RuntimeError: dictionary changed size during iteration

This reproduces that interleaving with two threads hammering the read
and mutate paths concurrently, and asserts it never raises.  Without
the lock the stress loop trips within a few hundred iterations on
CPython; with the lock it runs clean.
"""

import threading
import time

import app.api.hls as hls
from app.api.hls import snapshot_recent_segment_bytes


def _clear_cache():
    with hls._segment_cache_lock:
        hls._segment_cache.clear()


def test_snapshot_vs_eviction_no_dict_mutation_error(monkeypatch):
    """Two threads: one reads via the public snapshot helper, one
    mutates via the push+evict path.  Run a few thousand interleavings
    and assert neither thread raises.

    A small global byte cap forces `_evict_global_oldest` to actually
    delete on most pushes, maximising the mutation rate so the race
    window stays wide open throughout the run.
    """
    _clear_cache()
    # Tiny cap so eviction fires aggressively (each ~1 KB segment over
    # ~30 KB total triggers global eviction).
    monkeypatch.setattr(hls.settings, "SEGMENT_CACHE_MAX_TOTAL_BYTES", 30_000)
    monkeypatch.setattr(hls.settings, "SEGMENT_CACHE_MAX_PER_CAMERA", 40)

    camera_id = "cam_race_test"
    payload = b"x" * 1024  # 1 KB per segment
    stop = threading.Event()
    errors: list[Exception] = []

    def writer():
        """Mimics the event-loop push path: insert + both eviction passes,
        all under the cache lock (same as push_segment does)."""
        seq = 0
        try:
            while not stop.is_set():
                seq += 1
                fname = f"segment_{seq:05d}.ts"
                with hls._segment_cache_lock:
                    hls._segment_cache.setdefault(camera_id, {})[fname] = (
                        payload,
                        time.monotonic(),
                    )
                    hls._evict_segment_cache(camera_id)
                    hls._evict_global_oldest(hls.settings.SEGMENT_CACHE_MAX_TOTAL_BYTES)
        except Exception as e:  # noqa: BLE001 — capture for the assert
            errors.append(e)

    def reader():
        """Mimics attach_clip's worker-thread read via the public helper."""
        try:
            while not stop.is_set():
                snapshot_recent_segment_bytes(camera_id, 15)
        except Exception as e:  # noqa: BLE001
            errors.append(e)

    threads = [
        threading.Thread(target=writer),
        threading.Thread(target=reader),
        threading.Thread(target=reader),  # two readers widen the window
    ]
    for t in threads:
        t.start()

    # Let them interleave for a bounded wall-clock window.  ~0.75s is
    # plenty: the unlocked version trips well under 100ms on CPython.
    time.sleep(0.75)
    stop.set()
    for t in threads:
        t.join(timeout=5)

    _clear_cache()

    assert not errors, (
        f"segment-cache race regressed — got {type(errors[0]).__name__}: "
        f"{errors[0]}"
    )


def test_snapshot_helper_return_contract():
    """Pin the three-way return contract attach_clip depends on:
      - None  → no cache bucket (stream never live / fully evicted)
      - []    → bucket exists but nothing readable / count <= 0
      - list  → newest `count` segments' bytes, oldest-first
    """
    _clear_cache()
    camera_id = "cam_contract"

    # No bucket at all → None.
    assert snapshot_recent_segment_bytes(camera_id, 5) is None

    # Seed three segments with distinguishable bytes.
    with hls._segment_cache_lock:
        hls._segment_cache[camera_id] = {
            "segment_00001.ts": (b"one", 1.0),
            "segment_00002.ts": (b"two", 2.0),
            "segment_00003.ts": (b"three", 3.0),
        }

    # count <= 0 → [] (bucket exists, nothing selected).
    assert snapshot_recent_segment_bytes(camera_id, 0) == []

    # Newest 2, oldest-first (sorted by filename = sequence order).
    assert snapshot_recent_segment_bytes(camera_id, 2) == [b"two", b"three"]

    # count larger than available → all of them, oldest-first.
    assert snapshot_recent_segment_bytes(camera_id, 99) == [b"one", b"two", b"three"]

    _clear_cache()
