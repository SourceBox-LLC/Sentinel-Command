#!/usr/bin/env bash
#
# The Clerk-mode differential: everything that only means something
# against the pair on 8100/8101.
#
# The default tiers run AUTH_PROVIDER=local, where `resolve_org_plan`
# returns "self_host" before reading anything. Against them no camera
# is ever over cap, so registration's skip loop, its skipped list, the
# plan-limit notification and its debounce are all unreachable — and
# every `org_plan` a case writes is inert. That is not a hole a case
# can close: five mutations survived a full run of the register spec
# for this reason alone, and the cases covering them PASSED.
#
# These routes authenticate with a node API key, so unlike the twenty
# or so Clerk-gated ones they need no RS256 session token and can run
# against the Clerk pair as they are. Every case pins a PAID slug,
# because `resolve_org_plan` short-circuits on one without calling
# Clerk; a free slug would reach for the network on every request.
#
# The two pairs share one database, so this cannot run at the same time
# as write_run.sh.
#
# Two kinds of case live here: the plan-cap half of the node routes
# (below), and the Clerk webhook, which main.py does not even mount
# under local auth.
#
# Usage: tests/differential/clerk_run.sh [-v]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! curl -fsS -m 3 "http://127.0.0.1:8100/api/health" >/dev/null 2>&1 \
   || ! curl -fsS -m 3 "http://127.0.0.1:8101/api/health" >/dev/null 2>&1; then
    echo "starting the clerk-mode pair..."
    "$HERE/tiers.sh" start-clerk
fi

CASE_SET=clerk \
    RUST_URL="http://127.0.0.1:8100" \
    PYTHON_URL="http://127.0.0.1:8101" \
    exec "$HERE/write_run.sh" "$@"
