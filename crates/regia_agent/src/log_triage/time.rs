// SPDX-License-Identifier: MPL-2.0
//! Numeric log times and the RFC 3339 timestamp subset used by engine JSON logs.

use serde_json::Value;

pub(super) fn number(value: &Value) -> Option<f64> {
    let n = value.as_f64().or_else(|| value.as_str()?.trim().parse().ok())?;
    n.is_finite().then_some(n)
}

pub(super) fn timestamp(record: &Value) -> Option<f64> {
    let millis = number(&record["t_ms"])
        .or_else(|| number(&record["t"]).map(|n| n * 1000.0))
        .or_else(|| number(&record["unix_ms"]))
        .or_else(|| {
            ["wall", "timestamp", "time", "ts"].iter().find_map(|key| {
                let value = &record[key];
                number(value)
                    .map(|n| if n.abs() >= 1e11 { n } else { n * 1000.0 })
                    .or_else(|| iso(value.as_str()?))
            })
        })?;
    // Non-finite values are untimed. The finite cap stays above 2^64 so a wide bucket
    // index saturates to u64::MAX instead of being dropped before the cast (ADR-8442).
    (millis.is_finite() && millis.abs() <= 1e20).then_some(millis)
}

fn integer(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    bytes
        .iter()
        .try_fold(0_i64, |n, b| n.checked_mul(10)?.checked_add(i64::from(b - b'0')))
}

fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn iso(text: &str) -> Option<f64> {
    let b = text.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year = integer(&b[..4])?;
    let month = integer(&b[5..7])?;
    let day = integer(&b[8..10])?;
    let hour = integer(&b[11..13])?;
    let minute = integer(&b[14..16])?;
    let second = integer(&b[17..19])?;
    let days = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if year < 1
        || !(1..=12).contains(&month)
        || day < 1
        || day > days[(month - 1) as usize]
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    // Gregorian years preceding y. Differences cancel the arbitrary calendar origin.
    let before = |y: i64| {
        let y = y - 1;
        365 * y + y / 4 - y / 100 + y / 400
    };
    let day_index = before(year) - before(1970) + days[..(month - 1) as usize].iter().sum::<i64>() + day - 1;
    let mut at = 19;
    let mut fraction = 0.0;
    if b.get(at) == Some(&b'.') {
        at += 1;
        let begin = at;
        let mut scale = 0.1;
        while let Some(digit) = b.get(at).filter(|n| n.is_ascii_digit()) {
            fraction += f64::from(digit - b'0') * scale;
            scale *= 0.1;
            at += 1;
        }
        if at == begin {
            return None;
        }
    }
    let zone = &b[at..];
    let offset = match zone {
        [] | [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), a, c, b':', d, e] => {
            let h = integer(&[*a, *c])?;
            let m = integer(&[*d, *e])?;
            if h > 23 || m > 59 {
                return None;
            }
            (h * 3600 + m * 60) * if *sign == b'-' { -1 } else { 1 }
        }
        _ => return None,
    };
    Some(((day_index * 86400 + hour * 3600 + minute * 60 + second - offset) as f64 + fraction) * 1000.0)
}
