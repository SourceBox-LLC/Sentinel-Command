#!/usr/bin/env bash
#
# The dialect differential for the background loops: each body run once
# on the PostgreSQL build and once on the SQLite build, from the SAME
# rows, and compared by what it reports and by what it leaves behind.
#
# The loops have no HTTP surface, so dialect_run.sh cannot reach them.
# loops_run.sh is the harness that verified them against the Python; its
# read-back queries are PostgreSQL throughout, so this one compares the
# TABLES instead — every table, after each body — which is a wider net
# than those readouts and needs nothing written in a second dialect.
#
# Seeded ONCE per body and then copied, not seeded twice: the fixture
# places rows relative to now(), and two seedings a second apart would
# differ in every one of those timestamps before either build ran.
#
# Usage: tests/differential/dialect_loops.sh [-v] [body ...]

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RS="$(cd "$HERE/../.." && pwd)"
BIN="$RS/target/tier-bin"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
PG_URL="${PROBE_DATABASE_URL:-postgresql://cc:cc@127.0.0.1:15434/cc}"
LITE="$RS/target/dialect-loops.db"
WORK="$(mktemp -d)"
VERBOSE=""; BODIES=()
for arg in "$@"; do [[ "$arg" == -v ]] && VERBOSE=1 || BODIES+=("$arg"); done
[[ ${#BODIES[@]} -gt 0 ]] || BODIES=(sweep cleanup reaper digest license reconcile sync)

for port in 8000 8001; do
    if curl -fsS -m 2 "http://127.0.0.1:$port/api/health" >/dev/null 2>&1; then
        echo "REFUSING: a tier is up on :$port and its own loops run against these databases." >&2
        echo "Stop it first: tests/differential/dialect_run.sh stop" >&2
        exit 2
    fi
done

mkdir -p "$BIN"
echo "building the probe for both drivers..."
( cd "$RS" && cargo build --quiet --example loops_probe \
    && cp target/debug/examples/loops_probe "$BIN/loops_probe-pg" \
    && cargo build --quiet --features sqlite --example loops_probe \
    && cp target/debug/examples/loops_probe "$BIN/loops_probe-sqlite" ) || exit 2

PIDS=()
fake() {  # script port probe-path
    local holder; holder="$(ss -lptnH "sport = :$2" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1)"
    [[ -n "$holder" ]] && kill "$holder" 2>/dev/null
    python3 "$HERE/$1" --port "$2" >/dev/null 2>&1 &
    PIDS+=($!)
    for _ in $(seq 25); do curl -fsS -m 1 $3 >/dev/null 2>&1 && return 0; sleep 0.2; done
    echo "REFUSING: $1 did not come up on :$2" >&2; exit 2
}
LICENSE_URL=http://127.0.0.1:18099; CLERK_URL=http://127.0.0.1:18089; SYNC_URL=http://127.0.0.1:18091
teardown() {
    kill "${PIDS[@]}" 2>/dev/null
    docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q < "$HERE/loops_teardown.sql" >/dev/null 2>&1
    docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q < "$HERE/seed_cameras.sql" >/dev/null 2>&1
    rm -rf "$WORK"
}
trap teardown EXIT
fake fake_license.py 18099 "-X POST $LICENSE_URL/scenario/valid/v1/licenses/check-in"
fake fake_clerk.py 18089 "$CLERK_URL/__calls"
fake fake_sync.py 18091 "$SYNC_URL/__pushes"

# The schema, once. The probe does not migrate; the service does.
rm -f "$LITE" "$LITE-wal" "$LITE-shm"
python3 - "$RS/migrations-sqlite/0001_adopt_python_schema.sql" "$LITE" <<'PY'
import sqlite3, sys
con = sqlite3.connect(sys.argv[2]); con.executescript(open(sys.argv[1]).read()); con.close()
PY

bad=0; compared=0
for body in "${BODIES[@]}"; do
    docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q < "$HERE/seed_cameras.sql" >/dev/null 2>&1
    if docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q < "$HERE/loops_fixture.sql" 2>&1 | grep -v '^$'; then
        echo "REFUSING: the loop fixture did not apply cleanly"; exit 2
    fi
    python3 - "$LITE" <<PY || exit 2
import sys; sys.path.insert(0, "$HERE")
import dialect
dialect.align_sequences()
dialect.copy_from_postgres(sys.argv[1])
PY
    args=(--body "$body" --license-url "$LICENSE_URL" --clerk-url "$CLERK_URL" --sync-url "$SYNC_URL")
    # After each probe, what the fake mirror is holding. The probes'
    # own summary of a push is its SHAPE — tables, columns, row counts —
    # because against the Python that was what could be compared. Here
    # the VALUES can be, and they are the part a second dialect is most
    # likely to get wrong: SQLite keeps a timestamp as text with a space
    # in it and a boolean as 0 or 1, and the mirror must not.
    "$BIN/loops_probe-pg" --db "$PG_URL" "${args[@]}" > "$WORK/pg-$body.jsonl" 2> "$WORK/pg-$body.err"
    curl -fsS -m 10 "$SYNC_URL/__pushes" > "$WORK/pg-$body.pushes" 2>/dev/null || echo '[]' > "$WORK/pg-$body.pushes"
    "$BIN/loops_probe-sqlite" --db "sqlite:///$LITE" "${args[@]}" > "$WORK/lite-$body.jsonl" 2> "$WORK/lite-$body.err"
    curl -fsS -m 10 "$SYNC_URL/__pushes" > "$WORK/lite-$body.pushes" 2>/dev/null || echo '[]' > "$WORK/lite-$body.pushes"

    if python3 - "$WORK" "$body" "$LITE" "${VERBOSE:-0}" <<PY
import json, re, subprocess, sys
from datetime import datetime, timedelta, timezone
sys.path.insert(0, "$HERE")
import dialect
from diffutil import value_diff

work, body, lite, verbose = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4] == "1"
now = datetime.now(timezone.utc).replace(tzinfo=None)
ISO = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(\+00:00|Z)?$")
HEX = re.compile(r"^[0-9a-f]{32}$")


def mask(value):
    """A clock reading from this run, or a freshly minted id.

    Each build stamps its own "now" and mints its own install id; the
    fixture's timestamps are hours to months old and are compared as
    they are.
    """
    if isinstance(value, dict):
        return {k: mask(v) for k, v in value.items()}
    if isinstance(value, list):
        return [mask(v) for v in value]
    if isinstance(value, str):
        if ISO.match(value):
            try:
                stamp = datetime.fromisoformat(value.replace("Z", "").replace("+00:00", ""))
                if abs(now - stamp) < timedelta(minutes=5):
                    return "<now>"
            except ValueError:
                pass
        if HEX.match(value):
            return "<minted>"
    return value


def summaries(path):
    out = []
    for line in open(path):
        line = line.strip()
        if line.startswith("{"):
            entry = json.loads(line)
            if "summary" in entry:
                summary = entry["summary"]
                # A SELECT with no ORDER BY, on both builds.
                if isinstance(summary, dict) and isinstance(summary.get("ids"), list):
                    summary["ids"] = sorted(summary["ids"])
                out.append(mask(summary))
    return out


pg, lt = summaries(f"{work}/pg-{body}.jsonl"), summaries(f"{work}/lite-{body}.jsonl")
if not pg or not lt:
    print(f"  REFUSING  {body}: a probe reported nothing (postgres {len(pg)}, sqlite {len(lt)})")
    for side in ("pg", "lite"):
        print("            " + open(f"{work}/{side}-{body}.err").read().strip()[-400:])
    sys.exit(2)

bad = 0
if pg != lt:
    bad += 1
    print(f"  DIFFER  {body} / what the body reported")
    for line in value_diff(lt, pg):
        print(f"            {line}  (python=sqlite, rust=postgres)")

if body == "sync":
    def pushed(path):
        out = {}
        for push in json.load(open(path)):
            rows = sorted(push.get("rows") or [], key=lambda r: str(r.get("id")))
            out.setdefault(push.get("table"), []).extend(rows)
        return mask(out)
    pg_push, lt_push = pushed(f"{work}/pg-sync.pushes"), pushed(f"{work}/lite-sync.pushes")
    total = sum(len(v) for v in pg_push.values())
    if total < 50:
        print(f"  REFUSING  sync: only {total} row(s) reached the mirror — nothing to compare values on")
        sys.exit(2)
    if pg_push != lt_push:
        bad += 1
        print(f"  DIFFER  sync / the values pushed to the mirror ({total} rows)")
        for line in value_diff(lt_push, pg_push)[:12]:
            print(f"            {line}  (python=sqlite, rust=postgres)")
    elif verbose:
        print(f"            sync: {total} mirrored row(s) identical in value")

con_tables = [r[0] for r in dialect.query(
    lite, "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")]
parts = ", ".join(
    f"'{t}', COALESCE((SELECT json_agg(x) FROM (SELECT * FROM {t} ORDER BY 1) x), '[]'::json)"
    for t in con_tables)
raw = subprocess.run(
    ["docker", "exec", "-i", "$PG_CONTAINER", "psql", "-U", "cc", "-d", "cc", "-tAq", "-c",
     f"SELECT json_build_object({parts})"], capture_output=True, text=True, timeout=120, check=True).stdout
pg_rows = mask(json.loads(raw))
lt_rows = mask(dialect.snapshot(lite, con_tables))
# Both of these bodies delete settings rows and write them again, several
# times over. PostgreSQL's sequence keeps climbing; a SQLite INTEGER
# PRIMARY KEY hands back the ids just freed. The same row under a
# different number — see ID_BLIND in write_diff.py for the full note.
for table in {"license": ("settings",), "sync": ("settings",)}.get(body, ()):
    for snapshot_rows in (pg_rows[table], lt_rows[table]):
        for row in snapshot_rows:
            row.pop("id", None)
        snapshot_rows.sort(key=lambda r: json.dumps(r, sort_keys=True))
for table in con_tables:
    if pg_rows[table] == lt_rows[table]:
        continue
    bad += 1
    print(f"  DIFFER  {body} / rows left in {table}")
    for line in value_diff(lt_rows[table], pg_rows[table])[:8]:
        print(f"            {line}  (python=sqlite, rust=postgres)")
if verbose and not bad:
    print(f"            {body}: {json.dumps(pg, sort_keys=True)[:200]}")
sys.exit(1 if bad else 0)
PY
    then printf '  ok      %s\n' "$body"
    else rc=$?; [[ "$rc" == 2 ]] && exit 2; bad=$((bad + 1)); fi
    compared=$((compared + 1))
done

echo
echo "$((compared - bad))/$compared identical, $bad differing"
[[ "$bad" == 0 ]]
