"""The live video path, which the request/response differential cannot reach.

Every other harness here sends one request to each tier and compares the
two answers. That does not work for HLS, because the answer depends on
what the *same* tier was told earlier: a segment is readable only from
the process that was pushed it, and each tier has its own cache. A
single request proves nothing.

So this sends a *scenario* — an ordered run of requests — to one tier,
records what came back from each step, then reseeds and sends the same
scenario to the other. What is compared is the two transcripts.

That also makes the cache policies testable, which is the point: which
segment is evicted first, when the playlist goes stale, what a re-push
of the same filename does to the count. `tiers.sh` shrinks the caches
(five segments per camera, four kilobytes in total) so those paths are
reachable without pushing hundreds of megabytes through both stacks.

Bodies are compared by length and digest, not inline: a segment is
binary and a playlist is only interesting in its rewritten shape, which
is asserted by the scenarios that read one back.

Usage: hls_diff.py <token> [-v]
"""

import hashlib
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
PYTHON_BASE = "http://127.0.0.1:8001"
RUST_BASE = "http://127.0.0.1:8000"
PG_CONTAINER = os.environ.get("PG_CONTAINER", "cc-schema-test")
REDIS_CONTAINER = os.environ.get("REDIS_CONTAINER", "cc-redis-test")

TOKEN = sys.argv[1] if len(sys.argv) > 1 else ""
VERBOSE = "-v" in sys.argv
DIFF_ONLY = os.environ.get("DIFF_ONLY", "")

NODE_KEY = "test-node-key"          # node-aaaa1111, which owns the fixtures
# Each scenario gets a camera of its own, created after the reseed and
# named for the scenario. The caches are keyed by camera id and live in
# the process, not the database — so without this a scenario would read
# whatever earlier scenarios left in the tier, and the two tiers would
# only agree while they happened to have the same history. Restarting
# both between scenarios would do it too, at five seconds apiece.
CAMERA = "{cam}"
OTHER_NODES_CAMERA = "cam-dddd"     # same org, a different node
NODELESS_CAMERA = "cam-orphan"      # no node at all

# Matches tiers.sh. A scenario that pushes six segments expects the
# first to be gone; one that pushes five 1000-byte segments is at the
# global ceiling.
MAX_PER_CAMERA = int(os.environ.get("SEGMENT_CACHE_MAX_PER_CAMERA", "5"))
MAX_TOTAL_BYTES = int(os.environ.get("SEGMENT_CACHE_MAX_TOTAL_BYTES", "3000000"))


class FixtureError(RuntimeError):
    """The run proved nothing; say so rather than reporting agreement."""


def psql(sql: str) -> None:
    proc = subprocess.run(
        ["docker", "exec", "-i", PG_CONTAINER, "psql", "-U", "cc", "-d", "cc", "-q",
         "-v", "ON_ERROR_STOP=1"],
        input=sql.encode(), capture_output=True, timeout=180, check=False,
    )
    if proc.returncode != 0:
        raise FixtureError(f"psql failed: {proc.stderr.decode()[:300]}")


def reseed() -> None:
    psql((HERE / "seed_cameras.sql").read_text())
    subprocess.run(["docker", "exec", REDIS_CONTAINER, "redis-cli", "FLUSHDB"],
                   capture_output=True, timeout=60, check=False)


def request(base, method, path, *, body=None, key=None, token=False, headers=None):
    req = urllib.request.Request(base + path, method=method, data=body)
    if key is not None:
        req.add_header("X-Node-API-Key", key)
    if token:
        req.add_header("Authorization", f"Bearer {TOKEN}")
    for name, value in (headers or {}).items():
        req.add_header(name, value)
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, r.read(), {k.lower(): v for k, v in r.headers.items()}
    except urllib.error.HTTPError as e:
        return e.code, e.read(), {k.lower(): v for k, v in e.headers.items()}
    except Exception as e:  # noqa: BLE001
        return None, str(e).encode(), {}


def summarise(status, raw, headers, want_headers):
    """One step's answer, in the terms the two stacks can be compared on."""
    out = {"status": status, "len": len(raw)}
    try:
        out["body"] = json.loads(raw)
    except Exception:  # noqa: BLE001
        text = raw.decode("utf-8", "replace")
        # A playlist is worth reading; a segment is not.
        out["body"] = text if raw[:7] == b"#EXTM3U" or len(raw) < 200 else hashlib.sha256(raw).hexdigest()[:16]
    for name in want_headers:
        out[name] = headers.get(name)
    return out


def segment_push(n, size=10, camera=CAMERA, key=NODE_KEY):
    return ("push", f"/api/cameras/{camera}/push-segment?filename=segment_{n:05}.ts",
            {"body": b"x" * size, "key": key})


def segment_get(n, camera=CAMERA):
    return ("get", f"/api/cameras/{camera}/segment/segment_{n:05}.ts", {"token": True})


PLAYLIST = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-CODECS:avc1.64001f\n#EXTINF:1.0,\nsegment_00001.ts\n"

SCENARIOS = [
    # --- the round trip ------------------------------------------------
    ("push then read back", [
        segment_push(1),
        segment_get(1),
    ]),
    ("playlist then stream.m3u8", [
        ("playlist", f"/api/cameras/{CAMERA}/playlist", {"body": PLAYLIST, "key": NODE_KEY}),
        ("m3u8", f"/api/cameras/{CAMERA}/stream.m3u8", {"token": True}),
    ]),
    # Every shape of segment URI the rewriter has to normalise, read
    # back through the cache so the comparison is on what a player gets.
    ("playlist rewriting", [
        ("playlist", f"/api/cameras/{CAMERA}/playlist", {
            "body": b"#EXTM3U\n/var/hls/segment_00001.ts\nC:\\hls\\segment_00002.ts\n"
                    b"segment_00003.ts  \r\n#EXT-X-CODECS:avc1\n#segment_00004.ts\n"
                    b"not_a_segment.ts\nsegment_x.ts\n",
            "key": NODE_KEY}),
        ("m3u8", f"/api/cameras/{CAMERA}/stream.m3u8", {"token": True}),
    ]),
    ("no playlist pushed", [
        ("m3u8", f"/api/cameras/{CAMERA}/stream.m3u8", {"token": True}),
    ]),
    ("segment never pushed", [
        segment_get(9),
    ]),

    # --- eviction ------------------------------------------------------
    # One past the per-camera cap: the oldest by filename goes, and the
    # count in the response says so at every step.
    ("per-camera cap", [
        *[segment_push(n) for n in range(1, MAX_PER_CAMERA + 2)],
        segment_get(1),
        segment_get(MAX_PER_CAMERA + 1),
    ]),
    # A re-push of a filename already cached overwrites rather than
    # counting twice.
    ("re-push the same filename", [
        segment_push(1, size=10),
        segment_push(1, size=40),
        segment_get(1),
    ]),
    # Past the global byte ceiling, which evicts oldest-first across
    # cameras and down to 95% of the cap rather than exactly to it.
    # Four megabyte-ish pushes against a three-megabyte ceiling: the
    # two oldest go, and eviction stops at 95% of the cap rather than
    # exactly at it, so the next push does not walk the cache again.
    ("global byte cap", [
        *[segment_push(n, size=1_000_000) for n in range(1, 5)],
        segment_get(1),
        segment_get(2),
        segment_get(4),
    ]),

    # --- refusals ------------------------------------------------------
    ("push with no key", [
        ("push", f"/api/cameras/{CAMERA}/push-segment?filename=segment_00001.ts", {"body": b"x"}),
    ]),
    ("push with a bad key", [
        ("push", f"/api/cameras/{CAMERA}/push-segment?filename=segment_00001.ts",
         {"body": b"x", "key": "nope"}),
    ]),
    ("push to another org's camera", [
        ("push", "/api/cameras/cam-theirs/push-segment?filename=segment_00001.ts",
         {"body": b"x", "key": NODE_KEY}),
    ]),
    # The camera exists, the key is valid, and they belong to the same
    # org — but not to each other. The push path matches on all three.
    ("push to another node's camera", [
        ("push", f"/api/cameras/{OTHER_NODES_CAMERA}/push-segment?filename=segment_00001.ts",
         {"body": b"x", "key": NODE_KEY}),
    ]),
    # Over the plan's camera cap: a 402 carrying the plan, the cap and
    # the camera's name, so the node can say why instead of retrying.
    ("push to a camera suspended by plan", [
        ("setup", "UPDATE cameras SET disabled_by_plan = true WHERE camera_id = '{cam}'", {"sql": True}),
        segment_push(1),
        segment_get(1),
    ]),
    ("push with no filename", [
        ("push", f"/api/cameras/{CAMERA}/push-segment", {"body": b"x", "key": NODE_KEY}),
    ]),
    *[(f"push with filename {name!r}", [
        ("push", f"/api/cameras/{CAMERA}/push-segment?filename={name}",
         {"body": b"x", "key": NODE_KEY}),
    ]) for name in [
        "segment_1.ts", "segment_00001.ts", "SEGMENT_1.ts", "segment_.ts",
        "segment_1.tsx", "x%2Fsegment_1.ts", "", "%D9%A3",
        "segment_%D9%A3.ts",        # Arabic-Indic digits: Python's \d takes them
        "segment_1.ts%0A",          # a trailing newline, which $ also takes
        "segment_1.ts%0Ax",
    ]],
    # The cap is on the declared length first, so an honest oversized
    # client is refused before its bytes are read.
    ("push over the size cap", [
        ("push", f"/api/cameras/{CAMERA}/push-segment?filename=segment_00001.ts",
         {"body": b"x" * (2 * 1024 * 1024 + 1), "key": NODE_KEY}),
    ]),
    # A malformed Content-Length is not sent here: uvicorn and hyper
    # both reject it as a protocol error before the handler, with
    # different bodies, so the case would compare two web servers rather
    # than two ports. It also means the Python's own "Invalid
    # Content-Length header" branch cannot be reached through HTTP at
    # all. Recorded in expected_divergences.md.
    ("push an empty body", [
        ("push", f"/api/cameras/{CAMERA}/push-segment?filename=segment_00001.ts",
         {"body": b"", "key": NODE_KEY}),
        segment_get(1),
    ]),

    # --- playlist refusals ---------------------------------------------
    ("playlist with no key", [
        ("playlist", f"/api/cameras/{CAMERA}/playlist", {"body": PLAYLIST}),
    ]),
    ("playlist for another org's camera", [
        ("playlist", "/api/cameras/cam-theirs/playlist", {"body": PLAYLIST, "key": NODE_KEY}),
    ]),
    # The decode error is put straight into the 400, so the message is
    # CPython's: which byte, which position, which of the three reasons.
    *[(f"playlist with invalid utf-8 {label}", [
        ("playlist", f"/api/cameras/{CAMERA}/playlist", {"body": body, "key": NODE_KEY}),
    ]) for label, body in [
        ("a bare continuation byte", b"#EXTM3U\n\x80\n"),
        ("a truncated character", b"#EXTM3U\n\xe2\x82"),
        ("a bad continuation", b"#EXTM3U\n\xe2\x28\xa1"),
        ("an overlong form", b"\xc0\x80"),
        ("a surrogate", b"\xed\xa0\x80"),
    ]],
    ("playlist over the size cap", [
        ("playlist", f"/api/cameras/{CAMERA}/playlist",
         {"body": b"#" * (64 * 1024 + 1), "key": NODE_KEY}),
    ]),
    ("playlist at exactly the size cap", [
        ("playlist", f"/api/cameras/{CAMERA}/playlist",
         {"body": b"#" + b"x" * (64 * 1024 - 1), "key": NODE_KEY}),
    ]),
    ("empty playlist", [
        ("playlist", f"/api/cameras/{CAMERA}/playlist", {"body": b"", "key": NODE_KEY}),
        ("m3u8", f"/api/cameras/{CAMERA}/stream.m3u8", {"token": True}),
    ]),

    # --- the browser side ----------------------------------------------
    ("stream.m3u8 for a camera that is not ours", [
        ("m3u8", "/api/cameras/cam-theirs/stream.m3u8", {"token": True}),
    ]),
    ("stream.m3u8 for a camera with no node", [
        ("m3u8", f"/api/cameras/{NODELESS_CAMERA}/stream.m3u8", {"token": True}),
    ]),
    ("stream.m3u8 unauthenticated", [
        ("m3u8", f"/api/cameras/{CAMERA}/stream.m3u8", {}),
    ]),
    ("segment for a camera that is not ours", [
        ("get", "/api/cameras/cam-theirs/segment/segment_00001.ts", {"token": True}),
    ]),
    *[(f"segment named {name!r}", [
        ("get", f"/api/cameras/{CAMERA}/segment/{name}", {"token": True}),
    ]) for name in ["segment_1.ts", "nope.ts", "segment_.ts", "..%2Fetc%2Fpasswd", "%D9%A3"]],
    # The access log is written once per user and camera per five
    # minutes however often the player polls, and the row records the
    # node's integer id as a string.
    ("three polls write one access log", [
        ("playlist", f"/api/cameras/{CAMERA}/playlist", {"body": PLAYLIST, "key": NODE_KEY}),
        ("m3u8", f"/api/cameras/{CAMERA}/stream.m3u8", {"token": True}),
        ("m3u8", f"/api/cameras/{CAMERA}/stream.m3u8", {"token": True}),
        ("m3u8", f"/api/cameras/{CAMERA}/stream.m3u8", {"token": True}),
        ("count", "SELECT count(*)::text FROM stream_access_logs WHERE camera_id = '{cam}'"
                  " AND accessed_at > now()::timestamp - interval '1 minute'", {"sql": True}),
    ]),
]

HEADERS_OF_INTEREST = {
    "m3u8": ["content-type", "cache-control", "pragma"],
    "get": ["content-type", "cache-control", "retry-after"],
    "push": ["content-type"],
    "playlist": ["content-type"],
    "count": [],
    "setup": [],
}


def scenario_camera(name):
    """A stable per-scenario camera id, the same on both tiers."""
    slug = re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")[:40]
    return f"cam-hls-{slug}"


def run_scenario(base, steps, camera):
    reseed()
    # Attached to node-aaaa1111, which is what NODE_KEY authenticates.
    psql(
        "INSERT INTO cameras (camera_id, org_id, node_id, name, node_type, status,"
        " capabilities, last_seen, created_at, updated_at, disabled_by_plan,"
        " continuous_24_7, scheduled_recording)"
        " SELECT '" + camera + "', 'self-host', id, 'HLS Scenario', 'rtsp', 'streaming',"
        " 'streaming,motion', now()::timestamp, now()::timestamp, now()::timestamp,"
        " false, false, false FROM camera_nodes WHERE node_id = 'node-aaaa1111';"
    )
    transcript = []
    for kind, target, options in steps:
        target = target.replace("{cam}", camera)
        if options.get("sql"):
            proc = subprocess.run(
                ["docker", "exec", "-i", PG_CONTAINER, "psql", "-U", "cc", "-d", "cc", "-tAc", target],
                capture_output=True, timeout=60, check=False,
            )
            if proc.returncode != 0:
                raise FixtureError(f"step SQL failed: {proc.stderr.decode()[:200]}")
            # A `setup` step changes the fixture and is not compared; a
            # `count` step is the assertion itself.
            if kind != "setup":
                transcript.append({"sql": proc.stdout.decode().strip()})
            continue
        method = "GET" if kind in ("m3u8", "get") else "POST"
        status, raw, headers = request(
            base, method, target,
            body=options.get("body"), key=options.get("key"),
            token=options.get("token", False), headers=options.get("headers"),
        )
        transcript.append(summarise(status, raw, headers, HEADERS_OF_INTEREST[kind]))
    return transcript


def main() -> int:
    if not TOKEN:
        print(__doc__)
        return 64

    wanted = [w for w in DIFF_ONLY.split("|") if w]
    scenarios = [s for s in SCENARIOS if not wanted or any(w in s[0] for w in wanted)]

    same = bad = 0
    for name, steps in scenarios:
        camera = scenario_camera(name)
        python = run_scenario(PYTHON_BASE, steps, camera)
        rust = run_scenario(RUST_BASE, steps, camera)
        if python == rust:
            same += 1
            if VERBOSE:
                print(f"  ok      {name}")
            continue
        bad += 1
        print(f"  DIFFER  {name}")
        for i, (p, r) in enumerate(zip(python, rust)):
            if p != r:
                for key in sorted(set(p) | set(r)):
                    if p.get(key) != r.get(key):
                        print(f"            step {i} {key}: python={p.get(key)!r} rust={r.get(key)!r}")
        if len(python) != len(rust):
            print(f"            {len(python)} steps in python, {len(rust)} in rust")

    total = same + bad
    scope = f" [DIFF_ONLY={DIFF_ONLY!r}: {total} of {len(SCENARIOS)} scenarios]" if wanted else ""
    print(f"\n{same}/{total} identical, {bad} differing{scope}")
    return 1 if bad else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except FixtureError as exc:
        print(f"FIXTURE ERROR — this run proves nothing:\n  {exc}")
        sys.exit(2)
