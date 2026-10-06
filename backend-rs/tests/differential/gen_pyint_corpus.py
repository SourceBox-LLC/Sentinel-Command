#!/usr/bin/env python3
"""Regenerate tests/fixtures/pyint_corpus.json.

`src/pyint.rs` ports pydantic-core's `str_as_int` and the jiter integer
parse beneath it — the path every `int` query parameter, and every
string sent where a body wants an int, goes through. It is not
Python's `int()`: leading zeros are fine, `"5.00"` is 5, `"1e3"` is not,
and past 4,300 characters there is a size error — but only when the
first, strict attempt reaches that length.

Each entry is the outcome of `TypeAdapter(int).validate_python(s)`:
the value, when it fits an i64; only the sign, when it does not, which
is all a bound check needs; or the error type.

Usage: backend/.venv/bin/python tests/differential/gen_pyint_corpus.py
"""
import json
import pathlib
import random

from pydantic import TypeAdapter, ValidationError

random.seed(20260918)  # reproducible: a flaky corpus is not evidence

I64_MIN, I64_MAX = -(2**63), 2**63 - 1
INT = TypeAdapter(int)

HAND_PICKED = [
    "0", "5", "-5", "+5", "-0", "+0", "00", "007", "-007", "+007",
    "5.0", "5.00", "-5.0", "5.", ".5", ".0", "5.5", "0.0", "00.0", "-0.0",
    "1e3", "1E3", "1_000", "1__000", "_1", "1_", "+_1", "-_1", "0_0", "0_",
    "0_0_1", "05_0", "5_0.0", "5.0_0", "5_.0", "0_.0",
    " 5", "5 ", "\t5\t", "\n5\n", " 5", " 5", "5　", " +5 ", " -5 ",
    "- 5", "+ 5", "+-5", "-+5", "--5", "++5", "", " ", "+", "-", "_", ".",
    "0x10", "0o7", "0b1", "inf", "-inf", "nan", "NaN", "Infinity", "-Infinity",
    "٣", "５", "𝟓", "1٣",
    "9223372036854775807", "9223372036854775808", "-9223372036854775808",
    "-9223372036854775809", "99999999999999999999", "-99999999999999999999",
    "999999999999999999", "1000000000000000000", "-1000000000000000000",
    "9223372036854775807.0", "9223372036854775808.00", "09223372036854775808",
    "1_000_000_000_000_000_000_000", "0000099999999999999999999",
    "123abc", "12 34", "1,000", "1.000.0",
    # --- the 4,300-character boundary, from both sides and every angle ---
    "1" + "0" * 4299,
    "1" + "0" * 4300,
    "-1" + "0" * 4298,
    "-1" + "0" * 4299,
    "-1" + "0" * 4300,
    "+1" + "0" * 4299,
    "+1" + "0" * 4300,
    " 1" + "0" * 4299,
    "1" + "0" * 4300 + " ",
    "1_" + "0" * 4299,
    "1_" + "0" * 4300,
    "1" + "0" * 4299 + ".0",
    "1" + "0" * 4300 + ".0",
    "1" + "0" * 4300 + "x",
    "1" + "0" * 4300 + "e5",
    "0" * 4300 + "5",
    "0" * 5000 + "9" * 20,
    "0" * 5000,
    "5." + "0" * 5000,
    "-" + "0" * 5000 + "1",
]

ALPHABET = list("0123456789") * 3 + list("_.-+ eE") + ["\t", "x", " "]


def fuzz() -> str:
    n = random.choice((1, 2, 3, 5, 8, 18, 19, 20, 25))
    s = "".join(random.choice(ALPHABET) for _ in range(n))
    if random.random() < 0.01:
        s += "0" * random.choice((4280, 4290, 4300))
    return s


def outcome(s: str) -> dict:
    try:
        v = INT.validate_python(s)
    except ValidationError as exc:
        return {"error": exc.errors()[0]["type"]}
    if I64_MIN <= v <= I64_MAX:
        return {"int": str(v)}
    return {"big": "-" if v < 0 else "+"}


def main() -> None:
    inputs = list(dict.fromkeys(HAND_PICKED))
    seen = set(inputs)
    while len(inputs) < len(HAND_PICKED) + 5000:
        s = fuzz()
        if s not in seen:
            seen.add(s)
            inputs.append(s)

    corpus = [{"in": s, **outcome(s)} for s in inputs]
    out = pathlib.Path(__file__).resolve().parents[2] / "tests" / "fixtures" / "pyint_corpus.json"
    out.write_text(json.dumps(corpus, ensure_ascii=True, indent=0) + "\n")

    kinds = {}
    for c in corpus:
        k = c.get("error") or ("big" if "big" in c else "int")
        kinds[k] = kinds.get(k, 0) + 1
    print(f"wrote {len(corpus)} cases to {out}: {kinds}")


if __name__ == "__main__":
    main()
