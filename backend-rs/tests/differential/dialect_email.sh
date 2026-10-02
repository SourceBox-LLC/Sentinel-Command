#!/usr/bin/env bash
#
# The dialect differential for the email worker: the probe that verified
# it against the Python (email_run.sh), run once per driver and compared.
#
# The worker is the heaviest writer with no HTTP surface of its own — it
# claims a batch with one UPDATE over a list of ids, which is the
# statement the two databases spell most differently (`= ANY($1)` with an
# array against `IN (SELECT value FROM json_each($1))`).
#
# Usage: tests/differential/dialect_email.sh [-v]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RS="$(cd "$HERE/../.." && pwd)"
BIN="$RS/target/tier-bin"
PORT="${FAKE_RESEND_PORT:-18095}"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
DB_NAME="cc_email"
PG_URL="postgresql://cc:cc@127.0.0.1:${PG_HOST_PORT:-15434}/$DB_NAME"
LITE="$RS/target/dialect-email.db"
OUT="$(mktemp -d)"

if ! docker exec "$PG_CONTAINER" psql -U cc -d postgres -tAc \
        "SELECT 1 FROM pg_database WHERE datname='$DB_NAME'" | grep -q 1; then
    docker exec "$PG_CONTAINER" psql -U cc -d postgres -q -c "CREATE DATABASE $DB_NAME"
    docker exec -i "$PG_CONTAINER" psql -U cc -d "$DB_NAME" -q \
        < "$RS/migrations/0001_adopt_production_schema.sql"
fi
rm -f "$LITE" "$LITE-wal" "$LITE-shm"
python3 - "$RS/migrations-sqlite/0001_adopt_python_schema.sql" "$LITE" <<'PY'
import sqlite3, sys
con = sqlite3.connect(sys.argv[2]); con.executescript(open(sys.argv[1]).read()); con.close()
PY

mkdir -p "$BIN"
echo "building the probe for both drivers..."
( cd "$RS" && cargo build --quiet --example email_probe \
    && cp target/debug/examples/email_probe "$BIN/email_probe-pg" \
    && cargo build --quiet --features sqlite --example email_probe \
    && cp target/debug/examples/email_probe "$BIN/email_probe-sqlite" )

pid="$(ss -lntpH "sport = :$PORT" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1 || true)"
[[ -n "${pid:-}" ]] && { kill "$pid" 2>/dev/null || true; sleep 0.5; }
python3 "$HERE/fake_resend.py" --port "$PORT" >"$OUT/fake.log" 2>&1 &
fake=$!
trap 'kill "$fake" 2>/dev/null; rm -rf "$OUT"' EXIT
for _ in $(seq 50); do curl -fsS -m 1 "http://127.0.0.1:$PORT/" >/dev/null 2>&1 && break; sleep 0.2; done
curl -fsS -m 2 "http://127.0.0.1:$PORT/" >/dev/null || { echo "fake resend never came up" >&2; exit 2; }

run() {  # label binary url
    curl -fsS -m 2 "http://127.0.0.1:$PORT/__reset" >/dev/null
    ( cd "$RS" && "$2" --resend "http://127.0.0.1:$PORT" --db "$3" ) >"$OUT/$1.jsonl" 2>"$OUT/$1.err" || {
        echo "$1 probe failed:"; tail -15 "$OUT/$1.err"; exit 2; }
}
run pg "$BIN/email_probe-pg" "$PG_URL"
run sqlite "$BIN/email_probe-sqlite" "sqlite:///$LITE"

python3 - "$OUT/pg.jsonl" "$OUT/sqlite.jsonl" "${1:-}" "$HERE" <<'PY'
import json, sys
sys.path.insert(0, sys.argv[4])
from diffutil import value_diff

pg = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
lt = [json.loads(line) for line in open(sys.argv[2]) if line.strip()]
verbose = sys.argv[3] == "-v"
if not pg or not lt:
    print("REFUSING: one of the probes produced nothing"); sys.exit(2)
if len(pg) != len(lt):
    print(f"DIFFER: postgres printed {len(pg)} scenario(s), sqlite {len(lt)}"); sys.exit(1)
bad = 0
for left, right in zip(pg, lt):
    name = left.get("scenario") or left.get("name") or "?"
    if left == right:
        if verbose:
            print(f"  ok      {name}")
        continue
    bad += 1
    print(f"  DIFFER  {name}")
    for line in value_diff(right, left)[:10]:
        print(f"            {line}  (python=sqlite, rust=postgres)")
print(f"\n{len(pg) - bad}/{len(pg)} identical, {bad} differing")
sys.exit(1 if bad else 0)
PY
