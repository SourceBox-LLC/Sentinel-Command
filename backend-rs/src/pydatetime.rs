//! CPython 3.12's `datetime.fromisoformat`, and the one conversion the
//! routes do with its result.
//!
//! A port of the C accelerator in `Modules/_datetimemodule.c`, which is
//! what `datetime.fromisoformat` runs — not `_pydatetime.py`, which is
//! only the fallback and disagrees with it. The C:
//!
//! * accepts a colon after the seconds and reads what follows as the
//!   fraction, so `15:00:00:00` parses, and so does `15.00`;
//! * never looks at the character in the separator position, so any
//!   code point works there, `X` and `é` included;
//! * parses ASCII digits only, where `int()` in the reference would
//!   take `١`;
//! * stops at an embedded NUL as though the string had ended, because
//!   it walks a NUL-terminated UTF-8 buffer — so text after the NUL can
//!   be ignored entirely;
//! * range-checks neither the hours nor the minutes of an offset, only
//!   the total, so `+05:99` is a valid zone.
//!
//! The functions below follow the C one for one, over a byte slice
//! whose reads past the end return 0, which is what the terminator
//! gives the C. Each keeps its C return codes, because the callers
//! branch on their signs. `tests/fixtures/fromisoformat_corpus.json`,
//! from `tests/differential/gen_fromisoformat_corpus.py`, holds the
//! port to the interpreter.
//!
//! One input the C handles cannot reach Rust: a lone surrogate in the
//! separator position, which the C swaps for `T`. A `&str` cannot hold
//! one, and the only caller's input is a query string, which Starlette
//! decodes as UTF-8 with replacement — so Python cannot see one there
//! either.

use chrono::{Duration, NaiveDate, NaiveDateTime};

/// A parsed value: naive fields plus the offset, when there was one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoDateTime {
    pub naive: NaiveDateTime,
    /// Total UTC offset in microseconds; `None` for a naive result.
    pub offset_us: Option<i64>,
}

/// The exception a Python caller would see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyDateError {
    /// Unparseable, a field out of range, or an offset of 24 hours or
    /// more. Every one of these is a ValueError in Python.
    Value,
    /// `astimezone` stepped outside years 1–9999. OverflowError, which
    /// is *not* a ValueError — callers catching ValueError let it
    /// through.
    Overflow,
}

/// The C string as it sees it: reads at or past the end give the
/// terminator.
struct CStr<'a>(&'a [u8]);

impl CStr<'_> {
    fn at(&self, i: usize) -> u8 {
        self.0.get(i).copied().unwrap_or(0)
    }
}

/// `is_digit`: `(unsigned int)(c - '0') < 10`. ASCII only — a UTF-8
/// lead byte is a negative `char`, which wraps far above 9.
fn is_digit(c: u8) -> bool {
    c.wrapping_sub(b'0') < 10
}

/// `parse_digits`: `num_digits` ASCII digits into `var`, or `None` at
/// the first non-digit. Returns the position after them.
fn parse_digits(s: &CStr, mut pos: usize, var: &mut i32, num_digits: usize) -> Option<usize> {
    for _ in 0..num_digits {
        let tmp = s.at(pos).wrapping_sub(b'0');
        pos += 1;
        if tmp > 9 {
            return None;
        }
        *var = *var * 10 + i32::from(tmp);
    }
    Some(pos)
}

fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Monday = 0, matching the C `weekday`.
fn weekday(year: i32, month: u32, day: u32) -> u32 {
    NaiveDate::from_ymd_opt(year, month, day)
        .map(|d| chrono::Datelike::weekday(&d).num_days_from_monday())
        .unwrap_or(0)
}

/// `iso_to_ymd`. Its return codes (-2, -3, -4) are offset by the
/// caller into its own.
fn iso_to_ymd(iso_year: i32, iso_week: i32, iso_day: i32) -> Result<(i32, u32, u32), i32> {
    if !(1..=9999).contains(&iso_year) {
        return Err(-4);
    }
    if iso_week <= 0 || iso_week >= 53 {
        let mut out_of_range = true;
        if iso_week == 53 {
            // 53 weeks in years starting on a Thursday, and in leap
            // years starting on a Wednesday.
            let first_weekday = weekday(iso_year, 1, 1);
            if first_weekday == 3 || (first_weekday == 2 && is_leap(iso_year)) {
                out_of_range = false;
            }
        }
        if out_of_range {
            return Err(-2);
        }
    }
    if iso_day <= 0 || iso_day >= 8 {
        return Err(-3);
    }
    // `iso_week1_monday` plus the offset, done on proleptic Gregorian
    // ordinals as the C does. The year is 1–9999 and the week and day
    // are bounded, so the result is at most a week outside the year
    // and always representable.
    let jan1 = NaiveDate::from_ymd_opt(iso_year, 1, 1).expect("year checked above");
    let first_weekday = i64::from(chrono::Datelike::weekday(&jan1).num_days_from_monday());
    let mut week1_monday = jan1 - Duration::days(first_weekday);
    if first_weekday > 3 {
        week1_monday += Duration::days(7);
    }
    let date = week1_monday + Duration::days(i64::from((iso_week - 1) * 7 + iso_day - 1));
    Ok((
        chrono::Datelike::year(&date),
        chrono::Datelike::month(&date),
        chrono::Datelike::day(&date),
    ))
}

/// `parse_isoformat_date`. `len` is the separator location, and only
/// the week branch consults it.
fn parse_isoformat_date(s: &CStr, len: isize) -> Result<(i32, i32, i32), i32> {
    let mut year = 0;
    let mut month = 0;
    let mut day = 0;
    let mut p = parse_digits(s, 0, &mut year, 4).ok_or(-1)?;

    let uses_separator = s.at(p) == b'-';
    if uses_separator {
        p += 1;
    }

    if s.at(p) == b'W' {
        p += 1;
        let mut iso_week = 0;
        let mut iso_day = 0;
        p = parse_digits(s, p, &mut iso_week, 2).ok_or(-3)?;
        // `(size_t)(p - dtstr) < len` — with `len` unsigned, so a
        // separator location of -1 compares as the largest value.
        if len < 0 || (p as isize) < len {
            if uses_separator {
                let c = s.at(p);
                p += 1;
                if c != b'-' {
                    return Err(-2);
                }
            }
            parse_digits(s, p, &mut iso_day, 1).ok_or(-4)?;
        } else {
            iso_day = 1;
        }
        return match iso_to_ymd(year, iso_week, iso_day) {
            Ok((y, m, d)) => Ok((y, m as i32, d as i32)),
            Err(rv) => Err(-3 + rv),
        };
    }

    p = parse_digits(s, p, &mut month, 2).ok_or(-1)?;
    if uses_separator {
        let c = s.at(p);
        p += 1;
        if c != b'-' {
            return Err(-2);
        }
    }
    parse_digits(s, p, &mut day, 2).ok_or(-1)?;
    Ok((year, month, day))
}

/// `parse_hh_mm_ss_ff`: `Ok(0)` at the end of the string, `Ok(1)` when
/// something non-NUL follows, `Err` with the C's negative code.
fn parse_hh_mm_ss_ff(s: &CStr, start: usize, end: usize) -> Result<(i32, [i32; 4]), i32> {
    // hour, minute, second, microsecond
    let mut vals = [0i32; 4];
    let mut p = start;
    let mut has_separator = true;

    for (i, val) in vals.iter_mut().take(3).enumerate() {
        p = parse_digits(s, p, val, 2).ok_or(-3)?;
        let c = s.at(p);
        p += 1;
        if i == 0 {
            has_separator = c == b':';
        }
        if p >= end {
            return Ok((i32::from(c != 0), vals));
        } else if has_separator && c == b':' {
            continue;
        } else if c == b'.' || c == b',' {
            break;
        } else if !has_separator {
            p -= 1;
        } else {
            return Err(-4);
        }
    }

    // Fractional seconds: at most six digits count, the rest are
    // skipped. Every path here left `p` short of `end`, so there is at
    // least one character to parse.
    let to_parse = (end - p).min(6);
    let mut microsecond = 0;
    p = parse_digits(s, p, &mut microsecond, to_parse).ok_or(-3)?;
    const CORRECTION: [i32; 5] = [100_000, 10_000, 1_000, 100, 10];
    if to_parse < 6 {
        microsecond *= CORRECTION[to_parse - 1];
    }
    vals[3] = microsecond;
    while is_digit(s.at(p)) {
        p += 1;
    }
    Ok((i32::from(s.at(p) != 0), vals))
}

/// What `parse_isoformat_time` fills in.
#[derive(Default)]
struct TimeParts {
    hms_us: [i32; 4],
    tzoffset: i32,
    tzusec: i32,
}

/// `parse_isoformat_time`: `Ok(0)` with no zone, `Ok(1)` with one.
fn parse_isoformat_time(s: &CStr, start: usize, dtlen: usize) -> Result<(i32, TimeParts), i32> {
    let p_end = start + dtlen;
    let mut out = TimeParts::default();

    // The do/while tests the first character even when the time part
    // is empty, and steps one past the end in that case.
    let mut tzinfo_pos = start;
    loop {
        let c = s.at(tzinfo_pos);
        if c == b'Z' || c == b'+' || c == b'-' {
            break;
        }
        tzinfo_pos += 1;
        if tzinfo_pos >= p_end {
            break;
        }
    }

    let (rv, hms_us) = parse_hh_mm_ss_ff(s, start, tzinfo_pos)?;
    out.hms_us = hms_us;
    if tzinfo_pos == p_end {
        // No zone, so anything left over is an error.
        return if rv == 1 { Err(-5) } else { Ok((0, out)) };
    }

    if s.at(tzinfo_pos) == b'Z' {
        return if s.at(tzinfo_pos + 1) != 0 { Err(-5) } else { Ok((1, out)) };
    }

    let tzsign = if s.at(tzinfo_pos) == b'-' { -1 } else { 1 };
    let (rv, tz) = match parse_hh_mm_ss_ff(s, tzinfo_pos + 1, p_end) {
        Ok(ok) => ok,
        Err(_) => return Err(-5),
    };
    out.tzoffset = tzsign * (tz[0] * 3600 + tz[1] * 60 + tz[2]);
    out.tzusec = tzsign * tz[3];
    if rv != 0 {
        Err(-5)
    } else {
        Ok((1, out))
    }
}

/// `_find_isoformat_datetime_separator`, over the UTF-8 bytes.
fn find_separator(s: &CStr, len: usize) -> isize {
    if len == 7 {
        return 7;
    }
    if s.at(4) == b'-' {
        if s.at(5) == b'W' {
            if len < 8 {
                return -1;
            }
            if len > 8 && s.at(8) == b'-' {
                if len == 9 {
                    return -1;
                }
                if len > 10 && is_digit(s.at(10)) {
                    // YYYY-Www-## is ambiguous; the C bets on a hyphen
                    // separator at 8.
                    return 8;
                }
                return 10;
            }
            return 8;
        }
        return 10;
    }
    if s.at(4) == b'W' {
        let mut idx = 7;
        while idx < len && is_digit(s.at(idx)) {
            idx += 1;
        }
        if idx < 9 {
            return idx as isize;
        }
        return if idx % 2 == 0 { 7 } else { 8 };
    }
    8
}

/// `datetime.fromisoformat(s)`.
pub fn fromisoformat(input: &str) -> Result<IsoDateTime, PyDateError> {
    // `_sanitize_isoformat_str` refuses anything under seven code
    // points, before the bytes are looked at.
    if input.chars().count() < 7 {
        return Err(PyDateError::Value);
    }
    let bytes = input.as_bytes();
    let s = CStr(bytes);
    let len = bytes.len();
    let separator = find_separator(&s, len);

    let (year, month, day) = parse_isoformat_date(&s, separator).map_err(|_| PyDateError::Value)?;
    let mut time = TimeParts::default();
    let mut rv = 0;
    if (len as isize) > separator {
        // Skip the separator, however many bytes its code point takes —
        // judged by the lead byte alone, as the C does.
        let mut p = separator as usize;
        let lead = s.at(p);
        p += if lead & 0x80 == 0 {
            1
        } else {
            match lead & 0xf0 {
                0xe0 => 3,
                0xf0 => 4,
                _ => 2,
            }
        };
        let (code, parts) =
            parse_isoformat_time(&s, p, len.saturating_sub(p)).map_err(|_| PyDateError::Value)?;
        rv = code;
        time = parts;
    }

    // `tzinfo_from_isoformat_results`: a zero offset in whole seconds
    // is UTC, whatever the microseconds said. Anything else must be
    // strictly inside ±24h.
    let offset_us = if rv == 1 {
        if time.tzoffset == 0 {
            Some(0)
        } else {
            let total = i64::from(time.tzoffset) * 1_000_000 + i64::from(time.tzusec);
            if total.abs() >= 86_400_000_000 {
                return Err(PyDateError::Value);
            }
            Some(total)
        }
    } else {
        None
    };

    // `new_datetime_subclass_ex`: `check_date_args` then
    // `check_time_args`, both ValueError.
    let [hour, minute, second, microsecond] = time.hms_us;
    let date = u32::try_from(month)
        .ok()
        .zip(u32::try_from(day).ok())
        .filter(|_| (1..=9999).contains(&year))
        .and_then(|(m, d)| NaiveDate::from_ymd_opt(year, m, d))
        .ok_or(PyDateError::Value)?;
    let in_range = |v: i32, max: i32| (0..=max).contains(&v);
    if !(in_range(hour, 23) && in_range(minute, 59) && in_range(second, 59) && in_range(microsecond, 999_999)) {
        return Err(PyDateError::Value);
    }
    let naive = date
        .and_hms_micro_opt(hour as u32, minute as u32, second as u32, microsecond as u32)
        .ok_or(PyDateError::Value)?;

    Ok(IsoDateTime { naive, offset_us })
}

/// `dt.astimezone(UTC).replace(tzinfo=None)` for an aware value, the
/// value itself for a naive one.
///
/// A zero offset is Python's UTC singleton, and converting a datetime
/// to its own zone returns it untouched — so it cannot overflow even at
/// the edges of the calendar. Any other offset is subtracted, and a
/// result outside years 1–9999 is an OverflowError.
pub fn to_naive_utc(dt: IsoDateTime) -> Result<NaiveDateTime, PyDateError> {
    match dt.offset_us {
        None | Some(0) => Ok(dt.naive),
        Some(off) => {
            let shifted = dt
                .naive
                .checked_sub_signed(Duration::microseconds(off))
                .ok_or(PyDateError::Overflow)?;
            if (1..=9999).contains(&chrono::Datelike::year(&shifted)) {
                Ok(shifted)
            } else {
                Err(PyDateError::Overflow)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn fmt(dt: NaiveDateTime) -> String {
        dt.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
    }

    fn error_name(err: PyDateError) -> &'static str {
        match err {
            PyDateError::Value => "ValueError",
            PyDateError::Overflow => "OverflowError",
        }
    }

    /// Every input in the corpus, through `fromisoformat` alone and
    /// through the `since` pipeline, against what CPython 3.12 did.
    #[test]
    fn matches_cpython_corpus() {
        let raw = include_str!("../tests/fixtures/fromisoformat_corpus.json");
        let corpus: Vec<Value> = serde_json::from_str(raw).unwrap();
        assert!(corpus.len() > 4000, "corpus shrank to {}", corpus.len());

        let mut failures = Vec::new();
        for case in &corpus {
            let input = case["in"].as_str().unwrap();

            let got_iso = match fromisoformat(input) {
                Ok(dt) => serde_json::json!({
                    "naive": fmt(dt.naive),
                    "offset_us": dt.offset_us,
                }),
                Err(e) => serde_json::json!({ "error": error_name(e) }),
            };
            if got_iso != case["iso"] {
                failures.push(format!("iso   {input:?}: want {} got {got_iso}", case["iso"]));
            }

            let got_since = match fromisoformat(&input.replace('Z', "+00:00")).and_then(to_naive_utc) {
                Ok(dt) => serde_json::json!({ "utc": fmt(dt) }),
                Err(e) => serde_json::json!({ "error": error_name(e) }),
            };
            if got_since != case["since"] {
                failures.push(format!("since {input:?}: want {} got {got_since}", case["since"]));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} disagree with CPython:\n{}",
            failures.len(),
            corpus.len() * 2,
            failures.iter().take(40).cloned().collect::<Vec<_>>().join("\n")
        );
    }
}
