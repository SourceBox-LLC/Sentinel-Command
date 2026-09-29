#!/usr/bin/env bash
#
# The background-loop differential: the two loop BODIES, side by side.
#
# Neither loop has an HTTP surface, so this is a probe pair rather than a
# request differential. Each body is run against a freshly seeded
# database on each side and both the summary it returns and the rows it
# left behind are compared — a body that reports the right counts while
# deleting the wrong rows is what a count-only check cannot see.
#
# Needs the Postgres container, NOT the two tiers: nothing here speaks
# HTTP. It does need Python's tier to be STOPPED, or at least not
# sweeping — see the note below.
#
# Usage: tests/differential/loops_run.sh [-v]
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RS="$(cd "$HERE/../.." && pwd)"
REPO="$(cd "$RS/.." && pwd)"
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
PG_URL="${PROBE_DATABASE_URL:-postgresql://cc:cc@127.0.0.1:15434/cc}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if ! docker exec "$PG_CONTAINER" true 2>/dev/null; then
    echo "no postgres container '$PG_CONTAINER' — start it first" >&2
    exit 2
fi

# Python's own `_offline_sweep_loop` runs every 30 seconds inside its
# tier, against this same database. It is a no-op on the MAIN fixture
# (every 'online' node there has a NULL last_seen, which the sweep
# skips), but this harness seeds rows it is NOT a no-op on — so a tier
# left running would flip them between the seed and the probe, and
# whichever probe ran second would find the work already done and report
# zero. That reads as a difference in the port.
if curl -fsS -m 2 "http://127.0.0.1:8001/api/health" >/dev/null 2>&1; then
    echo "REFUSING: Python's tier is up on :8001 and its offline sweep runs every"
    echo "30s against this database. Stop it first (tests/differential/tiers.sh stop)"
    echo "or this harness is racing it."
    exit 2
fi

# Built before the first probe rather than between them: a release build
# in the middle would put minutes between the two sides' clocks, and the
# cleanup compares ages in days — close enough to a boundary and the two
# sides disagree for that reason alone.
( cd "$RS" && cargo build --quiet --example loops_probe ) || exit 2

seed() {
    docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q < "$HERE/seed_cameras.sql" >/dev/null 2>&1
    # The loop fixture goes on top: seed_cameras.sql cannot exercise
    # either body. See the header of loops_fixture.sql.
    docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q < "$HERE/loops_fixture.sql" 2>&1 \
        | grep -v '^$' && return 1
    return 0
}

bad=0
compared=0
for body in sweep cleanup; do
    seed || { echo "REFUSING: the loop fixture did not apply cleanly"; exit 2; }
    "$PYTHON" "$HERE/py_loops_probe.py" --db "postgresql+psycopg://${PG_URL#postgresql://}" \
        --body "$body" 2>/dev/null | grep '^{' > "$WORK/py-$body.jsonl"

    seed || { echo "REFUSING: the loop fixture did not apply cleanly"; exit 2; }
    ( cd "$RS" && cargo run --quiet --example loops_probe -- \
        --db "$PG_URL" --body "$body" ) > "$WORK/rs-$body.jsonl"

    # Compared as parsed JSON per line: Python's json.dumps and
    # serde_json space their separators differently, and that is not a
    # finding.
    if "$PYTHON" - "$WORK/py-$body.jsonl" "$WORK/rs-$body.jsonl" "$body" <<'PY'
import json, sys

py, rs, body = sys.argv[1], sys.argv[2], sys.argv[3]
a = [json.loads(line) for line in open(py) if line.strip()]
b = [json.loads(line) for line in open(rs) if line.strip()]
if not a or not b:
    print(f"  REFUSING  {body}: one side produced nothing "
          f"(python {len(a)} line(s), rust {len(b)})")
    sys.exit(2)
if len(a) != len(b):
    print(f"  DIFFER  {body}: python printed {len(a)} line(s), rust {len(b)}")
    sys.exit(1)
bad = 0
for left, right in zip(a, b):
    if left == right:
        continue
    bad += 1
    label = left.get("rows") or "summary"
    print(f"  DIFFER  {body} / {label}")
    print(f"            python {json.dumps(left, sort_keys=True)[:400]}")
    print(f"            rust   {json.dumps(right, sort_keys=True)[:400]}")
sys.exit(1 if bad else 0)
PY
    then
        printf '  ok      %s\n' "$body"
    else
        rc=$?
        [ "$rc" = 2 ] && exit 2
        bad=$((bad + 1))
    fi
    compared=$((compared + 1))
done

# A run where both bodies did nothing would agree perfectly and prove
# nothing. Both summaries have to report work.
flips=$("$PYTHON" -c "
import json,sys
d=json.loads(open('$WORK/rs-sweep.jsonl').readline())['summary']
print(d['nodes_flipped'] + d['cameras_flipped'])
" 2>/dev/null || echo 0)
deleted=$("$PYTHON" -c "
import json,sys
print(json.loads(open('$WORK/rs-cleanup.jsonl').readline())['summary']['total_deleted'])
" 2>/dev/null || echo 0)
echo
echo "fixture: the sweep flipped $flips row(s), the cleanup deleted $deleted"
if [ "$flips" -lt 4 ] || [ "$deleted" -lt 20 ]; then
    echo "FIXTURE TOO THIN — both bodies must actually do work, or two no-ops agree"
    exit 2
fi

echo "$((compared - bad))/$compared identical, $bad differing"
[ "$bad" = 0 ] || exit 1
