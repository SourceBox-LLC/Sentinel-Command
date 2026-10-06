#!/usr/bin/env bash
#
# Round trip through the cloud mirror, across both databases.
#
# `sync.rs` pushes rows up; `sentinel-restore-from-cloud` pulls them back
# down. Each is one program per driver, and the mirror in between is
# shared, so there are four ways through and a restore is only a backup
# if all four give back what went in:
#
#     pushed by      restored into
#     PostgreSQL  →  PostgreSQL     the hosted-shaped path
#     PostgreSQL  →  SQLite         moving an install onto SQLite
#     SQLite      →  SQLite         a self-hosted install recovering
#     SQLite      →  PostgreSQL     moving an install onto PostgreSQL
#
# What is compared: for every table the mirror received, every mirrored
# column of every row, source against restored. (Not mirrored, by
# design, and so not compared: `camera_nodes.api_key_hash` and
# `incident_evidence.data`.)
#
# Usage: tests/differential/dialect_restore.sh [-v]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RS="$(cd "$HERE/../.." && pwd)"
BIN="$RS/target/tier-bin"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
PG_PORT="${PG_HOST_PORT:-15434}"
SRC_PG="postgresql://cc:cc@127.0.0.1:$PG_PORT/cc"
DST_DB="cc_restore"
DST_PG="postgresql://cc:cc@127.0.0.1:$PG_PORT/$DST_DB"
SRC_LITE="$RS/target/dialect-restore-src.db"
DST_LITE="$RS/target/dialect-restore-dst.db"
SYNC_URL=http://127.0.0.1:18091; LICENSE_URL=http://127.0.0.1:18099; CLERK_URL=http://127.0.0.1:18089
VERBOSE="${1:-}"

for port in 8000 8001; do
    if curl -fsS -m 2 "http://127.0.0.1:$port/api/health" >/dev/null 2>&1; then
        echo "REFUSING: a tier is up on :$port. Stop it: tests/differential/dialect_run.sh stop" >&2; exit 2
    fi
done

mkdir -p "$BIN"
echo "building both drivers..."
( cd "$RS"
  cargo build --quiet --example loops_probe --bin sentinel-restore-from-cloud
  cp target/debug/examples/loops_probe "$BIN/loops_probe-pg"
  cp target/debug/sentinel-restore-from-cloud "$BIN/restore-pg"
  cargo build --quiet --features sqlite --example loops_probe --bin sentinel-restore-from-cloud
  cp target/debug/examples/loops_probe "$BIN/loops_probe-sqlite"
  cp target/debug/sentinel-restore-from-cloud "$BIN/restore-sqlite" )

PIDS=()
fake() {
    local holder; holder="$(ss -lptnH "sport = :$2" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1 || true)"
    [[ -n "$holder" ]] && { kill "$holder" 2>/dev/null || true; sleep 0.3; }
    python3 "$HERE/$1" --port "$2" >/dev/null 2>&1 &
    PIDS+=($!)
    for _ in $(seq 25); do curl -fsS -m 1 $3 >/dev/null 2>&1 && return 0; sleep 0.2; done
    echo "REFUSING: $1 did not come up" >&2; exit 2
}
cleanup() {
    kill "${PIDS[@]}" 2>/dev/null || true
    docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q < "$HERE/loops_teardown.sql" >/dev/null 2>&1 || true
    docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q < "$HERE/seed_cameras.sql" >/dev/null 2>&1 || true
}
trap cleanup EXIT
fake fake_license.py 18099 "-X POST $LICENSE_URL/scenario/valid/v1/licenses/check-in"
fake fake_clerk.py 18089 "$CLERK_URL/__calls"
fake fake_sync.py 18091 "$SYNC_URL/__pushes"

schema_lite() {
    rm -f "$1" "$1-wal" "$1-shm"
    python3 - "$RS/migrations-sqlite/0001_adopt_python_schema.sql" "$1" <<'PY'
import sqlite3, sys
con = sqlite3.connect(sys.argv[2]); con.executescript(open(sys.argv[1]).read()); con.close()
PY
}
fresh_pg() {
    docker exec "$PG_CONTAINER" psql -U cc -d postgres -q -c "DROP DATABASE IF EXISTS $DST_DB" -c "CREATE DATABASE $DST_DB"
    docker exec -i "$PG_CONTAINER" psql -U cc -d "$DST_DB" -q < "$RS/migrations/0001_adopt_production_schema.sql"
}

# The source: the loops fixture, in PostgreSQL and copied to SQLite, so
# both pushers start from the same rows.
docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q < "$HERE/seed_cameras.sql" >/dev/null 2>&1
docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q < "$HERE/loops_fixture.sql" >/dev/null 2>&1
schema_lite "$SRC_LITE"
python3 - "$SRC_LITE" <<PY
import sys; sys.path.insert(0, "$HERE")
import dialect; dialect.copy_from_postgres(sys.argv[1])
PY

bad=0; ways=0
for from in pg sqlite; do
  for into in pg sqlite; do
    [[ "$from" == pg ]] && src_url="$SRC_PG" || src_url="sqlite:///$SRC_LITE"
    if [[ "$into" == pg ]]; then fresh_pg; dst_url="$DST_PG"; else schema_lite "$DST_LITE"; dst_url="sqlite:///$DST_LITE"; fi

    # Push. The probe's sync body ends with one table scripted to fail,
    # so what the mirror holds afterwards is every pushable table but
    # that one — which is fine: whatever is there is what is restored.
    "$BIN/loops_probe-$from" --db "$src_url" --body sync --license-url "$LICENSE_URL" \
        --clerk-url "$CLERK_URL" --sync-url "$SYNC_URL" >/dev/null 2>"$RS/target/dialect-restore.err" \
        || { echo "push from $from failed:"; tail -5 "$RS/target/dialect-restore.err"; exit 2; }

    DATABASE_URL="$dst_url" SENTINEL_SYNC_SERVICE_URL="$SYNC_URL" SENTINEL_LICENSE_KEY=harness-licence-key \
        AUTH_PROVIDER=local APP_SECRET_KEY=differential-test-secret-not-a-real-key \
        "$BIN/restore-$into" > "$RS/target/dialect-restore.out" 2>&1 \
        || { echo "restore into $into failed:"; tail -8 "$RS/target/dialect-restore.out"; exit 2; }

    if python3 - "$from" "$into" "$SRC_LITE" "$DST_LITE" "$DST_DB" "$VERBOSE" <<PY
import json, subprocess, sys, urllib.request
sys.path.insert(0, "$HERE")
import dialect
from diffutil import value_diff

frm, into, src_lite, dst_lite, dst_db, verbose = sys.argv[1:7]
pushes = json.load(urllib.request.urlopen("$SYNC_URL/__pushes"))
tables = sorted({p["table"] for p in pushes if p.get("rows")})
DENIED = {"camera_nodes": {"api_key_hash"}, "incident_evidence": {"data"}}


def pg_rows(database, table):
    out = subprocess.run(
        ["docker", "exec", "-i", "$PG_CONTAINER", "psql", "-U", "cc", "-d", database, "-tAq", "-c",
         f"SELECT COALESCE(json_agg(x), '[]'::json) FROM (SELECT * FROM {table} ORDER BY 1) x"],
        capture_output=True, text=True, timeout=60, check=True).stdout
    return json.loads(out)


def rows(side, table):
    if side == "source":
        got = pg_rows("cc", table) if frm == "pg" else dialect.snapshot(src_lite, [table])[table]
    else:
        got = pg_rows(dst_db, table) if into == "pg" else dialect.snapshot(dst_lite, [table])[table]
    drop = DENIED.get(table, set())
    return [{k: v for k, v in r.items() if k not in drop} for r in got]


if not tables:
    print(f"  REFUSING  {frm} → {into}: the mirror received nothing"); sys.exit(2)
bad, total = 0, 0
pushed_ids = {t: {str(r["id"]) for p in pushes if p["table"] == t for r in p["rows"]} for t in tables}
for table in tables:
    # Only what was pushed: a source row the sync skipped was never in
    # the mirror to restore.
    want = [r for r in rows("source", table) if str(r["id"]) in pushed_ids[table]]
    got = rows("restored", table)
    total += len(want)
    if want != got:
        bad += 1
        print(f"  DIFFER  {frm} → {into} / {table}: {len(want)} pushed, {len(got)} restored")
        for line in value_diff(got, want)[:6]:
            print(f"            {line}  (python=restored, rust=source)")
if total < 50:
    print(f"  REFUSING  {frm} → {into}: only {total} row(s) made the round trip"); sys.exit(2)
if not bad:
    print(f"  ok      {frm:6} → {into:6}  {total} row(s) across {len(tables)} table(s)")
sys.exit(1 if bad else 0)
PY
    then :; else rc=$?; [[ "$rc" == 2 ]] && exit 2; bad=$((bad + 1)); fi
    ways=$((ways + 1))
  done
done
echo
echo "$((ways - bad))/$ways identical, $bad differing"
[[ "$bad" == 0 ]]
