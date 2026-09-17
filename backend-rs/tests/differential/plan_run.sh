#!/usr/bin/env bash
#
# Plan-resolution differential: the real Python `plans.py` against the
# Rust `plans` module, both over one fake Clerk and one Postgres.
#
# Why this is not part of the HTTP differential: `resolve_org_plan`
# opens with
#
#     if settings.is_local_auth():
#         return "self_host"
#
# and both tiers there run AUTH_PROVIDER=local, because that is what
# lets them share one HS256 secret. Every plan lookup in every other
# harness in this directory therefore returns one constant, and the
# entitlement rules, the two in-process caches and the seven-day
# past-due grace have never executed under test. Roughly twenty routes
# gate on this code.
#
# Neither probe goes through HTTP. Reaching this code through a request
# would mean AUTH_PROVIDER=clerk on both tiers, and therefore real
# RS256 session tokens from a Clerk instance neither tier has. The
# entitlement logic needs none of that.
#
# Usage: tests/differential/plan_run.sh [-v]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
RS="$REPO/backend-rs"
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"
PORT="${FAKE_CLERK_PORT:-18080}"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
PG_HOST_PORT="${PG_HOST_PORT:-15434}"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

# A database of its own, not the differential's `cc`. The write
# differential snapshots the whole `settings` table and compares it
# literally, so probe rows landing there would read as a phantom side
# effect on every write case.
DB_NAME="cc_plan"
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
if ! curl -fsS -m 2 "http://127.0.0.1:$PORT/__calls" >/dev/null 2>&1; then
    "$PYTHON" "$HERE/fake_clerk.py" --port "$PORT" >"$OUT/fake.log" 2>&1 &
    started_fake=$!
    for _ in $(seq 50); do
        curl -fsS -m 1 "http://127.0.0.1:$PORT/__calls" >/dev/null 2>&1 && break
        sleep 0.2
    done
fi
cleanup() { [[ "$started_fake" != 0 ]] && kill "$started_fake" 2>/dev/null; rm -rf "$OUT"; }
trap cleanup EXIT

echo "python probe..."
PROBE_DATABASE_URL="$PY_URL" "$PYTHON" -u "$HERE/py_plan_probe.py" \
    --clerk "http://127.0.0.1:$PORT/v1" >"$OUT/py.jsonl" 2>"$OUT/py.err" || {
        echo "python probe failed:"; grep -v '^\[Limiter\]' "$OUT/py.err" | tail -5; exit 2; }
grep -E 'coverage|REFUS' "$OUT/py.err" || true

echo "rust probe..."
(cd "$RS" && cargo run --quiet --example plan_probe -- \
    --clerk "http://127.0.0.1:$PORT/v1" --db "$RS_URL") \
    >"$OUT/rs.jsonl" 2>"$OUT/rs.err" || {
        echo "rust probe failed:"; tail -5 "$OUT/rs.err"; exit 2; }
grep -E 'coverage|REFUS' "$OUT/rs.err" || true

echo
exec "$PYTHON" - "$OUT/py.jsonl" "$OUT/rs.jsonl" "${1:-}" <<'PYEOF'
import json, sys

py = [json.loads(l) for l in open(sys.argv[1])]
rs = [json.loads(l) for l in open(sys.argv[2])]
verbose = sys.argv[3] == "-v" if len(sys.argv) > 3 else False

if not py or not rs:
    print("REFUSING: one of the probes produced nothing")
    sys.exit(2)
if len(py) != len(rs):
    print(f"case count differs: python {len(py)} rust {len(rs)}")
    sys.exit(1)

bad = 0
for a, b in zip(py, rs):
    if a["case"] != b["case"]:
        print(f"  ORDER   python={a['case']} rust={b['case']}")
        bad += 1
        continue
    if a == b:
        if verbose:
            print(f"  ok      {a['case']:32} {a['plan']}")
        continue
    bad += 1
    print(f"  DIFFER  {a['case']}")
    for key in ("plan", "display", "limits"):
        if a[key] != b[key]:
            print(f"            {key}: python={a[key]!r} rust={b[key]!r}")

# A run where every case resolved to the same slug proves nothing: it is
# what a stubbed-out resolver would produce, and what the local-auth
# short-circuit produces.
distinct = {r["plan"] for r in rs}
if len(distinct) < 3:
    print(f"\nCOVERAGE TOO THIN: rust resolved only {sorted(distinct)} across "
          f"{len(rs)} cases — a constant is what a short-circuit looks like")
    sys.exit(2)

print(f"\n{len(py) - bad}/{len(py)} identical, {bad} differing "
      f"({len(distinct)} distinct plans resolved)")
sys.exit(1 if bad else 0)
PYEOF
