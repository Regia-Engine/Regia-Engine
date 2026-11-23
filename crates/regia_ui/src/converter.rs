// SPDX-License-Identifier: MPL-2.0
//! Binding converters (ADR-2489, `docs/specs/81-ui-designer.md` § 2.6): a small fixed set of pure
//! value transforms between a view-model field and a widget property. There is no scripting: a
//! converter is data in the `.ui.ron` (`bindings: [(source: "HudVm.hp", converter: map_range(from:
//! (0, 100), to: (0, 1)))]`) and runs before `format` and `invert`.
//!
//! Converters read the same text a binding already formats (numbers and booleans in their `Debug`
//! form, enums by variant name), so they cannot fail: text a converter cannot read passes through
//! unchanged. Non-finite or reversed bounds in a file never panic.

use serde::{Deserialize, Serialize};

use crate::interaction::UiValue;

/// Most digits `number` writes after the point.
pub const MAX_NUMBER_PRECISION: u8 = 64;

/// One value transform of a binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindConverter {
    /// A number with a fixed count of decimals and an optional digit-group separator
    /// (`number(precision: 2, group: ",")`: `1234.5` shows as `1,234.50`).
    Number {
        /// Digits after the point (at most [`MAX_NUMBER_PRECISION`]).
        precision: u8,
        /// Text between each group of three whole digits; no grouping when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
    },
    /// A number limited to `min..=max` (`clamp(min: 0, max: 100)`). Reversed bounds are swapped;
    /// a non-finite bound leaves the value as it is.
    Clamp {
        /// Lower bound.
        min: f64,
        /// Upper bound.
        max: f64,
    },
    /// A number mapped linearly from one range to another
    /// (`map_range(from: (0, 100), to: (0, 1), clamp: true)`).
    MapRange {
        /// Input range.
        from: (f64, f64),
        /// Output range.
        to: (f64, f64),
        /// Limit the result to the output range.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        clamp: bool,
    },
    /// Any value as `true` or `false` for a `visible`, `enabled` or `checked` target: `false`,
    /// `None`, an empty text and zero are `false`, everything else is `true`.
    BoolToVisibility,
    /// A text per enum variant (`enum_text(table: [("Idle", "Waiting")], default: "?")`). The
    /// lookup uses the whole value, then its variant name; with no entry the result is `default`,
    /// or the value when there is no `default`.
    EnumText {
        /// `(value or variant name, text)` pairs; the first match wins.
        table: Vec<(String, String)>,
        /// The text when no entry matches.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    /// The value as a localization key (`loc_key(prefix: "state-")`: variant `Idle` shows the
    /// Fluent message `state-Idle`). On a `text` target the span holds the key, so it re-resolves
    /// on a language switch; a missing key shows as `⟦key⟧`.
    LocKey {
        /// Text put before the value to make the key.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        prefix: String,
    },
}

/// Each variant's field names, for the format fingerprint (variant names alone would miss a renamed
/// field; a test checks this against the serialized form).
pub(crate) fn converter_field_names() -> Vec<String> {
    [
        "number.precision",
        "number.group",
        "clamp.min",
        "clamp.max",
        "map_range.from",
        "map_range.to",
        "map_range.clamp",
        "enum_text.table",
        "enum_text.default",
        "loc_key.prefix",
    ]
    .map(str::to_owned)
    .to_vec()
}

impl BindConverter {
    /// Every converter name, in picker order.
    pub const NAMES: [&'static str; 6] = [
        "number",
        "clamp",
        "map_range",
        "bool_to_visibility",
        "enum_text",
        "loc_key",
    ];

    /// The `.ui.ron` name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Number { .. } => "number",
            Self::Clamp { .. } => "clamp",
            Self::MapRange { .. } => "map_range",
            Self::BoolToVisibility => "bool_to_visibility",
            Self::EnumText { .. } => "enum_text",
            Self::LocKey { .. } => "loc_key",
        }
    }

    /// A new converter of the kind `name` with neutral settings (the designer's picker).
    pub fn template(name: &str) -> Option<Self> {
        Some(match name {
            "number" => Self::Number {
                precision: 1,
                group: None,
            },
            "clamp" => Self::Clamp { min: 0.0, max: 1.0 },
            "map_range" => Self::MapRange {
                from: (0.0, 1.0),
                to: (0.0, 1.0),
                clamp: false,
            },
            "bool_to_visibility" => Self::BoolToVisibility,
            "enum_text" => Self::EnumText {
                table: Vec::new(),
                default: None,
            },
            "loc_key" => Self::LocKey {
                prefix: String::new(),
            },
            _ => return None,
        })
    }

    /// Model → widget: `raw` is the field's text.
    pub fn apply(&self, raw: &str) -> String {
        let number = || raw.trim().parse::<f64>().ok();
        match self {
            Self::Number { precision, group } => format_number(raw, *precision, group.as_deref()),
            Self::Clamp { min, max } => match number() {
                Some(v) if !v.is_nan() && min.is_finite() && max.is_finite() => {
                    let limited = v.clamp(min.min(*max), min.max(*max));
                    // An untouched value keeps its own text (an integer stays an integer).
                    if limited == v {
                        raw.trim().to_owned()
                    } else {
                        limited.to_string()
                    }
                }
                _ => raw.to_owned(),
            },
            Self::MapRange { from, to, clamp } => match number() {
                Some(v) => map_range(v, *from, *to, *clamp).to_string(),
                None => raw.to_owned(),
            },
            Self::BoolToVisibility => truthy(raw).to_string(),
            Self::EnumText { table, default } => {
                let whole = raw.trim();
                let head = variant_head(whole);
                table
                    .iter()
                    .find(|(k, _)| k == whole)
                    .or_else(|| table.iter().find(|(k, _)| k == head))
                    .map(|(_, text)| text.clone())
                    .or_else(|| default.clone())
                    .unwrap_or_else(|| raw.to_owned())
            }
            Self::LocKey { prefix } => format!("{prefix}{}", variant_head(raw.trim())),
        }
    }

    /// Widget → model for a two-way binding: the value to store. An `Err` means this converter
    /// has no way back and nothing is written (`binding_converter_one_way`).
    pub fn invert(&self, value: &UiValue) -> Result<UiValue, String> {
        match (self, value) {
            (Self::Number { .. }, v) => Ok(v.clone()),
            (Self::Clamp { min, max }, UiValue::Number(n)) => {
                if min.is_finite() && max.is_finite() && !n.is_nan() {
                    let limited = f64::from(*n).clamp(min.min(*max), min.max(*max));
                    Ok(UiValue::Number(limited as f32))
                } else {
                    Ok(UiValue::Number(*n))
                }
            }
            (Self::Clamp { .. }, v) => Ok(v.clone()),
            (Self::MapRange { from, to, clamp }, UiValue::Number(n)) => {
                // The output end of a zero-width range has lost the input, so there is no way back.
                if to.0 == to.1 || !to.0.is_finite() || !to.1.is_finite() {
                    return Err(format!(
                        "map_range output {to:?} is not invertible: the input is lost"
                    ));
                }
                Ok(UiValue::Number(
                    map_range(f64::from(*n), *to, *from, *clamp) as f32
                ))
            }
            (Self::MapRange { .. }, v) => Ok(v.clone()),
            (Self::BoolToVisibility | Self::EnumText { .. } | Self::LocKey { .. }, _) => {
                Err(format!("converter `{}` has no way back", self.name()))
            }
        }
    }

    /// Whether a two-way binding can write through this converter.
    pub fn is_invertible(&self) -> bool {
        matches!(
            self,
            Self::Number { .. } | Self::Clamp { .. } | Self::MapRange { .. }
        )
    }

    /// What is wrong with the converter's own settings, for the designer's lint.
    pub fn problem(&self) -> Option<String> {
        match self {
            Self::Number { precision, .. } if *precision > MAX_NUMBER_PRECISION => Some(format!(
                "precision {precision} is above {MAX_NUMBER_PRECISION}; it counts as {MAX_NUMBER_PRECISION}"
            )),
            Self::Clamp { min, max } if !min.is_finite() || !max.is_finite() => {
                Some("clamp bounds must be finite; the value passes through".to_owned())
            }
            Self::Clamp { min, max } if min > max => {
                Some(format!("clamp min {min} is above max {max}; they are swapped"))
            }
            Self::MapRange { from, to, .. }
                if !(from.0.is_finite() && from.1.is_finite() && to.0.is_finite() && to.1.is_finite()) =>
            {
                Some("map_range ends must be finite".to_owned())
            }
            Self::MapRange { from, .. } if from.0 == from.1 => Some(format!(
                "map_range input {from:?} has no width; every value maps to the output start"
            )),
            Self::EnumText { table, .. } if table.is_empty() => {
                Some("enum_text has an empty table".to_owned())
            }
            _ => None,
        }
    }
}

/// `v` mapped from `from` to `to`. A zero-width or non-finite input range gives the output start.
fn map_range(v: f64, from: (f64, f64), to: (f64, f64), clamp: bool) -> f64 {
    let width = from.1 - from.0;
    if width == 0.0 || !width.is_finite() || !v.is_finite() {
        return to.0;
    }
    let mut t = (v - from.0) / width;
    if clamp {
        t = t.clamp(0.0, 1.0);
    }
    // Exact at both ends of the output range.
    if t == 1.0 { to.1 } else { to.0 + t * (to.1 - to.0) }
}

/// `false`, `None`, empty text, zero and NaN are false; every other value is true.
fn truthy(raw: &str) -> bool {
    let text = raw.trim();
    match text {
        "" | "false" | "None" => false,
        "true" => true,
        _ => text.parse::<f64>().map_or(true, |n| n != 0.0 && !n.is_nan()),
    }
}

/// The variant name at the start of a reflected enum's text (`Moving { speed: 1.0 }` -> `Moving`).
fn variant_head(text: &str) -> &str {
    let end = text
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(text.len());
    if end == 0 { text } else { &text[..end] }
}

/// `raw` with `precision` decimals and groups of three whole digits. Text that is not a number
/// passes through; an integer above 2^53 keeps every digit.
fn format_number(raw: &str, precision: u8, group: Option<&str>) -> String {
    let precision = usize::from(precision.min(MAX_NUMBER_PRECISION));
    let trimmed = raw.trim();
    let text = if let Ok(int) = trimmed.parse::<i128>() {
        if precision == 0 {
            int.to_string()
        } else {
            format!("{int}.{}", "0".repeat(precision))
        }
    } else {
        match trimmed.parse::<f64>() {
            Ok(n) if n.is_finite() => format!("{n:.precision$}"),
            _ => return raw.to_owned(),
        }
    };
    match group {
        Some(sep) if !sep.is_empty() => group_digits(&text, sep),
        _ => text,
    }
}

/// `-1234.50` -> `-1,234.50`.
fn group_digits(text: &str, sep: &str) -> String {
    let (sign, rest) = text.strip_prefix('-').map_or(("", text), |digits| ("-", digits));
    let (whole, decimals) = rest.split_once('.').map_or((rest, None), |(w, d)| (w, Some(d)));
    let mut out = String::with_capacity(text.len() + whole.len() / 3 * sep.len());
    out.push_str(sign);
    for (i, digit) in whole.char_indices() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            out.push_str(sep);
        }
        out.push(digit);
    }
    if let Some(decimals) = decimals {
        out.push('.');
        out.push_str(decimals);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formats_precision_and_groups() {
        let n = |precision, group: Option<&str>| BindConverter::Number {
            precision,
            group: group.map(str::to_owned),
        };
        assert_eq!(n(2, None).apply("3.14159"), "3.14");
        assert_eq!(n(0, None).apply("2.5"), "2");
        assert_eq!(n(2, None).apply("7"), "7.00");
        assert_eq!(n(2, Some(",")).apply("1234.5"), "1,234.50");
        assert_eq!(n(0, Some(" ")).apply("-1234567"), "-1 234 567");
        assert_eq!(n(0, Some(",")).apply("999"), "999");
        assert_eq!(n(0, Some(",")).apply("1000"), "1,000");
        // Every digit of a u64 survives (no float on the way).
        assert_eq!(n(0, None).apply("18446744073709551615"), "18446744073709551615");
        assert_eq!(n(2, None).apply("sword"), "sword");
        assert_eq!(n(2, Some(",")).apply("NaN"), "NaN");
        assert_eq!(
            n(255, None).apply("1").len(),
            2 + usize::from(MAX_NUMBER_PRECISION)
        );
    }

    #[test]
    fn clamp_limits_and_survives_bad_bounds() {
        let c = |min, max| BindConverter::Clamp { min, max };
        assert_eq!(c(0.0, 1.0).apply("2.5"), "1");
        assert_eq!(c(0.0, 1.0).apply("-3"), "0");
        assert_eq!(c(0.0, 100.0).apply("42"), "42");
        assert_eq!(c(0.0, 1.0).apply(" 0.5 "), "0.5");
        assert_eq!(c(1.0, 0.0).apply("2"), "1", "reversed bounds are swapped");
        assert_eq!(c(f64::NAN, 1.0).apply("2"), "2", "f64::clamp would panic on NaN");
        assert_eq!(c(0.0, f64::INFINITY).apply("2"), "2");
        assert_eq!(c(0.0, 1.0).apply("NaN"), "NaN");
        assert_eq!(c(0.0, 1.0).apply("hello"), "hello");
    }

    #[test]
    fn map_range_maps_clamps_and_survives_zero_width() {
        let m = |from, to, clamp| BindConverter::MapRange { from, to, clamp };
        assert_eq!(m((0.0, 100.0), (0.0, 1.0), false).apply("50"), "0.5");
        assert_eq!(m((0.0, 100.0), (0.0, 1.0), false).apply("150"), "1.5");
        assert_eq!(m((0.0, 100.0), (0.0, 1.0), true).apply("150"), "1");
        assert_eq!(m((0.0, 100.0), (1.0, 0.0), false).apply("25"), "0.75");
        assert_eq!(m((5.0, 5.0), (2.0, 9.0), false).apply("5"), "2");
        assert_eq!(m((0.0, f64::NAN), (2.0, 9.0), true).apply("5"), "2");
        assert_eq!(m((0.0, 1.0), (0.0, 1.0), false).apply("x"), "x");
    }

    #[test]
    fn bool_to_visibility_reads_any_value() {
        let v = BindConverter::BoolToVisibility;
        for yes in ["true", "1", "-0.5", "text", "Some(3)"] {
            assert_eq!(v.apply(yes), "true", "{yes}");
        }
        for no in ["false", "0", "0.0", "", "  ", "None", "NaN"] {
            assert_eq!(v.apply(no), "false", "{no:?}");
        }
    }

    #[test]
    fn enum_text_looks_up_whole_value_then_variant() {
        let table = vec![
            ("Idle".to_owned(), "Waiting".to_owned()),
            ("Moving".to_owned(), "On the way".to_owned()),
            ("Moving(9)".to_owned(), "Fast".to_owned()),
        ];
        let plain = BindConverter::EnumText {
            table: table.clone(),
            default: None,
        };
        assert_eq!(plain.apply("Idle"), "Waiting");
        assert_eq!(plain.apply("Moving { speed: 1.0 }"), "On the way");
        assert_eq!(plain.apply("Moving(9)"), "Fast");
        assert_eq!(plain.apply("Gone"), "Gone");
        let with_default = BindConverter::EnumText {
            table,
            default: Some("?".to_owned()),
        };
        assert_eq!(with_default.apply("Gone"), "?");
    }

    #[test]
    fn loc_key_adds_the_prefix() {
        let l = BindConverter::LocKey {
            prefix: "state-".to_owned(),
        };
        assert_eq!(l.apply("Idle"), "state-Idle");
        assert_eq!(l.apply("Moving { speed: 1.0 }"), "state-Moving");
    }

    #[test]
    fn invert_writes_back_through_invertible_converters_only() {
        let m = BindConverter::MapRange {
            from: (0.0, 100.0),
            to: (0.0, 1.0),
            clamp: false,
        };
        assert_eq!(m.invert(&UiValue::Number(0.25)), Ok(UiValue::Number(25.0)));
        assert!(m.is_invertible());
        let flat = BindConverter::MapRange {
            from: (0.0, 100.0),
            to: (1.0, 1.0),
            clamp: false,
        };
        assert!(flat.invert(&UiValue::Number(1.0)).is_err());
        let c = BindConverter::Clamp { min: 0.0, max: 1.0 };
        assert_eq!(c.invert(&UiValue::Number(3.0)), Ok(UiValue::Number(1.0)));
        // Spec 81 § 2.6: `number` writes the value back as it is (the grouping is display only).
        let n = BindConverter::Number {
            precision: 2,
            group: Some(",".to_owned()),
        };
        assert!(n.is_invertible());
        assert_eq!(n.invert(&UiValue::Number(1234.5)), Ok(UiValue::Number(1234.5)));
        assert_eq!(
            n.invert(&UiValue::Text("1,234.50".to_owned())),
            Ok(UiValue::Text("1,234.50".to_owned()))
        );
        for one_way in [
            BindConverter::BoolToVisibility,
            BindConverter::EnumText {
                table: Vec::new(),
                default: None,
            },
            BindConverter::LocKey {
                prefix: String::new(),
            },
        ] {
            assert!(!one_way.is_invertible());
            assert!(one_way.invert(&UiValue::Bool(true)).is_err());
        }
    }

    /// `converter_field_names` lists what serde writes, so a renamed field moves the fingerprint.
    #[test]
    fn fingerprint_names_match_the_serialized_fields() {
        let full = [
            BindConverter::Number {
                precision: 1,
                group: Some(",".to_owned()),
            },
            BindConverter::Clamp { min: 0.0, max: 1.0 },
            BindConverter::MapRange {
                from: (0.0, 1.0),
                to: (0.0, 1.0),
                clamp: true,
            },
            BindConverter::BoolToVisibility,
            BindConverter::EnumText {
                table: vec![("a".to_owned(), "b".to_owned())],
                default: Some("c".to_owned()),
            },
            BindConverter::LocKey {
                prefix: "p-".to_owned(),
            },
        ];
        let mut names = Vec::new();
        for converter in &full {
            let json = serde_json::to_value(converter).expect("json");
            if let Some((variant, fields)) = json.as_object().and_then(|o| o.iter().next()) {
                for field in fields.as_object().expect("struct variant").keys() {
                    names.push(format!("{variant}.{field}"));
                }
            }
        }
        names.sort();
        let mut listed = converter_field_names();
        listed.sort();
        assert_eq!(names, listed);
    }

    #[test]
    fn templates_cover_every_name_and_problems_name_bad_settings() {
        for name in BindConverter::NAMES {
            let c = BindConverter::template(name).expect(name);
            assert_eq!(c.name(), name);
        }
        assert_eq!(BindConverter::template("rocket"), None);
        assert!(
            BindConverter::Clamp { min: 2.0, max: 1.0 }.problem().is_some(),
            "reversed clamp"
        );
        assert!(
            BindConverter::MapRange {
                from: (1.0, 1.0),
                to: (0.0, 1.0),
                clamp: false
            }
            .problem()
            .is_some()
        );
        assert!(BindConverter::BoolToVisibility.problem().is_none());
    }
}
