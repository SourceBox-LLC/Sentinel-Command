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

// ---------------------------------------------------------------------
// The builtin `int()`, which is not the same function as the above
// ---------------------------------------------------------------------

/// `int(s)` for a string — the *builtin*, not Pydantic's coercion.
///
/// The two differ, and the difference is reachable: `int("1_0")` is 10,
/// because Python's integer literal grammar allows single underscores
/// between digits, and Pydantic's string coercion does not accept them.
/// Anywhere the Python source calls `int()` directly — the motion
/// score, the segment sequence, the hidden cooldown setting — this is
/// the function it called.
///
/// `None` is every case Python raises `ValueError` for.
pub fn python_int(s: &str) -> Option<PyInt> {
    // `int()` strips the same whitespace `str.strip()` does, which is
    // Unicode whitespace rather than just ASCII.
    let s = s.trim_matches(char::is_whitespace);
    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if digits.is_empty() {
        return None;
    }

    // An underscore is allowed only *between* digits: never leading,
    // never trailing, never doubled.
    let mut clean = String::with_capacity(digits.len());
    let mut previous_was_digit = false;
    for ch in digits.chars() {
        if ch == '_' {
            if !previous_was_digit {
                return None;
            }
            previous_was_digit = false;
            continue;
        }
        if !ch.is_ascii_digit() {
            return None;
        }
        clean.push(ch);
        previous_was_digit = true;
    }
    if !previous_was_digit {
        // Ended on an underscore.
        return None;
    }

    Some(match clean.parse::<i64>() {
        Ok(value) => PyInt::Small(if negative { -value } else { value }),
        // Past i64 in one direction or the other. Python has no such
        // limit, so the sign is all a caller can act on.
        Err(_) => {
            if negative {
                PyInt::Big { negative: true }
            } else {
                PyInt::Big { negative: false }
            }
        }
    })
}

/// `int(value)` where the value came out of a JSON body.
///
/// `None` is every case Python raises for — `ValueError` on an
/// unparseable string, `TypeError` on a list, a mapping or `None`.
/// A float truncates *toward zero*, which is what `int()` does and
/// what `floor` does not.
pub fn python_int_of_json(value: &serde_json::Value) -> Option<PyInt> {
    match value {
        // `int(True)` is 1. JSON booleans reach this because a node
        // sends what it sends.
        serde_json::Value::Bool(b) => Some(PyInt::Small(i64::from(*b))),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Some(PyInt::Small(i));
            }
            if let Some(u) = n.as_u64() {
                return Some(i64::try_from(u).map_or(PyInt::Big { negative: false }, PyInt::Small));
            }
            let f = n.as_f64()?;
            if !f.is_finite() {
                // `int(nan)` raises ValueError and `int(inf)` raises
                // OverflowError — neither of which the motion path
                // catches, so both are a 500 there. serde_json cannot
                // even represent them, so this is unreachable from a
                // parsed body and defensive only.
                return None;
            }
            let truncated = f.trunc();
            if truncated >= i64::MIN as f64 && truncated <= i64::MAX as f64 {
                Some(PyInt::Small(truncated as i64))
            } else {
                Some(PyInt::Big { negative: truncated < 0.0 })
            }
        }
        serde_json::Value::String(s) => python_int(s),
        // `int(None)`, `int([])`, `int({})` are all TypeError.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    /// Every value here was run through the interpreter, including the
    /// ones that raise.
    #[test]
    fn the_builtin_int_is_not_pydantics() {
        use super::{python_int, python_int_of_json, PyInt};
        use serde_json::json;

        let small = |n: i64| Some(PyInt::Small(n));
        // Underscores between digits, which is the difference that
        // makes this a separate function from `str_as_int`.
        assert_eq!(python_int("1_0"), small(10));
        assert_eq!(python_int("1_000_000"), small(1_000_000));
        // ...but never leading, trailing or doubled.
        for bad in ["_1", "1_", "1__0", "_", "-_1"] {
            assert_eq!(python_int(bad), None, "{bad:?}");
        }
        // Whitespace and a sign, as `int()` accepts them.
        assert_eq!(python_int(" 50 "), small(50));
        assert_eq!(python_int("+7"), small(7));
        assert_eq!(python_int("-7"), small(-7));
        assert_eq!(python_int("\t\n 12 \r"), small(12));
        // ValueError cases.
        for bad in ["", " ", "3.9", "abc", "0x10", "1e3", "- 1", "1 2"] {
            assert_eq!(python_int(bad), None, "{bad:?}");
        }
        // Past i64 keeps only its sign, which is all a caller can use.
        assert_eq!(python_int(&"9".repeat(30)), Some(PyInt::Big { negative: false }));
        assert_eq!(python_int(&format!("-{}", "9".repeat(30))), Some(PyInt::Big { negative: true }));

        // And over a JSON value, where the type decides.
        assert_eq!(python_int_of_json(&json!(50)), small(50));
        // Truncation is toward zero, not floor — the two differ for
        // negatives, and `int()` does the former.
        assert_eq!(python_int_of_json(&json!(3.9)), small(3));
        assert_eq!(python_int_of_json(&json!(-3.9)), small(-3));
        assert_eq!(python_int_of_json(&json!(true)), small(1));
        assert_eq!(python_int_of_json(&json!(false)), small(0));
        assert_eq!(python_int_of_json(&json!("50")), small(50));
        // TypeError and ValueError alike come back as None, because the
        // one caller catches both and does the same thing.
        for bad in [json!(null), json!([]), json!({}), json!("3.9"), json!("abc")] {
            assert_eq!(python_int_of_json(&bad), None, "{bad}");
        }
    }

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
