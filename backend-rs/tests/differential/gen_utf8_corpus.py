#!/usr/bin/env python3
"""Regenerate tests/fixtures/utf8_corpus.json.

`POST /api/cameras/{id}/playlist` decodes the pushed body with
`body.decode("utf-8")` and puts the exception straight into its 400:

    raise HTTPException(400, detail=f"Invalid playlist content: {e}")

So the message is part of the response, and it is CPython's, not
Rust's: which byte it names, which position, how many bytes the bad
sequence spans, and which of the three reasons it gives. Rust's
`from_utf8` reports an error with an offset and a length, but not the
same words and not always the same span.

Every case is one body: what `decode` did with it, and — when it
failed — `str(e)` exactly.

Usage: backend/.venv/bin/python tests/differential/gen_utf8_corpus.py
"""
import itertools
import json
import pathlib
import random

random.seed(20260920)  # reproducible: a flaky corpus is not evidence

PREFIXES = [b"", b"a", b"#EXTM3U\n", b"\xc3\xa9", b"\xe2\x82\xac", b"\xf0\x9f\x98\x80"]

# Lead bytes that stand for each branch of the decoder, and the
# continuations that make each branch succeed or fail in its own way.
LEADS = [
    *range(0x00, 0x02),   # ASCII
    0x7F, 0x80, 0x81, 0xBF,  # continuation bytes with nothing to continue
    0xC0, 0xC1,           # overlong two-byte leads, rejected outright
    0xC2, 0xDF,           # two-byte
    0xE0, 0xE1, 0xEC, 0xED, 0xEE, 0xEF,  # three-byte, incl. the two special cases
    0xF0, 0xF1, 0xF4, 0xF5, 0xFE, 0xFF,  # four-byte and beyond the range
]
TAILS = [b"", b"\x80", b"\xbf", b"\x41", b"\xc0", b"\x80\x80", b"\x80\x41",
         b"\xa0\x80", b"\x9f\x80", b"\x80\x80\x80", b"\x8f\x80\x80"]


def outcome(raw: bytes) -> dict:
    try:
        raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        return {"error": str(exc)}
    return {"ok": True}


def main() -> None:
    bodies: list[bytes] = []
    # Every single byte on its own.
    bodies += [bytes([b]) for b in range(256)]
    # Each lead against each tail, bare and behind a prefix that shifts
    # the reported position.
    for prefix, lead, tail in itertools.product(PREFIXES, LEADS, TAILS):
        bodies.append(prefix + bytes([lead]) + tail)
    # Truncations of valid multi-byte characters, which is the
    # "unexpected end of data" branch.
    for char in ("é", "€", "😀", "߿", "￿", "\U0010ffff"):
        encoded = char.encode()
        for cut in range(1, len(encoded)):
            bodies.append(encoded[:cut])
            bodies.append(b"ok" + encoded[:cut])
    # Surrogates and other encodings that never appear in valid UTF-8.
    bodies += [b"\xed\xa0\x80", b"\xed\xbf\xbf", b"\xc0\x80", b"\xe0\x80\x80",
               b"\xf0\x80\x80\x80", b"\xf4\x90\x80\x80", b"\xf7\xbf\xbf\xbf"]
    # A realistic playlist with one bad byte somewhere in it.
    playlist = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:1.0,\nsegment_00001.ts\n"
    for pos in (0, 7, 20, len(playlist) - 1):
        bodies.append(playlist[:pos] + b"\xff" + playlist[pos:])
    # Fuzz, for the shapes none of the above thought of.
    for _ in range(3000):
        n = random.randint(1, 12)
        bodies.append(bytes(random.randrange(256) for _ in range(n)))

    seen, corpus = set(), []
    for raw in bodies:
        if raw in seen:
            continue
        seen.add(raw)
        corpus.append({"body": raw.hex(), **outcome(raw)})

    out = pathlib.Path(__file__).resolve().parents[2] / "tests" / "fixtures" / "utf8_corpus.json"
    out.write_text(json.dumps(corpus, indent=0) + "\n")
    bad = sum("error" in c for c in corpus)
    reasons = sorted({c["error"].split(": ")[-1] for c in corpus if "error" in c})
    print(f"wrote {len(corpus)} bodies ({bad} rejected) to {out}")
    print("reasons:", reasons)


if __name__ == "__main__":
    main()
