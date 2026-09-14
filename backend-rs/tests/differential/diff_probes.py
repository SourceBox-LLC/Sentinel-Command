"""Compare two probe outputs as JSON, not as text.

Text comparison is useless here: Python's json.dumps separates with
", " and serde_json with ",", so byte-identical results differ on every
line. Parse both and compare values.
"""
import json
import sys
from collections import Counter

corpus_path, a_path, b_path = sys.argv[1], sys.argv[2], sys.argv[3]
label = sys.argv[4] if len(sys.argv) > 4 else ""

corpus = [json.loads(l) for l in open(corpus_path)]
a = [json.loads(l) for l in open(a_path)]
b = [json.loads(l) for l in open(b_path)]
if not (len(corpus) == len(a) == len(b)):
    print(f"LENGTH MISMATCH corpus={len(corpus)} a={len(a)} b={len(b)}")
    sys.exit(2)

diffs = [(c, x, y) for c, x, y in zip(corpus, a, b) if x != y]
print(f"{label:<44} {len(corpus)-len(diffs):5d} identical  {len(diffs):5d} differing")

if diffs and "-v" in sys.argv:
    kinds = Counter()
    for c, x, y in diffs:
        keys = set(x) | set(y)
        kinds[tuple(sorted(k for k in keys if x.get(k) != y.get(k)))] += 1
    for k, n in kinds.most_common(6):
        print(f"      {n:5d}  fields: {k}")
    for c, x, y in diffs[:3]:
        print("      claims:", json.dumps(c, sort_keys=True))
        print("        py:", json.dumps(x, sort_keys=True))
        print("        rs:", json.dumps(y, sort_keys=True))
