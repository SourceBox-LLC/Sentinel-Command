"""Request-body handling through the proxy: correctness and memory.

`push-segment` is the highest-volume route in the service (up to 20/s
per node) and the one place a proxy's body handling has teeth.

`_read_capped_body` in hls.py rejects an oversized push on its
Content-Length before reading it, and says why in its own docstring:
"the lever that makes a 10 GB attempted upload cost zero memory at the
server". A proxy that buffers the body in front of that defeats it
entirely — the bytes land in Rust's memory before Python ever sees the
header.

Measured with `to_bytes(body, usize::MAX)` in the proxy: a single 400 MB
upload took RSS from 21 MB to 731 MB. The machine has 985 MB and a
384 MB segment cache to fit beside it, so two concurrent uploads were an
OOM. The body is streamed now, and this pins that.

Usage: upload_diff.py <token>
"""

import hashlib
import os
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

RUST = "http://127.0.0.1:8000"
PYTHON = "http://127.0.0.1:8001"
NODE_KEY = "test-node-key"
CAMERA = "cam-live"

# A single upload must not cost more than this much RSS. The real cap is
# 2 MB (SEGMENT_PUSH_MAX_BYTES); the headroom here is for allocator
# behaviour, not for buffering. Buffering 400 MB blows through it by an
# order of magnitude.
MAX_RSS_GROWTH_MB = 64


def curl_post(base, path, body_path, headers):
    """POST a file with curl and return the status code as a string.

    Not urllib: an oversized push is answered 413 and the connection is
    closed while the client is still sending, which urllib surfaces as
    ConnectionResetError rather than as the response. That reset is the
    *desired* behaviour — the server refusing without reading — so the
    probe has to be able to see past it. curl reports the status.
    """
    cmd = ["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}",
           "--max-time", "120", "-X", "POST", "--data-binary", f"@{body_path}"]
    for k, v in headers.items():
        cmd += ["-H", f"{k}: {v}"]
    cmd.append(base + path)
    out = subprocess.run(cmd, capture_output=True, text=True).stdout.strip()
    return out or "<no response>"


def post(base, path, body, headers):
    req = urllib.request.Request(base + path, method="POST", data=body)
    for k, v in headers.items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def get(base, path, headers):
    req = urllib.request.Request(base + path)
    for k, v in headers.items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def rust_pid():
    out = subprocess.run(["ss", "-ltnp"], capture_output=True, text=True).stdout
    for line in out.splitlines():
        if ":8000" in line and "pid=" in line:
            return int(line.split("pid=")[1].split(",")[0])
    return None


def rss_kb(pid):
    try:
        with open(f"/proc/{pid}/status") as fh:
            for line in fh:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return 0


def main():
    token = sys.argv[1]
    bad = 0

    # --- 1. a real segment survives the hop byte for byte -------------
    payload = os.urandom(300 * 1024)
    digest = hashlib.sha256(payload).hexdigest()
    for name, base in (("python", PYTHON), ("rust", RUST)):
        fn = f"segment_{1 if name == 'python' else 2}.ts"
        st, _ = post(base, f"/api/cameras/{CAMERA}/push-segment?filename={fn}",
                     payload, {"X-Node-API-Key": NODE_KEY,
                               "Content-Type": "video/mp2t"})
        gst, got = get(base, f"/api/cameras/{CAMERA}/segment/{fn}",
                       {"Authorization": f"Bearer {token}"})
        ok = st == 200 and gst == 200 and hashlib.sha256(got).hexdigest() == digest
        print(f"  {'ok    ' if ok else 'FAIL  '} {name:<7} push={st} fetch={gst} "
              f"{len(got)} bytes {'identical' if ok else 'MISMATCH'}")
        if not ok:
            bad += 1

    # --- 2. an oversized push is refused, on both --------------------
    over_path = "/tmp/_upload_probe_over.bin"
    with open(over_path, "wb") as fh:
        fh.write(b"\0" * (8 * 1024 * 1024))
    codes = {}
    for name, base in (("python", PYTHON), ("rust", RUST)):
        codes[name] = curl_post(
            base, f"/api/cameras/{CAMERA}/push-segment?filename=segment_99.ts",
            over_path, {"X-Node-API-Key": NODE_KEY, "Content-Type": "video/mp2t"})
    os.unlink(over_path)
    same = codes["python"] == codes["rust"] == "413"
    print(f"  {'ok    ' if same else 'FAIL  '} 8MB push (cap 2MB) "
          f"python={codes['python']} rust={codes['rust']}")
    if not same:
        bad += 1

    # --- 3. the memory ceiling ---------------------------------------
    pid = rust_pid()
    if pid is None:
        print("  SKIP   could not find the Rust pid; memory ceiling unchecked")
        return 2

    baseline = rss_kb(pid)
    peak = [baseline]
    stop = threading.Event()

    def sample():
        while not stop.is_set():
            peak[0] = max(peak[0], rss_kb(pid))
            time.sleep(0.02)

    sampler = threading.Thread(target=sample, daemon=True)
    sampler.start()
    huge_path = "/tmp/_upload_probe_huge.bin"
    with open(huge_path, "wb") as fh:
        fh.write(b"\0" * (400 * 1024 * 1024))
    curl_post(RUST, f"/api/cameras/{CAMERA}/push-segment?filename=segment_98.ts",
              huge_path, {"X-Node-API-Key": NODE_KEY, "Content-Type": "video/mp2t"})
    os.unlink(huge_path)
    stop.set()
    sampler.join(timeout=2)

    growth_mb = (peak[0] - baseline) / 1024
    within = growth_mb < MAX_RSS_GROWTH_MB
    print(f"  {'ok    ' if within else 'FAIL  '} 400MB upload grew RSS by "
          f"{growth_mb:.0f} MB (ceiling {MAX_RSS_GROWTH_MB} MB)")
    if not within:
        print("           the proxy is buffering request bodies again — stream them")
        bad += 1

    print(f"\n{'all upload checks passed' if not bad else f'{bad} check(s) failed'}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
