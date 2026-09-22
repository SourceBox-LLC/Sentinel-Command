#!/usr/bin/env bash
#
# The email-worker differential: the real Python `email_worker.py`
# against the Rust `email_worker` module, both over one fake Resend and
# one Postgres.
#
# Why this is not part of the HTTP differential: the worker has no HTTP
# surface. It is a loop, and a write case could only reach it by waiting
# for a timer — which is the race tiers.sh pins every other loop out of
# the way to avoid. `run_one_tick` is a function over a session on both
# sides, so both are driven directly.
#
# The fake Resend is what makes this possible at all, and it is possible
# because `resend.api_url` comes from the environment. Clerk's SDK has
# no equivalent, which is why the webhook's member-limit call has to be
# marked untestable while this is not.
#
# A database of its own, not the differential's `cc`: the write
# differential snapshots `email_outbox` and `email_log` and compares
# them literally, so probe rows landing there would read as a phantom
# side effect on every write case.
#
# Usage: tests/differential/email_run.sh [-v]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
RS="$REPO/backend-rs"
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"
PORT="${FAKE_RESEND_PORT:-18095}"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
PG_HOST_PORT="${PG_HOST_PORT:-15434}"
OUT="$(mktemp -d)"

DB_NAME="cc_email"
PY_URL="postgresql+psycopg://cc:cc@127.0.0.1:$PG_HOST_PORT/$DB_NAME"
RS_URL="postgresql://cc:cc@127.0.0.1:$PG_HOST_PORT/$DB_NAME"

if ! docker exec "$PG_CONTAINER" psql -U cc -d postgres -tAc \
        "SELECT 1 FROM pg_database WHERE datname='$DB_NAME'" | grep -q 1; then
    echo "creating $DB_NAME..."
    docker exec "$PG_CONTAINER" psql -U cc -d postgres -q -c "CREATE DATABASE $DB_NAME"
    docker exec -i "$PG_CONTAINER" psql -U cc -d "$DB_NAME" -q \
        < "$RS/migrations/0001_adopt_production_schema.sql"
fi

started_fake=0
if ! curl -fsS -m 2 "http://127.0.0.1:$PORT/" >/dev/null 2>&1; then
    "$PYTHON" "$HERE/fake_resend.py" --port "$PORT" >"$OUT/fake.log" 2>&1 &
    started_fake=$!
    for _ in $(seq 50); do
        curl -fsS -m 1 "http://127.0.0.1:$PORT/" >/dev/null 2>&1 && break
        sleep 0.2
    done
fi
cleanup() { [[ "$started_fake" != 0 ]] && kill "$started_fake" 2>/dev/null; rm -rf "$OUT"; }
trap cleanup EXIT

# Each probe starts from the same fake state; see fake_resend.py.
curl -fsS -m 2 "http://127.0.0.1:$PORT/__reset" >/dev/null

echo "python probe..."
PROBE_DATABASE_URL="$PY_URL" "$PYTHON" -u "$HERE/py_email_probe.py" \
    --resend "http://127.0.0.1:$PORT" --db "$PY_URL" >"$OUT/py.jsonl" 2>"$OUT/py.err" || {
        echo "python probe failed:"; grep -v '^\[Limiter\]' "$OUT/py.err" | tail -15; exit 2; }

curl -fsS -m 2 "http://127.0.0.1:$PORT/__reset" >/dev/null

echo "rust probe..."
(cd "$RS" && cargo run --quiet --example email_probe -- \
    --resend "http://127.0.0.1:$PORT" --db "$RS_URL") \
    >"$OUT/rs.jsonl" 2>"$OUT/rs.err" || {
        echo "rust probe failed:"; tail -15 "$OUT/rs.err"; exit 2; }

echo
exec "$PYTHON" - "$OUT/py.jsonl" "$OUT/rs.jsonl" "${1:-}" <<'PYEOF'
import json, sys

py = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
rs = [json.loads(line) for line in open(sys.argv[2]) if line.strip()]
verbose = sys.argv[3] == "-v" if len(sys.argv) > 3 else False

if not py or not rs:
    print("REFUSING: one of the probes produced nothing")
    sys.exit(2)
if len(py) != len(rs):
    print(f"scenario count differs: python {len(py)} rust {len(rs)}")
    sys.exit(1)

bad = 0
for a, b in zip(py, rs):
    if a["scenario"] != b["scenario"]:
        print(f"  ORDER   python={a['scenario']} rust={b['scenario']}")
        bad += 1
        continue
    if a == b:
        if verbose:
            print(f"  ok      {a['scenario']}")
        continue
    bad += 1
    print(f"  DIFFER  {a['scenario']}")
    for key in ("summaries", "outbox", "log"):
        if a[key] != b[key]:
            print(f"            {key}:")
            print(f"              python={json.dumps(a[key])[:300]}")
            print(f"              rust=  {json.dumps(b[key])[:300]}")

# A run where every scenario sent nothing proves nothing: the fake
# would answer identically to two stacks that never called it.
touched = sum(s["summaries"][0].get("sent", 0) + s["summaries"][0].get("failed", 0)
              for s in py if s["summaries"])
if touched == 0:
    print("COVERAGE TOO THIN: no scenario sent or failed a single message")
    sys.exit(2)

print(f"\n{len(py) - bad}/{len(py)} identical, {bad} differing")
sys.exit(1 if bad else 0)
PYEOF
