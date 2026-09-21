//! CPython's UTF-8 decode errors, because one route puts them on the
//! wire.
//!
//! `POST /api/cameras/{id}/playlist` does
//!
//! ```python
//! try:
//!     playlist_content = body.decode("utf-8")
//! except UnicodeDecodeError as e:
//!     raise HTTPException(400, detail=f"Invalid playlist content: {e}")
//! ```
//!
//! so the message is the response. Rust's `str::from_utf8` reports an
//! error too, but not in the same words, and not always over the same
//! span: CPython names the *first* offending byte for an invalid start,
//! and the whole partial sequence for a bad continuation, and
//! distinguishes a sequence that is merely cut short at the end of the
//! input ("unexpected end of data") from one that is wrong in the
//! middle ("invalid continuation byte").
//!
//! The decoder below follows `_PyUnicode_DecodeUTF8Stateful`'s branches
//! — the same lead-byte ranges, the same constraints on the first
//! continuation of a three- or four-byte sequence — and reports the
//! spans it reports. `tests/fixtures/utf8_corpus.json`, from
//! `tests/differential/gen_utf8_corpus.py`, holds it to the
//! interpreter over every single byte, every lead-and-tail pair, every
//! truncation of a multi-byte character, and three thousand fuzzed
//! bodies.

/// `str(UnicodeDecodeError)` for a body that does not decode, or `None`
/// when it does.
pub fn utf8_decode_error(bytes: &[u8]) -> Option<String> {
    let (start, end, reason) = find_error(bytes)?;
    Some(format_error(bytes, start, end, reason))
}

/// `'utf-8' codec can't decode byte|bytes ...`, as
/// `UnicodeDecodeError.__str__` builds it: one byte is named and shown,
/// a longer span is only located.
fn format_error(bytes: &[u8], start: usize, end: usize, reason: &str) -> String {
    if end == start + 1 {
        format!(
            "'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}",
            bytes[start]
        )
    } else {
        format!("'utf-8' codec can't decode bytes in position {start}-{}: {reason}", end - 1)
    }
}

/// The first ill-formed sequence: where it starts, where CPython ends
/// it, and why.
fn find_error(bytes: &[u8]) -> Option<(usize, usize, &'static str)> {
    const START: &str = "invalid start byte";
    const CONTINUATION: &str = "invalid continuation byte";
    const TRUNCATED: &str = "unexpected end of data";

    let mut i = 0;
    while i < bytes.len() {
        let lead = bytes[i];
        // How many continuation bytes this lead calls for, and what the
        // first of them is allowed to be — the two extra constraints
        // are what keep out surrogates (0xed) and the overlong or
        // out-of-range four-byte forms (0xf0, 0xf4).
        let (needed, first_range) = match lead {
            0x00..=0x7f => {
                i += 1;
                continue;
            }
            0xc2..=0xdf => (1, 0x80..=0xbf),
            0xe0 => (2, 0xa0..=0xbf),
            0xe1..=0xec | 0xee..=0xef => (2, 0x80..=0xbf),
            0xed => (2, 0x80..=0x9f),
            0xf0 => (3, 0x90..=0xbf),
            0xf1..=0xf3 => (3, 0x80..=0xbf),
            0xf4 => (3, 0x80..=0x8f),
            // A bare continuation byte, an overlong two-byte lead
            // (0xc0, 0xc1), or anything past the end of the encoding.
            _ => return Some((i, i + 1, START)),
        };

        for step in 1..=needed {
            let Some(&byte) = bytes.get(i + step) else {
                // Cut short by the end of the input rather than wrong:
                // the span is what there was.
                return Some((i, bytes.len(), TRUNCATED));
            };
            let ok = if step == 1 {
                first_range.contains(&byte)
            } else {
                (0x80..=0xbf).contains(&byte)
            };
            if !ok {
                // The span covers the lead and every continuation that
                // was acceptable, so a wrong second byte reports one
                // byte and a wrong third reports two.
                return Some((i, i + step, CONTINUATION));
            }
        }
        i += needed + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn matches_cpython_corpus() {
        let raw = include_str!("../tests/fixtures/utf8_corpus.json");
        let corpus: Vec<Value> = serde_json::from_str(raw).unwrap();
        assert!(corpus.len() > 4000, "corpus shrank to {}", corpus.len());

        let mut failures = Vec::new();
        for case in &corpus {
            let hex = case["body"].as_str().unwrap();
            let body: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            let want = case["error"].as_str();
            let got = utf8_decode_error(&body);
            if got.as_deref() != want {
                failures.push(format!("{hex}: want {want:?} got {got:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} disagree with CPython:\n{}",
            failures.len(),
            corpus.len(),
            failures.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
        );
    }

    /// The decoder agrees with Rust's about *whether* a body decodes;
    /// only the words and spans differ.
    #[test]
    fn agrees_with_rust_on_validity() {
        let raw = include_str!("../tests/fixtures/utf8_corpus.json");
        let corpus: Vec<Value> = serde_json::from_str(raw).unwrap();
        for case in &corpus {
            let hex = case["body"].as_str().unwrap();
            let body: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            assert_eq!(
                utf8_decode_error(&body).is_none(),
                std::str::from_utf8(&body).is_ok(),
                "{hex}"
            );
        }
    }
}
