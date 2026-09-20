#!/usr/bin/env bash
#
# Reproduces the latent 500s the port found in the Python service.
#
# These are kept out of the main HTTP differential deliberately: each one
# makes Python return 500 for *every* request to the affected route, so
# leaving the rows in place would turn every other case on that route red
# and hide real regressions. They are asserted here instead, so the
# finding is runnable evidence rather than a claim in a comment.
#
# Neither is reachable in production today — every writer sets these
# columns — so this is a hardening note for `backend/`, not an incident.
#
# Usage: tests/differential/latent_crashes.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PYTHON="${PYTHON:-$REPO/backend/.venv/bin/python}"
PG_CONTAINER="${PG_CONTAINER:-cc-schema-test}"
APP_SECRET_KEY="${APP_SECRET_KEY:-differential-test-secret-not-a-real-key}"

TOKEN="$(cd "$REPO/backend" && env \
    APP_SECRET_KEY="$APP_SECRET_KEY" AUTH_PROVIDER=local LOCAL_ORG_ID=self-host \
    "$PYTHON" -c "
import sys; sys.path.insert(0, '.')
from app.core.local_auth import issue_token
print(issue_token())
")"

psql () { docker exec -i "$PG_CONTAINER" psql -U cc -d cc -q "$@"; }
code () { curl -s -o /dev/null -w '%{http_code}' -m 10 -H "Authorization: Bearer $TOKEN" "$1"; }

fails=0
check () {
    local label="$1" url="$2" want_rust="$3" want_py="$4"
    local r p
    r=$(code "http://127.0.0.1:8000$url")
    p=$(code "http://127.0.0.1:8001$url")
    if [[ "$r" == "$want_rust" && "$p" == "$want_py" ]]; then
        printf '  ok    %-44s rust=%s python=%s\n' "$label" "$r" "$p"
    else
        printf '  FAIL  %-44s rust=%s (want %s)  python=%s (want %s)\n' \
               "$label" "$r" "$want_rust" "$p" "$want_py"
        fails=$((fails + 1))
    fi
}

echo "1. AuditLog.to_dict() calls .isoformat() on a nullable column."
echo "   One odd row 500s the whole page of audit history."
psql -c "INSERT INTO audit_log (org_id, timestamp, event, username)
         VALUES ('self-host', NULL, 'null_ts_probe', 'x')" >/dev/null
check "GET /api/audit-logs with a NULL timestamp" "/api/audit-logs?limit=500" 200 500
psql -c "DELETE FROM audit_log WHERE event = 'null_ts_probe'" >/dev/null

echo
echo "2. /settings/motion-ingestion calls .lower() on Setting.get(), which"
echo "   returns None for a row whose value is NULL. Its sibling routes"
echo "   compare with == and do not crash on the same shape of data."
psql -c "UPDATE settings SET value = NULL
          WHERE org_id = 'self-host' AND key = 'motion_ingestion_enabled'" >/dev/null
check "GET /api/settings/motion-ingestion, NULL value" "/api/settings/motion-ingestion" 200 500
# Rust answers 'false' here: re-enabling a kill switch an operator set,
# just because its value became unreadable, is the wrong direction for a
# valve whose purpose is to stop a flood of events.
psql -c "UPDATE settings SET value = 'TRUE'
          WHERE org_id = 'self-host' AND key = 'motion_ingestion_enabled'" >/dev/null

echo
echo "3. A JSON body sent with a non-JSON Content-Type 500s rather than"
echo "   being rejected. FastAPI reads the body for a declared Pydantic"
echo "   model but raises when the media type is not JSON."
r=$(curl -s -o /dev/null -w '%{http_code}' -m 10 -X POST \
      -H "Authorization: Bearer $TOKEN" -H "Content-Type: text/plain" \
      -d '{"name":"probe"}' "http://127.0.0.1:8000/api/camera-groups")
p=$(curl -s -o /dev/null -w '%{http_code}' -m 10 -X POST \
      -H "Authorization: Bearer $TOKEN" -H "Content-Type: text/plain" \
      -d '{"name":"probe"}' "http://127.0.0.1:8001/api/camera-groups")
if [[ "$p" == "500" ]]; then
    printf '  ok    %-44s rust=%s python=%s\n' "POST with Content-Type: text/plain" "$r" "$p"
else
    printf '  FAIL  %-44s rust=%s python=%s (expected python 500)\n' \
           "POST with Content-Type: text/plain" "$r" "$p"
    fails=$((fails + 1))
fi
psql -c "DELETE FROM camera_groups WHERE name = 'probe'" >/dev/null

echo
echo "4. An out-of-range path integer reaches the query and Postgres"
echo "   rejects it: incident_id is unbounded in python, the column is"
echo "   Integer, so anything past int32 is a DataError rather than a 404."
echo "   Rust answered 422 here until the port reproduced it instead —"
echo "   the value is one FastAPI accepts, so refusing it early was a"
echo "   divergence, not a fix. Both 500 now; the read differential"
echo "   sends the values on either side of the boundary."
check "GET /api/incidents/99999999999999" "/api/incidents/99999999999999" 500 500

echo
if (( fails )); then
    echo "$fails check(s) did not reproduce — the Python may have been fixed."
    echo "If so, delete the corresponding entry here and in"
    echo "expected_divergences.md rather than leaving a stale claim."
    exit 1
fi
echo "all four reproduced: Rust serves through the first three, and reproduces the fourth"
