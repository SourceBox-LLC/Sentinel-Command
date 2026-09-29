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

# The licence check-in talks to a service, so it gets a fake — the same
# one the tiers use, with scenario prefixes for the answers a live
# service would never produce on demand (a revoked licence, a malformed
# body, a 500). Its own port, so a tier's fake left running does not
# collide with this one.
LICENSE_PORT="${LOOPS_LICENSE_PORT:-18099}"
LICENSE_URL="http://127.0.0.1:$LICENSE_PORT"
holder="$(ss -lptnH "sport = :$LICENSE_PORT" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1)"
# A stale fake from an earlier run answers on this port and is NOT
# necessarily the file on disk — one served an entire email run once,
# scenarios and all, and every case agreed with a version of the code
# nobody was looking at. Kill the holder rather than assume.
[ -n "$holder" ] && kill "$holder" 2>/dev/null
"$PYTHON" "$HERE/fake_license.py" --port "$LICENSE_PORT" >/dev/null 2>&1 &
FAKE_PID=$!
trap 'rm -rf "$WORK"; kill $FAKE_PID 2>/dev/null' EXIT
for _ in $(seq 20); do
    curl -fsS -m 1 -X POST "$LICENSE_URL/scenario/valid/v1/licenses/check-in" \
        >/dev/null 2>&1 && break
    sleep 0.2
done

# And a fake Clerk for the plan reconcile, whose whole job is a live
# lookup. Same treatment: its own port, and whatever holds it first is
# killed rather than trusted.
CLERK_PORT="${LOOPS_CLERK_PORT:-18089}"
CLERK_URL="http://127.0.0.1:$CLERK_PORT"
holder="$(ss -lptnH "sport = :$CLERK_PORT" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1)"
[ -n "$holder" ] && kill "$holder" 2>/dev/null
"$PYTHON" "$HERE/fake_clerk.py" --port "$CLERK_PORT" >/dev/null 2>&1 &
CLERK_PID=$!
trap 'rm -rf "$WORK"; kill $FAKE_PID $CLERK_PID 2>/dev/null' EXIT
for _ in $(seq 20); do
    curl -fsS -m 1 "$CLERK_URL/__calls" >/dev/null 2>&1 && break
    sleep 0.2
done

# And a fake Sentinel-Sync-Service. Unlike the other two this one exists
# to be READ BACK: the mirror's behaviour is almost entirely in what it
# sends, and none of that is visible locally afterwards except the
# cursors.
SYNC_PORT="${LOOPS_SYNC_PORT:-18091}"
SYNC_URL="http://127.0.0.1:$SYNC_PORT"
holder="$(ss -lptnH "sport = :$SYNC_PORT" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1)"
[ -n "$holder" ] && kill "$holder" 2>/dev/null
"$PYTHON" "$HERE/fake_sync.py" --port "$SYNC_PORT" >/dev/null 2>&1 &
SYNC_PID=$!
# Teardown in the trap, not at the end: an interrupted run must not
# leave the next harness with a settings-id collision. See the header of
# loops_teardown.sql for why leaving these rows behind breaks a reseed.
teardown() {
    docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q \
        < "$HERE/loops_teardown.sql" >/dev/null 2>&1
    docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q \
        < "$HERE/seed_cameras.sql" >/dev/null 2>&1
}
trap 'rm -rf "$WORK"; kill $FAKE_PID $CLERK_PID $SYNC_PID 2>/dev/null; teardown' EXIT
for _ in $(seq 20); do
    curl -fsS -m 1 "$SYNC_URL/__pushes" >/dev/null 2>&1 && break
    sleep 0.2
done

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
for body in sweep cleanup reaper digest license reconcile sync; do
    seed || { echo "REFUSING: the loop fixture did not apply cleanly"; exit 2; }
    "$PYTHON" "$HERE/py_loops_probe.py" --db "postgresql+psycopg://${PG_URL#postgresql://}" \
        --body "$body" --license-url "$LICENSE_URL" --clerk-url "$CLERK_URL" \
        --sync-url "$SYNC_URL" 2>/dev/null \
        | grep '^{' > "$WORK/py-$body.jsonl"

    seed || { echo "REFUSING: the loop fixture did not apply cleanly"; exit 2; }
    ( cd "$RS" && cargo run --quiet --example loops_probe -- \
        --db "$PG_URL" --body "$body" --license-url "$LICENSE_URL" \
        --clerk-url "$CLERK_URL" --sync-url "$SYNC_URL" ) \
        > "$WORK/rs-$body.jsonl"

    # Compared as parsed JSON per line: Python's json.dumps and
    # serde_json space their separators differently, and that is not a
    # finding.
    if "$PYTHON" - "$WORK/py-$body.jsonl" "$WORK/rs-$body.jsonl" "$body" <<'PY'
import json, sys


def blank_window(rows):
    """Blank the digest meta's window timestamps.

    They are derived from the ANCHOR, and each side seeds its own
    fixture — so the two anchors are the seconds apart the two runs are,
    and so are these. The COUNT and the cooldown in the same blob are
    compared, which is what the window is there to describe.
    """
    for row in rows or []:
        if not isinstance(row, list) or not row:
            continue
        for i, cell in enumerate(row):
            if not isinstance(cell, str) or '"window_start"' not in cell:
                continue
            try:
                meta = json.loads(cell)
            except json.JSONDecodeError:
                continue
            for key in ("window_start", "window_end"):
                if key in meta:
                    meta[key] = "<derived from the anchor>"
            row[i] = json.dumps(meta)
    return rows


def normalise(entry):
    """Sort the one list whose order is not a contract.

    The reaper's `ids` comes from a SELECT with no ORDER BY, on both
    sides — so Postgres answers in physical order and is free to answer
    differently twice. The same trap the MCP differential hit on
    `list_cameras` and `get_stream_stats`, and the same treatment: sort
    it, and say here that the order is not being compared. Everything
    else keeps its order, including the rows, which the probes order
    explicitly in SQL.
    """
    summary = entry.get("summary")
    if isinstance(summary, dict) and isinstance(summary.get("ids"), list):
        summary["ids"] = sorted(summary["ids"])
    if entry.get("rows") == "digests":
        blank_window(entry.get("value"))
    return entry


py, rs, body = sys.argv[1], sys.argv[2], sys.argv[3]
a = [normalise(json.loads(line)) for line in open(py) if line.strip()]
b = [normalise(json.loads(line)) for line in open(rs) if line.strip()]
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
# The licence body's own guard. Every scenario writing the SAME verdict
# would agree perfectly and prove nothing — which is what happens if the
# fake stops distinguishing them, or if the client stops reading the
# body. Count the distinct verdicts instead of the calls.
# The reconcile's guard. A sweep that corrected NOTHING would agree
# perfectly and prove nothing — which is what a fake Clerk answering the
# cached plan for every org produces, and what an unreachable fake
# produces too, since an unreachable Clerk is a SKIP by design.
# The sync's guard. It pushed NOTHING for its first several runs --
# `is_sync_enabled` requires local auth, a licence key, a VALID licence
# and the entitlement, and the probe was running the reconcile's clerk
# mode. Both sides agreed about having done nothing and the run was
# green. Count the rows that actually went up.
synced=$("$PYTHON" -c "
import json
d=json.loads(open('$WORK/rs-sync.jsonl').readline())['summary']
print(sum(p['row_count'] for p in d['first']))
" 2>/dev/null || echo 0)
corrected=$("$PYTHON" -c "
import json
print(json.loads(open('$WORK/rs-reconcile.jsonl').readline())['summary']['changed'])
" 2>/dev/null || echo 0)
verdicts=$("$PYTHON" -c "
import json
d=json.loads(open('$WORK/rs-license.jsonl').readline())['summary']
print(len({(v['valid'], v['reachable'], v['sync_enabled']) for v in d.values()}))
" 2>/dev/null || echo 0)
digested=$("$PYTHON" -c "
import json
rows=[json.loads(l) for l in open('$WORK/rs-digest.jsonl')]
print(sum(len(r['value'] or []) for r in rows if r.get('rows') == 'digests'))
" 2>/dev/null || echo 0)
reaped=$("$PYTHON" -c "
import json
d=json.loads(open('$WORK/rs-reaper.jsonl').readline())['summary']
print(d['reaped'] + d['abandoned'])
" 2>/dev/null || echo 0)
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
echo "fixture: sweep flipped $flips, cleanup deleted $deleted, reaper stamped $reaped,"
echo "         digest emitted $digested, licence reached $verdicts distinct verdict(s),"
echo "         reconcile corrected $corrected plan(s), sync pushed $synced row(s)"
if [ "$flips" -lt 4 ] || [ "$deleted" -lt 20 ] || [ "$reaped" -lt 2 ] \
   || [ "$digested" -lt 1 ] || [ "$verdicts" -lt 3 ] \
   || [ "$corrected" -lt 2 ] || [ "$synced" -lt 50 ]; then
    echo "FIXTURE TOO THIN — every body must actually do work, or no-ops agree"
    exit 2
fi

echo "$((compared - bad))/$compared identical, $bad differing"
[ "$bad" = 0 ] || exit 1
