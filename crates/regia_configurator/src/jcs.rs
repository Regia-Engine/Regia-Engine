// SPDX-License-Identifier: MPL-2.0
//! RFC 8785 JSON Canonicalization Scheme (JCS): the byte form the contract hashes are taken over
//! (`build_hash`, `package_hash`), so a Python producer and Regia
//! agree byte for byte.
//!
//! Written from RFC 8785 (2020): object members sorted by the UTF-16 code units of their names,
//! no whitespace, strings escaped as ECMAScript `JSON.stringify` does (only `"`, `\`, and the
//! control characters; everything else raw UTF-8), and numbers in the ECMAScript
//! `Number.prototype.toString` form of the IEEE-754 double (RFC 8785 § 3.2.2.3).

use std::fmt::Write as _;

use serde_json::Value;

/// The canonical text of `value`, or `None` when it holds a number that is not a finite double.
pub fn canonical(value: &Value) -> Option<String> {
    let mut out = String::new();
    write_value(value, &mut out)?;
    Some(out)
}

fn write_value(value: &Value, out: &mut String) -> Option<()> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number(n.as_f64()?)?),
        Value::String(s) => string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                string(key, out);
                out.push(':');
                write_value(&map[key], out)?;
            }
            out.push('}');
        }
    }
    Some(())
}

fn string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// ECMAScript `Number.prototype.toString(x)` for a finite double (ECMA-262 § 6.1.6.1.20), built
/// from Rust's shortest round-trip digits.
pub fn number(x: f64) -> Option<String> {
    if !x.is_finite() {
        return None;
    }
    if x == 0.0 {
        return Some("0".to_string());
    }
    let sign = if x < 0.0 { "-" } else { "" };
    // `{:e}` prints the shortest digits that round-trip: `d[.ddd]e<exp>`.
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e')?;
    let exp: i32 = exp.parse().ok()?;
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let k = digits.len() as i32;
    // The value is 0.<digits> x 10^n.
    let n = exp + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        let (int, frac) = digits.split_at(n as usize);
        format!("{int}.{frac}")
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign_e = if e >= 0 { '+' } else { '-' };
        let (first, rest) = digits.split_at(1);
        if rest.is_empty() {
            format!("{first}e{sign_e}{}", e.abs())
        } else {
            format!("{first}.{rest}e{sign_e}{}", e.abs())
        }
    };
    Some(format!("{sign}{body}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The number examples of RFC 8785 Appendix B and the ECMAScript boundaries.
    #[test]
    fn numbers_follow_ecmascript() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (100.0, "100"),
            (0.1, "0.1"),
            (-1.5, "-1.5"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (123456789012345680000.0, "123456789012345680000"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (9007199254740992.0, "9007199254740992"),
            (295147905179352830000.0, "295147905179352830000"),
            (0.000001234, "0.000001234"),
            (12.5e-7, "0.00000125"),
            (333333333.3333333, "333333333.3333333"),
        ];
        for (x, want) in cases {
            assert_eq!(number(*x).as_deref(), Some(*want), "{x:e}");
        }
        assert_eq!(number(f64::NAN), None);
    }

    #[test]
    fn objects_sort_by_utf16_and_strings_escape_like_json_stringify() {
        let v = json!({"b": [1, 2.0, "x"], "a": {"\u{20ac}": 1, "\r": 2, "1": 3}, "c": "\u{1}\"\\é"});
        assert_eq!(
            canonical(&v).as_deref(),
            Some("{\"a\":{\"\\r\":2,\"1\":3,\"\u{20ac}\":1},\"b\":[1,2,\"x\"],\"c\":\"\\u0001\\\"\\\\é\"}")
        );
        // UTF-16 order differs from UTF-8 order for astral vs. high BMP characters.
        let v = json!({"\u{ff61}": 1, "\u{1f600}": 2});
        assert_eq!(canonical(&v).as_deref(), Some("{\"\u{1f600}\":2,\"\u{ff61}\":1}"));
    }
}
