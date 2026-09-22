//! Python's spelling of values, where a response or a stored string
//! interpolates one.
//!
//! Python handlers routinely put a value into a message with an
//! f-string or `{x!r}`, and the result is part of the response body the
//! differential compares: `f"Node '{node_id}' not found"` with a list
//! for `node_id` produces `Node '['a', None]' not found`. JSON has no
//! opinion about any of that, so the spelling is reproduced here.

use serde_json::Value;

/// Python's truthiness for a value that arrived as JSON.
///
/// `if not x` in a handler is a type-blind check: `0`, `0.0`, `false`,
/// `""`, `[]`, `{}` and `null` are all falsy, and a port that only
/// checks for a missing key or an empty string disagrees on the rest.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `repr()` of a `str`.
///
/// Single quotes unless the text contains a single quote and no double
/// quote. Backslash, the chosen quote, and control characters are
/// escaped; other characters — non-ASCII letters included — are left
/// literal, as Python does for printable text.
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (0x7f..=0xa0).contains(&(c as u32)) => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `repr()` of a float, which differs from JSON's spelling: `3.0` keeps
/// its `.0`, and the exponent form carries a sign and at least two
/// digits (`1e+20`, `1e-05`). Python switches to exponent form below
/// 1e-4 and at 1e16 and above.
pub fn repr_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // `{:e}` gives the shortest round-tripping digits, which is what
    // Python's repr uses too; only the layout differs.
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();

    if (-4..16).contains(&exp) {
        let point = exp + 1; // position of the decimal point in `digits`
        let body = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
        } else {
            format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
        };
        return if negative { format!("-{body}") } else { body };
    }
    let m = if digits.len() > 1 {
        format!("{}.{}", &digits[..1], &digits[1..])
    } else {
        digits
    };
    let sign = if exp < 0 { '-' } else { '+' };
    format!("{}{m}e{sign}{:02}", if negative { "-" } else { "" }, exp.abs())
}

/// `repr()` of a value that arrived as JSON — what `str(list)` and
/// `str(dict)` use for their elements.
pub fn repr_value(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => {
            if n.is_f64() {
                repr_float(n.as_f64().unwrap_or_default())
            } else {
                n.to_string()
            }
        }
        Value::String(s) => repr_str(s),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(repr_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", repr_str(k), repr_value(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

/// `str()` of a value that arrived as JSON: identical to `repr()`
/// except that a string is itself, unquoted.
pub fn str_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => repr_value(other),
    }
}

/// `len()` of a value that arrived as JSON, or `None` where Python
/// raises `TypeError` — numbers, booleans and null have no length.
pub fn len(value: &Value) -> Option<usize> {
    match value {
        Value::String(s) => Some(s.chars().count()),
        Value::Array(a) => Some(a.len()),
        Value::Object(o) => Some(o.len()),
        _ => None,
    }
}

/// `needle in value` for a string needle: substring for a string,
/// element equality for a list, key membership for a dict. `None` where
/// Python raises.
pub fn contains_str(value: &Value, needle: &str) -> Option<bool> {
    match value {
        Value::String(s) => Some(s.contains(needle)),
        Value::Array(a) => Some(a.iter().any(|v| v.as_str() == Some(needle))),
        Value::Object(o) => Some(o.contains_key(needle)),
        _ => None,
    }
}

/// `round(x, 2)`.
///
/// Python rounds to the nearest representable double of the correctly
/// rounded decimal, ties to even — not the `(x * 100).round() / 100`
/// that rounds halves away from zero. Formatting to two places and
/// reading it back gives the same double, because both sides round the
/// exact binary value the same way.
pub fn round_half_even(x: f64) -> f64 {
    round_to(x, 2)
}

/// `round(x, digits)` — the same reasoning as `round_half_even`, for
/// the places that want one decimal (a disk-usage percentage, a size
/// in GB) rather than two.
pub fn round_to(x: f64, digits: usize) -> f64 {
    format!("{x:.digits$}").parse().unwrap_or(x)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_two_places_like_python() {
        // Values where half-away-from-zero and Python's round() differ.
        // `round(2.675, 2)` is 2.67 because the double nearest 2.675 is
        // a hair below it.
        assert_eq!(round_half_even(2.675), 2.67);
        assert_eq!(round_half_even(0.125), 0.12);
        assert_eq!(round_half_even(0.135), 0.14);
        assert_eq!(round_half_even(1.0 / 3.0), 0.33);
        assert_eq!(round_half_even(0.0), 0.0);
        assert_eq!(round_half_even(2.0), 2.0);
    }

    fn corpus() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/pyrepr_corpus.json")).unwrap()
    }

    #[test]
    fn repr_and_str_match_cpython_for_every_corpus_value() {
        // Generated by the backend's own interpreter. The awkward rows
        // are the point: 1e16 switches to exponent form, 1e-05 pads its
        // exponent, 3.0 keeps its ".0", and a string holding a single
        // quote and no double quote switches to double quotes.
        let corpus = corpus();
        let values = corpus["values"].as_array().unwrap();
        assert!(values.len() > 30, "corpus shrank");
        for row in values {
            let v = &row["value"];
            assert_eq!(repr_value(v), row["repr"].as_str().unwrap(), "repr of {v}");
            assert_eq!(str_value(v), row["str"].as_str().unwrap(), "str of {v}");
        }
    }

    #[test]
    fn truthiness_is_python_s_not_json_s() {
        for falsy in [json_null(), Value::Bool(false), 0.into(), 0.0.into(), "".into(),
                      Value::Array(vec![]), Value::Object(Default::default())] {
            assert!(!truthy(&falsy), "{falsy} should be falsy");
        }
        for t in [Value::Bool(true), 1.into(), (-0.5).into(), "0".into(), " ".into(),
                  Value::Array(vec![Value::Null])] {
            assert!(truthy(&t), "{t} should be truthy");
        }
    }

    #[test]
    fn len_and_in_follow_the_operand_type() {
        // `len(5)` raises; `"\n" in ["\n"]` is element equality;
        // `"\n" in {"\n": 1}` is key membership.
        assert_eq!(len(&5.into()), None);
        assert_eq!(len(&Value::Bool(true)), None);
        assert_eq!(len(&"caf\u{e9}".into()), Some(4), "characters, not bytes");
        assert_eq!(contains_str(&serde_json::json!(["\n"]), "\n"), Some(true));
        assert_eq!(contains_str(&serde_json::json!(["a\nb"]), "\n"), Some(false));
        assert_eq!(contains_str(&serde_json::json!({"\n": 1}), "\n"), Some(true));
        assert_eq!(contains_str(&5.into(), "\n"), None);
    }

    fn json_null() -> Value {
        Value::Null
    }
}
