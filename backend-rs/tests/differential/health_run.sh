#!/usr/bin/env bash
#
# The health-probe differential: the real Python `health_probes.py`
# against the Rust `health_probes` module, both with every input
# supplied.
#
# The HTTP differential has the two endpoints and compares them, but it
# cannot reach most of what they report:
#
#   * the mutator restarts the tier before every run, so the process is
#     always inside its thirty-second startup grace — the states past
#     it are unreachable;
#   * the harness environment has email on, a valid licence and a
#     healthy disk, so `warn`, `critical`, `disabled` and
#     `unconfigured` mostly never occur;
#   * and the real filesystem moves between the two calls, so a live
#     reading is either flaky or normalised into invisibility.
#
# Seventeen mutations survived a full HTTP run for those reasons, and
# not one of them was a difference of opinion about the code. This
# drives the probes directly instead, with the disk reading, the
# worker's tick age, the uptime, the config flags and the licence rows
# all supplied.
#
# A database of its own, not the differential's `cc`: the licence rows
# are the fixture here and the write differential compares `settings`
# literally.
#
# Usage: tests/differential/health_run.sh [-v]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
RS="$REPO/backend-rs"
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
PG_HOST_PORT="${PG_HOST_PORT:-15434}"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

DB_NAME="cc_health"
PY_URL="postgresql+psycopg://cc:cc@127.0.0.1:$PG_HOST_PORT/$DB_NAME"
RS_URL="postgresql://cc:cc@127.0.0.1:$PG_HOST_PORT/$DB_NAME"

if ! docker exec "$PG_CONTAINER" psql -U cc -d postgres -tAc \
        "SELECT 1 FROM pg_database WHERE datname='$DB_NAME'" | grep -q 1; then
    echo "creating $DB_NAME..."
    docker exec "$PG_CONTAINER" psql -U cc -d postgres -q -c "CREATE DATABASE $DB_NAME"
    docker exec -i "$PG_CONTAINER" psql -U cc -d "$DB_NAME" -q \
        < "$RS/migrations/0001_adopt_production_schema.sql"
fi

echo "python probe..."
PROBE_DATABASE_URL="$PY_URL" "$PYTHON" -u "$HERE/py_health_probe.py" --db "$PY_URL" \
    >"$OUT/py.jsonl" 2>"$OUT/py.err" || {
        echo "python probe failed:"; grep -v '^\[Limiter\]' "$OUT/py.err" | tail -15; exit 2; }

echo "rust probe..."
(cd "$RS" && cargo run --quiet --example health_probe -- --db "$RS_URL") \
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
    for key in ("disk", "email_worker", "clerk", "sentinel_license", "readiness"):
        if a.get(key) != b.get(key):
            print(f"            {key}:")
            print(f"              python={json.dumps(a.get(key), sort_keys=True)[:280]}")
            print(f"              rust=  {json.dumps(b.get(key), sort_keys=True)[:280]}")

# A run where every scenario reported the same status proves nothing.
statuses = {s["disk"]["status"] for s in py} | {s["email_worker"]["status"] for s in py}
statuses |= {s["sentinel_license"]["status"] for s in py}
if len(statuses) < 4:
    print(f"COVERAGE TOO THIN: only {sorted(statuses)} across every probe")
    sys.exit(2)
readiness = {s["readiness"]["ready"] for s in py}
if len(readiness) < 2:
    print("COVERAGE TOO THIN: readiness never differed — no scenario went critical")
    sys.exit(2)

print(f"\n{len(py) - bad}/{len(py)} identical, {bad} differing")
sys.exit(1 if bad else 0)
PYEOF
