#!/usr/bin/env python3
"""Regenerate tests/fixtures/fromisoformat_corpus.json.

`src/pydatetime.rs` is a port of the C `datetime.fromisoformat` in
CPython 3.12's `Modules/_datetimemodule.c`, not of `_pydatetime.py`.
The two disagree: the C accepts `15:00:00:00` and `15.00`, never looks
at the character in the separator position, parses ASCII digits only,
and stops at an embedded NUL as if the string ended there. This corpus
is what the port is held to, so it has to come from the interpreter
the backend actually runs.

Every entry records two outcomes for one input:

* `iso`   — `datetime.fromisoformat(s)` alone: the naive fields and the
  UTC offset in microseconds, or the exception type.
* `since` — the whole `since` pipeline of `GET /api/sentinel/runs`:
  `.replace("Z", "+00:00")`, parse, then `astimezone(UTC)` for an aware
  result. The route turns ValueError into a 400 and lets anything
  else become a 500, so the exception type is part of the contract.

Two halves, as in gen_corpus.py: hand-picked shapes from reading the C,
then seeded random edits of valid strings, to catch what reading
missed.

Usage: backend/.venv/bin/python tests/differential/gen_fromisoformat_corpus.py
"""
import json
import pathlib
import random
import sys
from datetime import UTC, datetime

assert sys.version_info[:2] == (3, 12), "the port follows 3.12's C parser"

random.seed(20260918)  # reproducible: a flaky corpus is not evidence

HAND_PICKED = [
    # --- the ordinary shapes -------------------------------------------
    "2026-05-07T15:00:00",
    "2026-05-07T15:00:00.123",
    "2026-05-07T15:00:00.123456",
    "2026-05-07T15:00:00.1234567",
    "2026-05-07T15:00:00+00:00",
    "2026-05-07T15:00:00Z",
    "2026-05-07T15:00-05:00",
    "2026-05-07T15:00:00+05:30",
    "2026-05-07 15:00:00",
    "2026-05-07",
    "20260507",
    "20260507T150000",
    "20260507T150000.5",
    "2026-05-07T15",
    "2026-05-07T1500",
    "2026-05-07T150000",
    "2026-05-07T15:00:00,5",
    # --- week and ordinal dates ----------------------------------------
    "2026-W01", "2026W01", "2026-W01-1", "2026W011", "2026-W53-1",
    "2020-W53-7", "2026-W00-1", "2026-W01-0", "2026-W01-8",
    "2026W01T10", "2026W011T10", "2026W0110", "2026W01110",
    "2026W011100", "2026W0111000", "2026-W01-0000", "2026-W01-",
    "2026-W01T10:00", "2026-W01-1T10:00", "2026-W0", "2026-W",
    "2026-123", "2026123",
    # --- what the C accepts and the pure-Python reference does not ------
    "2026-05-07T15:00:00:00",
    "2026-05-07T15:00:00:123",
    "2026-05-07T15.00",
    "2026-05-07T15,5",
    "2026-05-07X15:00:00",
    "2026-05-07é15:00:00",
    "2026-05-07€15:00:00",
    "2026-05-07\U0001f600" + "15:00:00",
    "2026-05-07\x0015:00:00",
    "2026-05-07T150000123",
    "2026-05-07T15:00:00.123456\x00junk",
    "2026-05-07T15:00:00.123\x00junk",
    "2026-05-07T15:00\x00",
    "2026-05-07T15:00:00.123456789012",
    # --- digits that are not ASCII --------------------------------------
    "٢٠٢٦-05-07",
    "2026-05-07T１5:00",
    "2026-05-07T15:00:00.１",
    # --- range checks ----------------------------------------------------
    "0000-01-01", "0001-01-01", "9999-12-31", "2026-00-01", "2026-13-01",
    "2026-02-29", "2024-02-29", "2026-04-31", "2026-05-00",
    "2026-05-07T24:00", "2026-05-07T23:60", "2026-05-07T23:59:60",
    "2026-05-07T23:59:59.999999",
    # --- time zones ------------------------------------------------------
    "2026-05-07T15:00+24:00", "2026-05-07T15:00-24:00",
    "2026-05-07T15:00+23:59:59.999999", "2026-05-07T15:00-23:59:59.999999",
    "2026-05-07T15:00+05:99", "2026-05-07T15:00+99",
    "2026-05-07T15:00+0530", "2026-05-07T15:00+05", "2026-05-07T15:00+5",
    "2026-05-07T15:00+00:00:00.5", "2026-05-07T15:00-00:00:00.5",
    "2026-05-07T15:00+00:00:01.5", "2026-05-07T15:00-00:00:01.5",
    "2026-05-07T15:00+05:00:00:00", "2026-05-07T15:00+05:00x",
    "2026-05-07T15:00ZZ", "2026-05-07T15:00Z+01:00", "2026-05-07TZ",
    "2026-05-07T15:00+", "2026-05-07T15:00-", "2026-05-07T+05:00",
    "2026-05-07T15-05:00", "2026-05-07T15:00:00.5+05:00",
    "2026-05-07T15:00:00.5x+05:00",
    # --- overflow in astimezone, which the route does not catch ---------
    "0001-01-01T00:00+01:00", "0001-01-01T01:00+01:00",
    "0001-01-01T00:00-01:00", "9999-12-31T23:00-01:00",
    "9999-12-31T22:59:59.999999-01:00", "9999-12-31T23:59+00:00",
    # --- short, empty and degenerate -------------------------------------
    "", "2026", "2026-05", "202605", "2026-5-7", "2026-05-7", "2026-05-07T",
    "2026-05-07T1", "2026-05-07T15:0", "2026-05-07T15:", "2026-05-07 ",
    "        ", "abcdefgh", "2026-05-07T15:00:00 ", " 2026-05-07",
    "2026-05-07T15:00:00.", "2026-05-07T15:00:00.+05:00",
    "2026-0507", "202605-07", "2026-05-07T15:0000", "2026-05-07T1500:00",
]

ALPHABET = list("0123456789-:T W.,+Z") + ["\x00", "é", "€", "١", "x"]

SEEDS = [
    "2026-05-07T15:00:00.123456+05:30",
    "20260507T150000.5-0800",
    "2026-W01-3T10:00Z",
    "2026W013T1000",
    "0001-01-01T00:30+01:00",
    "9999-12-31T23:30-01:00",
    "2024-02-29 23:59:59,999999",
]


def mutate(s: str) -> str:
    chars = list(s)
    for _ in range(random.randint(1, 3)):
        op = random.choice(("insert", "delete", "replace"))
        pos = random.randrange(len(chars) + (op == "insert")) if chars else 0
        if op == "insert" or not chars:
            chars.insert(pos, random.choice(ALPHABET))
        elif op == "delete":
            del chars[pos]
        else:
            chars[pos] = random.choice(ALPHABET)
    return "".join(chars)


def fmt(dt: datetime) -> str:
    return dt.replace(tzinfo=None).isoformat(timespec="microseconds")


def iso_outcome(s: str) -> dict:
    try:
        dt = datetime.fromisoformat(s)
    except Exception as exc:  # noqa: BLE001 — the type is what is recorded
        return {"error": type(exc).__name__}
    off = dt.utcoffset()
    return {
        "naive": fmt(dt),
        "offset_us": None if off is None else off // off.resolution,
    }


def since_outcome(s: str) -> dict:
    # Mirrors backend/app/api/sentinel.py list_runs, line for line.
    try:
        since_dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
        if since_dt.tzinfo is not None:
            since_dt = since_dt.astimezone(UTC).replace(tzinfo=None)
    except Exception as exc:  # noqa: BLE001
        return {"error": type(exc).__name__}
    return {"utc": fmt(since_dt)}


def main() -> None:
    inputs = list(dict.fromkeys(HAND_PICKED))
    seen = set(inputs)
    while len(inputs) < len(HAND_PICKED) + 4000:
        s = mutate(random.choice(SEEDS))
        if s not in seen:
            seen.add(s)
            inputs.append(s)

    corpus = [{"in": s, "iso": iso_outcome(s), "since": since_outcome(s)} for s in inputs]
    out = pathlib.Path(__file__).resolve().parents[2] / "tests" / "fixtures" / "fromisoformat_corpus.json"
    out.write_text(json.dumps(corpus, ensure_ascii=True, indent=0) + "\n")

    ok = sum("naive" in c["iso"] for c in corpus)
    kinds = sorted({c["since"].get("error", "ok") for c in corpus})
    print(f"wrote {len(corpus)} cases ({ok} parse) to {out}; since outcomes: {kinds}")


if __name__ == "__main__":
    main()
