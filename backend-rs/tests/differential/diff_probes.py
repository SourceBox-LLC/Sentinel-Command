"""Compare two probe outputs as JSON, not as text.

Text comparison is useless here: Python's json.dumps separates with
", " and serde_json with ",", so byte-identical results differ on every
line. Parse both and compare values.

A small number of divergences are deliberate — see expected_divergences.md
and validate_claim_types() in src/auth/claims.rs. They are listed in
expected_divergences.jsonl and checked two ways: an unexpected divergence
fails the run, and so does an *expected* one that has stopped diverging,
because that means the list has gone stale and is hiding real coverage.

Exit codes: 0 all good, 1 divergences, 2 the corpus proved nothing.
"""
import json
import sys
from collections import Counter
from pathlib import Path

HERE = Path(__file__).resolve().parent

corpus_path, a_path, b_path = sys.argv[1], sys.argv[2], sys.argv[3]
label = sys.argv[4] if len(sys.argv) > 4 else ""
verbose = "-v" in sys.argv

corpus = [json.loads(l) for l in open(corpus_path)]
a = [json.loads(l) for l in open(a_path)]
b = [json.loads(l) for l in open(b_path)]
if not (len(corpus) == len(a) == len(b)):
    print(f"LENGTH MISMATCH corpus={len(corpus)} a={len(a)} b={len(b)}")
    sys.exit(2)


def key(claims):
    return json.dumps(claims, sort_keys=True)


expected_path = HERE / "expected_divergences.jsonl"
expected = set()
if expected_path.exists():
    for line in expected_path.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("//"):
            expected.add(key(json.loads(line)))

diverged = {key(c) for c, x, y in zip(corpus, a, b) if x != y}
unexpected = [(c, x, y) for c, x, y in zip(corpus, a, b)
              if x != y and key(c) not in expected]
stale = expected - diverged

print(f"{label:<44} {len(corpus)-len(diverged):5d} identical  "
      f"{len(diverged):5d} differing "
      f"({len(diverged)-len(unexpected)} expected, {len(unexpected)} unexpected)")

# A corpus that resolves nothing would compare equal and prove nothing.
resolved = sum(1 for r in b if "error" not in r)
with_perms = sum(1 for r in b if "error" not in r and r["org_permissions"])
admins = sum(1 for r in b if "error" not in r and r["is_admin"])
print(f"coverage: {resolved} users resolved, {with_perms} with permissions, "
      f"{admins} admin, {len(b)-resolved} rejected")

if verbose and unexpected:
    kinds = Counter()
    for c, x, y in unexpected:
        keys = set(x) | set(y)
        kinds[tuple(sorted(k for k in keys if x.get(k) != y.get(k)))] += 1
    for k, n in kinds.most_common(6):
        print(f"      {n:5d}  fields: {k}")

for c, x, y in unexpected[:10]:
    print("  UNEXPECTED", json.dumps(c, sort_keys=True))
    print("        py:", json.dumps(x, sort_keys=True))
    print("        rs:", json.dumps(y, sort_keys=True))

for s in sorted(stale)[:10]:
    print(f"  STALE (listed as expected but now agrees): {s}")

if resolved < 100 or with_perms < 100 or admins < 50:
    print("CORPUS IS TOO THIN — this run proves nothing")
    sys.exit(2)
sys.exit(1 if (unexpected or stale) else 0)
