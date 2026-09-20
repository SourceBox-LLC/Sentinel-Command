//! Pydantic's string-to-int coercion.
//!
//! Every `int` query parameter goes through this, as does a string sent
//! where a body field wants an int. It is pydantic-core's `str_as_int`
//! (pydantic 2.13.5, pydantic-core 2.46.5) over jiter 0.14's integer
//! parser, ported rather than approximated, because it agrees with
//! neither Python's `int()` nor Rust's `str::parse`:
//!
//! * It tries twice. First the string exactly as sent, as a strict JSON
//!   integer. Failing that, a cleaned copy — trimmed, `+` dropped,
//!   leading zeros and a `.000` tail stripped, digit-group underscores
//!   removed — and any failure there is a plain `int_parsing`.
//! * Past 4,300 characters (sign included) there is `int_parsing_size`,
//!   but only when the *first* attempt gets that far. `"1_"` and 4,300
//!   zeros is `int_parsing`; the same digits without the underscore are
//!   `int_parsing_size`.
//! * Integers outside i64 are ordinary Python ints. Rust cannot hold
//!   them, but the only thing a caller does with one is compare it to a
//!   bound or fail on it later, so [`PyInt::Big`] keeps just the sign.
//!
//! `tests/fixtures/pyint_corpus.json`, from
//! `tests/differential/gen_pyint_corpus.py`, holds this to the library.

/// An integer Pydantic accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyInt {
    Small(i64),
    /// Outside i64. Positive values exceed every upper bound, negative
    /// ones fall below every lower bound.
    Big { negative: bool },
}

impl PyInt {
    /// `value >= ge`.
    pub fn ge(self, bound: i64) -> bool {
        match self {
            PyInt::Small(v) => v >= bound,
            PyInt::Big { negative } => !negative,
        }
    }

    /// `value <= le`.
    pub fn le(self, bound: i64) -> bool {
        match self {
            PyInt::Small(v) => v <= bound,
            PyInt::Big { negative } => negative,
        }
    }

    pub fn small(self) -> Option<i64> {
        match self {
            PyInt::Small(v) => Some(v),
            PyInt::Big { .. } => None,
        }
    }

    /// `max(0, value)`: a big negative int clamps to zero like any
    /// other, while a big positive one is still beyond i64 and `None`.
    pub fn max_zero(self) -> Option<i64> {
        match self {
            PyInt::Small(v) => Some(v.max(0)),
            PyInt::Big { negative: true } => Some(0),
            PyInt::Big { negative: false } => None,
        }
    }

    /// Python truthiness: zero is falsy, and a big int never is.
    pub fn truthy(self) -> bool {
        self != PyInt::Small(0)
    }
}

/// Pydantic's two ways of refusing a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrIntError {
    /// `int_parsing`
    Parsing,
    /// `int_parsing_size`
    ParsingSize,
}

/// What jiter's `NumberInt::try_from` can fail with, as far as
/// `str_as_int` cares: out of range, or anything else.
enum JiterError {
    OutOfRange,
    Other,
}

/// The longest integer jiter builds, counting a leading `-`.
const MAX_INT_CHARS: usize = 4300;

/// jiter's `NumberInt::try_from(&[u8])`: the whole input must be one
/// JSON integer — no `+`, no leading zero before another digit, no
/// fraction or exponent, nothing trailing.
///
/// jiter walks the digits in chunks and checks the length after each
/// one, but only once the value has outgrown the first 19 digits; the
/// check fires before it looks at what ends the digits. So an overlong
/// run of digits is out of range even when a `.` or junk follows it.
fn jiter_int(bytes: &[u8]) -> Result<PyInt, JiterError> {
    let Some(&first) = bytes.first() else {
        return Err(JiterError::Other);
    };
    let negative = first == b'-';
    let digits_start = usize::from(negative);

    match bytes.get(digits_start) {
        // A lone zero, which must not be followed by another digit.
        Some(b'0') => {
            return match bytes.get(digits_start + 1) {
                None => Ok(PyInt::Small(0)),
                // `0.`, `0e`, `01`, or `0` then anything: not an integer
                // that consumes the whole input.
                Some(_) => Err(JiterError::Other),
            };
        }
        Some(b'1'..=b'9') => {}
        // `N`, `I`, a second sign, anything else, or nothing at all.
        _ => return Err(JiterError::Other),
    }

    let digits_end = bytes[digits_start..]
        .iter()
        .position(|b| !b.is_ascii_digit())
        .map_or(bytes.len(), |p| digits_start + p);
    let digit_count = digits_end - digits_start;

    // The first digit and 18 more are read before jiter decides the
    // value needs a BigInt; only on that path is the length checked.
    if digit_count >= 19 && digits_end > MAX_INT_CHARS {
        return Err(JiterError::OutOfRange);
    }
    if digits_end != bytes.len() {
        return Err(JiterError::Other);
    }

    let text = std::str::from_utf8(&bytes[..digits_end]).expect("ASCII digits and a sign");
    Ok(match text.parse::<i64>() {
        Ok(v) => PyInt::Small(v),
        Err(_) => PyInt::Big { negative },
    })
}

/// pydantic-core's `strip_leading_zeros`: keeps one zero for an
/// all-zero number and the zero before a `.`, and treats underscores
/// among the zeros as part of them.
fn strip_leading_zeros(s: &str) -> Option<&str> {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, '0')) => {}
        Some((_, c)) if ('1'..='9').contains(&c) || c == '-' => return Some(s),
        _ => return None,
    }
    for (i, c) in chars {
        match c {
            '0' | '_' => {}
            '1'..='9' | '-' => return Some(&s[i..]),
            // The byte before is a `0` or `_`, both one byte wide.
            '.' => return Some(&s[i - 1..]),
            _ => return None,
        }
    }
    Some(&s[s.len() - 1..])
}

/// pydantic-core's `strip_underscores`: only when there are some, none
/// at either end, and none doubled.
fn strip_underscores(s: &str) -> Option<String> {
    if s.starts_with('_') || s.ends_with('_') || !s.contains('_') || s.contains("__") {
        None
    } else {
        Some(s.replace('_', ""))
    }
}

/// pydantic-core's `clean_int_str`: `None` when there was nothing to
/// clean, which is itself a parsing error.
fn clean_int_str(original: &str) -> Option<String> {
    let len_before = original.len();
    let mut s = original.trim();

    if let Some(rest) = s.strip_prefix('+') {
        if rest.starts_with('-') {
            return None;
        }
        s = rest;
    }

    let mut negative = false;
    if let Some(rest) = s.strip_prefix('-') {
        if rest.starts_with('-') || rest.starts_with('+') {
            return None;
        }
        negative = true;
        s = rest;
    }

    s = strip_leading_zeros(s)?;

    if let Some(i) = s.find('.') {
        let decimal = &s[i + 1..];
        if !decimal.is_empty() && decimal.chars().all(|c| c == '0') {
            s = &s[..i];
        }
    }

    let sign = if negative { "-" } else { "" };
    if let Some(stripped) = strip_underscores(s) {
        return Some(format!("{sign}{stripped}"));
    }
    // Compared against the untrimmed original, sign included: a string
    // that only lost its minus sign still counts as changed.
    if len_before == s.len() {
        return None;
    }
    Some(format!("{sign}{s}"))
}

/// pydantic-core's `str_as_int`.
pub fn str_as_int(s: &str) -> Result<PyInt, StrIntError> {
    match jiter_int(s.as_bytes()) {
        Ok(v) => return Ok(v),
        Err(JiterError::OutOfRange) => return Err(StrIntError::ParsingSize),
        Err(JiterError::Other) => {}
    }
    let cleaned = clean_int_str(s).ok_or(StrIntError::Parsing)?;
    jiter_int(cleaned.as_bytes()).map_err(|_| StrIntError::Parsing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn matches_pydantic_corpus() {
        let raw = include_str!("../tests/fixtures/pyint_corpus.json");
        let corpus: Vec<Value> = serde_json::from_str(raw).unwrap();
        assert!(corpus.len() > 5000, "corpus shrank to {}", corpus.len());

        let mut failures = Vec::new();
        for case in &corpus {
            let input = case["in"].as_str().unwrap();
            let got = match str_as_int(input) {
                Ok(PyInt::Small(v)) => serde_json::json!({ "int": v.to_string() }),
                Ok(PyInt::Big { negative }) => serde_json::json!({ "big": if negative { "-" } else { "+" } }),
                Err(StrIntError::Parsing) => serde_json::json!({ "error": "int_parsing" }),
                Err(StrIntError::ParsingSize) => serde_json::json!({ "error": "int_parsing_size" }),
            };
            let mut want = case.as_object().unwrap().clone();
            want.remove("in");
            if got != Value::Object(want.clone()) {
                let shown: String = input.chars().take(40).collect();
                failures.push(format!("{shown:?} (len {}): want {want:?} got {got}", input.len()));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} disagree with pydantic:\n{}",
            failures.len(),
            corpus.len(),
            failures.iter().take(40).cloned().collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn bounds_treat_big_ints_by_sign() {
        let big = PyInt::Big { negative: false };
        let neg = PyInt::Big { negative: true };
        assert!(big.ge(0) && !big.le(i64::MAX));
        assert!(!neg.ge(i64::MIN) && neg.le(0));
        assert!(big.truthy() && neg.truthy() && !PyInt::Small(0).truthy());
        assert_eq!(neg.max_zero(), Some(0));
        assert_eq!(big.max_zero(), None);
        assert_eq!(PyInt::Small(-7).max_zero(), Some(0));
        assert_eq!(PyInt::Small(7).max_zero(), Some(7));
    }
}
