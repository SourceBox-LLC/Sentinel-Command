#!/usr/bin/env bash
#
# The crate's own tests, reported in the counting format a mutation run
# reads — so a spec can be held to the corpora as well as to the two
# HTTP differentials.
#
# This exists because a mutation survived that both differentials were
# structurally unable to catch. Deleting the key validation in
# `src/zoneinfo.rs::load` changes no response: a traversing name still
# resolves to nothing and still falls back to UTC. What changes is that
# the lookup will happily read a TZif file from anywhere on disk, which
# is a property of the function, not of any response — and the corpora
# and unit tests are where properties like that are pinned.
#
# Usage: tests/differential/unit_run.sh
#
# Env: CARGO (default: cargo on PATH)

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RS="$(cd "$HERE/../.." && pwd)"
CARGO="${CARGO:-cargo}"

out="$("$CARGO" test --manifest-path "$RS/Cargo.toml" 2>&1)"
status=$?

# `cargo test` prints one "test result:" line per binary; sum them.
read -r passed failed < <(
    printf '%s\n' "$out" | awk '
        /^test result:/ {
            for (i = 1; i <= NF; i++) {
                if ($(i+1) ~ /^passed/) p += $i
                if ($(i+1) ~ /^failed/) f += $i
            }
        }
        END { print p + 0, f + 0 }
    '
)
total=$((passed + failed))

if [[ "$total" == 0 ]]; then
    # No result line at all means it did not build or did not run —
    # report it as a difference rather than as a silent pass.
    echo "0/1 identical, 1 differing (no test result: cargo exited $status)"
    printf '%s\n' "$out" | tail -20
    exit 1
fi

printf '%s\n' "$out" | grep -E "^(test .* FAILED|failures:|---- )" | head -20
echo "$passed/$total identical, $failed differing"
[[ "$failed" == 0 && "$status" == 0 ]]
