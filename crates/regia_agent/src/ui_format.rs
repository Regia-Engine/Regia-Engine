// SPDX-License-Identifier: MPL-2.0
//! `ui.format_preview` (`regia_ui::loc_format`, ADR-2698, book `agents/ui-localization.md`):
//! formats a value in a locale with the same option names as Fluent's `NUMBER()` and `DATETIME()`,
//! so an agent can check what a translator's placeable will show before writing it.

use regia_app::prelude::*;
use regia_ui::Localization;
use regia_ui::loc_format::{
    self, CivilDateTime, DateOptions, FormatError, ListType, NumberOptions, OptionValue, RelativeNumeric,
    RelativeUnit,
};
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Most list items `ui.format_preview { kind: "list" }` accepts.
const MAX_LIST_ITEMS: usize = 256;

/// Longest single list item, in bytes.
const MAX_ITEM_BYTES: usize = 4096;

fn format_error(e: &FormatError) -> RpcError {
    let fix = match e {
        FormatError::BadOption { .. } => {
            "use the Fluent NUMBER()/DATETIME() option names and values in book/agents/ui-localization.md"
        }
        FormatError::BadValue(_) => {
            "pass a finite number, unix seconds, or ISO 8601 text such as 2026-11-05T14:30:00+01:00"
        }
        FormatError::BadPattern { .. } => "report this: a date pattern in the locale table is malformed",
    };
    RpcError::new(INVALID_PARAMS, e.code(), e.to_string(), fix)
}

fn bad_option(name: &str, reason: &str) -> RpcError {
    format_error(&FormatError::BadOption {
        name: name.to_owned(),
        reason: reason.to_owned(),
    })
}

fn bad_value(reason: impl Into<String>) -> RpcError {
    format_error(&FormatError::BadValue(reason.into()))
}

/// Reads `options` (an object) into option pairs; booleans become `"true"`/`"false"`.
fn option_pairs(options: Option<&Value>) -> Result<Vec<(String, Value)>, RpcError> {
    match options {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(map)) => Ok(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        Some(_) => Err(RpcError::invalid_params("`options` must be an object")),
    }
}

fn borrowed<'a>(
    pairs: &'a [(String, Value)],
    texts: &'a [Option<String>],
) -> Result<Vec<(&'a str, OptionValue<'a>)>, RpcError> {
    pairs
        .iter()
        .zip(texts)
        .map(|((name, value), text)| {
            let v = match (value, text) {
                (_, Some(text)) => OptionValue::Str(text.as_str()),
                (Value::Number(n), None) => OptionValue::Num(n.as_f64().unwrap_or(f64::NAN)),
                _ => return Err(bad_option(name, "must be a string, a number or a boolean")),
            };
            Ok((name.as_str(), v))
        })
        .collect()
}

fn option_texts(pairs: &[(String, Value)]) -> Vec<Option<String>> {
    pairs
        .iter()
        .map(|(_, v)| match v {
            Value::String(s) => Some(s.clone()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        })
        .collect()
}

/// Shows invisible and look-alike characters (`\u{a0}`, `\u{202f}`, bidi marks) as escapes, so an
/// agent can tell a no-break space from a space.
fn escaped(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' || (c.is_alphanumeric() && !c.is_whitespace()) {
                c.to_string()
            } else if c.is_whitespace()
                || c.is_control()
                || matches!(c, '\u{200e}' | '\u{200f}' | '\u{61c}' | '\u{2212}')
            {
                format!("\\u{{{:x}}}", u32::from(c))
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn number_value(params: &Value) -> Result<f64, RpcError> {
    match params.get("value") {
        Some(Value::Number(n)) => n
            .as_f64()
            .ok_or_else(|| bad_value("`value` is not a finite number")),
        // Text reaches NaN and infinity, which JSON cannot carry.
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| bad_value(format!("{s:?} is not a number"))),
        _ => Err(RpcError::invalid_params(
            "`value` (a number, or text such as \"NaN\") is required",
        )),
    }
}

fn date_value(params: &Value, default_offset: i32, zone: Option<i32>) -> Result<CivilDateTime, RpcError> {
    match params.get("value") {
        Some(Value::Number(n)) => {
            let seconds = n
                .as_i64()
                .or_else(|| {
                    n.as_f64()
                        .filter(|f| f.is_finite() && f.abs() < 1e15)
                        .map(|f| f.floor() as i64)
                })
                .ok_or_else(|| bad_value("`value` is not a unix time"))?;
            CivilDateTime::from_unix(seconds, zone.unwrap_or(default_offset)).map_err(|e| format_error(&e))
        }
        Some(Value::String(s)) => CivilDateTime::parse_iso(s).map_err(|e| format_error(&e)),
        _ => Err(RpcError::invalid_params(
            "`value` (unix seconds, or ISO 8601 text such as \"2026-11-05T14:30:00+01:00\") is required",
        )),
    }
}

/// `ui.format_preview { kind, locale?, value?, unit?, items?, options? }`.
pub(crate) fn format_preview(world: &World, params: &Value) -> Result<Value, RpcError> {
    let kind = match params.get("kind") {
        None | Some(Value::Null) => "number",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err(RpcError::invalid_params("`kind` must be a string")),
    };
    let loc = world.get_resource::<Localization>();
    let locale = match params.get("locale") {
        None | Some(Value::Null) => loc.map_or_else(|| "en".to_owned(), |l| l.locale().to_owned()),
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 64 => s.clone(),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`locale` must be a non-empty tag of at most 64 bytes",
            ));
        }
    };
    let default_offset = loc.map_or(0, Localization::utc_offset_seconds);
    let pairs = option_pairs(params.get("options"))?;
    let texts = option_texts(&pairs);
    let options = borrowed(&pairs, &texts)?;
    let mut ignored: Vec<String> = Vec::new();
    let text = match kind {
        "locales" => {
            return Ok(json!({
                "kind": kind,
                "locales": loc_format::data_locales(),
                "chain": loc_format::locale_chain(&locale),
            }));
        }
        "number" => {
            let (o, i) = NumberOptions::from_options(options).map_err(|e| format_error(&e))?;
            ignored = i;
            loc_format::format_number(number_value(params)?, &locale, &o).map_err(|e| format_error(&e))?
        }
        "date" | "time" | "datetime" => {
            let (mut o, i) = DateOptions::from_options(options).map_err(|e| format_error(&e))?;
            ignored = i;
            if o.date_style.is_none() && o.time_style.is_none() {
                match kind {
                    "time" => o.time_style = Some(loc_format::DateStyle::Short),
                    "datetime" => {
                        o.date_style = Some(loc_format::DateStyle::Medium);
                        o.time_style = Some(loc_format::DateStyle::Short);
                    }
                    _ => o.date_style = Some(loc_format::DateStyle::Short),
                }
            }
            let dt = date_value(params, default_offset, o.time_zone)?;
            loc_format::format_datetime(&dt, &locale, &o).map_err(|e| format_error(&e))?
        }
        "relative" => {
            let unit_name = crate::params::opt_str(params, "unit")?.unwrap_or("day");
            let unit = RelativeUnit::parse(unit_name).ok_or_else(|| {
                bad_option("unit", "must be second, minute, hour, day, week, month or year")
            })?;
            let mut numeric = RelativeNumeric::Always;
            let mut rest = Vec::new();
            for (name, value) in options {
                match (name, value) {
                    ("numeric", OptionValue::Str("always")) => numeric = RelativeNumeric::Always,
                    ("numeric", OptionValue::Str("auto")) => numeric = RelativeNumeric::Auto,
                    ("numeric", _) => return Err(bad_option("numeric", "must be \"always\" or \"auto\"")),
                    _ => rest.push((name, value)),
                }
            }
            let (o, i) = NumberOptions::from_options(rest).map_err(|e| format_error(&e))?;
            ignored = i;
            loc_format::format_relative(number_value(params)?, unit, &locale, numeric, &o)
                .map_err(|e| format_error(&e))?
        }
        "list" => {
            let Some(Value::Array(items)) = params.get("items") else {
                return Err(RpcError::invalid_params(
                    "`items` (an array of strings) is required for kind \"list\"",
                ));
            };
            if items.len() > MAX_LIST_ITEMS {
                return Err(RpcError::invalid_params(format!(
                    "`items` has more than {MAX_LIST_ITEMS} entries"
                )));
            }
            let items: Vec<&str> = items
                .iter()
                .map(|v| {
                    v.as_str().filter(|s| s.len() <= MAX_ITEM_BYTES).ok_or_else(|| {
                        RpcError::invalid_params(format!(
                            "`items` must be strings of at most {MAX_ITEM_BYTES} bytes"
                        ))
                    })
                })
                .collect::<Result<_, _>>()?;
            let mut list_type = ListType::And;
            for (name, value) in options {
                match (name, value) {
                    ("type", OptionValue::Str("conjunction" | "and")) => list_type = ListType::And,
                    ("type", OptionValue::Str("disjunction" | "or")) => list_type = ListType::Or,
                    ("type", _) => {
                        return Err(bad_option("type", "must be \"conjunction\" or \"disjunction\""));
                    }
                    (other, _) => ignored.push(other.to_owned()),
                }
            }
            loc_format::format_list(&items, &locale, list_type)
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "`kind` must be number, date, time, datetime, relative, list or locales, got {other:?}"
            )));
        }
    };
    let mut out = Map::new();
    out.insert("kind".into(), json!(kind));
    out.insert("locale".into(), json!(locale));
    out.insert("chain".into(), json!(loc_format::locale_chain(&locale)));
    out.insert("escaped".into(), json!(escaped(&text)));
    out.insert("text".into(), json!(text));
    out.insert("ignored_options".into(), json!(ignored));
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        World::new()
    }

    fn call(params: Value) -> Result<Value, RpcError> {
        format_preview(&world(), &params)
    }

    #[test]
    fn previews_each_kind() {
        let cases = [
            (
                json!({ "kind": "number", "locale": "de-DE", "value": 1234.5 }),
                "1.234,5",
            ),
            (
                json!({ "locale": "fr-FR", "value": 1234.5, "options": { "style": "currency", "currency": "EUR" } }),
                "1\u{202f}234,50\u{a0}€",
            ),
            (json!({ "locale": "hi-IN", "value": 1234567 }), "12,34,567"),
            (
                json!({ "locale": "en", "value": 1, "options": { "minimumFractionDigits": 2, "useGrouping": false } }),
                "1.00",
            ),
            (json!({ "locale": "en", "value": "NaN" }), "NaN"),
            (
                json!({ "kind": "date", "locale": "ja-JP", "value": "2026-11-05", "options": { "dateStyle": "long" } }),
                "2026年11月5日",
            ),
            (
                json!({ "kind": "time", "locale": "en-US", "value": 0, "options": { "hour12": false } }),
                "00:00",
            ),
            (
                json!({ "kind": "datetime", "locale": "en-US", "value": "2026-11-05T15:45:00" }),
                "Nov 5, 2026, 3:45\u{202f}PM",
            ),
            (
                json!({ "kind": "relative", "locale": "en", "value": -2, "unit": "hours" }),
                "2 hours ago",
            ),
            (
                json!({ "kind": "relative", "locale": "en", "value": 1, "unit": "day", "options": { "numeric": "auto" } }),
                "tomorrow",
            ),
            (
                json!({ "kind": "list", "locale": "en-US", "items": ["a", "b", "c"] }),
                "a, b, and c",
            ),
            (
                json!({ "kind": "list", "locale": "de", "items": ["a", "b", "c"], "options": { "type": "disjunction" } }),
                "a, b oder c",
            ),
        ];
        for (params, want) in cases {
            let out = call(params.clone()).unwrap_or_else(|e| panic!("{params}: {}", e.message));
            assert_eq!(out["text"], want, "{params}");
        }
        let out = call(json!({ "locale": "fr-FR", "value": 1234.5 })).expect("ok");
        assert_eq!(out["escaped"], "1\\u{202f}234,5");
        assert_eq!(out["chain"], json!(["fr-FR", "fr", "root"]));
        let locales = call(json!({ "kind": "locales", "locale": "pt-BR" })).expect("ok");
        assert_eq!(locales["chain"], json!(["pt-BR", "pt", "root"]));
        assert!(
            locales["locales"]
                .as_array()
                .is_some_and(|a| a.iter().any(|t| t == "root"))
        );
        let ignored =
            call(json!({ "locale": "en", "value": 1, "options": { "notation": "compact" } })).expect("ok");
        assert_eq!(ignored["ignored_options"], json!(["notation"]));
    }

    #[test]
    fn errors_have_codes() {
        let cases = [
            (
                json!({ "value": 1, "options": { "maximumFractionDigits": 500 } }),
                "format_bad_option",
            ),
            (
                json!({ "value": 1, "options": { "style": "currency" } }),
                "format_bad_option",
            ),
            (
                json!({ "value": 1, "options": { "useGrouping": [1] } }),
                "format_bad_option",
            ),
            (
                json!({ "kind": "date", "value": "2026-02-30" }),
                "format_bad_value",
            ),
            (
                json!({ "kind": "date", "value": "2026-11-05", "options": { "timeZone": "Europe/Paris" } }),
                "format_bad_option",
            ),
            (json!({ "kind": "relative", "value": "inf" }), "format_bad_value"),
            (
                json!({ "kind": "relative", "value": 1, "unit": "fortnight" }),
                "format_bad_option",
            ),
            (json!({ "value": "lots" }), "format_bad_value"),
        ];
        for (params, code) in cases {
            let err = call(params.clone()).expect_err("fails");
            let got = err
                .data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .unwrap_or("")
                .to_owned();
            assert_eq!(got, code, "{params}");
        }
        assert_eq!(
            call(json!({ "kind": "nope" })).expect_err("bad").code,
            INVALID_PARAMS
        );
        assert!(call(json!({ "value": 1, "options": 5 })).is_err());
        assert!(call(json!({ "kind": "list", "items": [1] })).is_err());
    }
}
