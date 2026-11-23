// SPDX-License-Identifier: MPL-2.0
//! Culture-aware formatting of values in localized text (UI parity L04, ADR-2698, spec 36 § 13):
//! numbers, percentages, currencies, dates, times, relative times and lists, plus the Fluent
//! functions `NUMBER()` and `DATETIME()` that every bundle of [`crate::Localization`] registers.
//!
//! - [`format_number`]: decimal and grouping separators, grouping sizes (Western 3, Indian 3;2),
//!   CLDR minimum grouping digits, minimum/maximum fraction digits, significant digits, a
//!   [`RoundingMode`], sign display, percent, and currency with symbol placement, ISO code or
//!   narrow symbol. NaN and infinity format as the locale's symbols; a value of any size formats
//!   with all its digits (no exponent).
//! - [`format_date`], [`format_time`], [`format_datetime`]: short, medium, long and full patterns,
//!   12- or 24-hour clock, month and weekday names. Input is a [`CivilDateTime`] (a wall-clock date
//!   and time, with its UTC offset when known), built directly or from unix seconds plus a UTC offset
//!   the game passes in ([`CivilDateTime::from_unix`]). **There is no time-zone database**: a zone is
//!   a fixed offset (`+02:00`, `UTC`), never an IANA name, and daylight saving time is the game's
//!   business. Only the Gregorian calendar is supported.
//! - [`format_relative`]: "in 3 days", "2 hours ago", with each language's plural rules
//!   ([`crate::plural`]); `numeric: auto` says "tomorrow".
//! - [`format_list`]: "a, b, and c" or "a, b, or c".
//!
//! Data comes from a compact table written by hand from the public Unicode CLDR charts (module
//! `data`, Unicode License v3). A lookup walks the locale's chain ([`locale_chain`]: `pt-BR` → `pt`
//! → `root`) and takes each field from the first row that has it.
//!
//! Native digits (Arabic-Indic, Devanagari, Thai) are opt-in with `numberingSystem: "native"` (or
//! the system's id, `arab`): Latin digits are the default everywhere, because a game's fonts are
//! more likely to carry them. Pseudo-localization does not touch formatted values: the pseudo
//! locale `en-XA` formats as English, and its accents apply to the message text around the
//! placeables only, so a formatted number or date in a pseudo screenshot is the real one.

mod data;

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::types::FluentType;
use fluent_bundle::{FluentArgs, FluentError, FluentResource, FluentValue};

use data::{
    CalendarData, Currency, DatePatterns, ListData, LocaleData, NativeData, NumberData, RelativeData,
};

/// The Fluent function name of number formatting.
pub const NUMBER_FUNCTION: &str = "NUMBER";

/// The Fluent function name of date and time formatting.
pub const DATETIME_FUNCTION: &str = "DATETIME";

/// Largest number of fraction digits an option may ask for (the ECMA-402 limit).
pub const MAX_FRACTION_DIGITS: u8 = 100;

/// Largest number of significant or integer digits an option may ask for.
pub const MAX_SIGNIFICANT_DIGITS: u8 = 21;

/// Largest UTC offset accepted, in seconds (±18 hours).
pub const MAX_UTC_OFFSET_SECONDS: i32 = 18 * 3600;

/// A formatting failure. Fluent functions turn it into Fluent's error value (the placeable shows
/// `NUMBER()` or `DATETIME()`); the agent reports [`FormatError::code`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    /// An option has a value the formatter does not accept.
    #[error("option `{name}`: {reason}")]
    BadOption {
        /// The option name (`minimumFractionDigits`).
        name: String,
        /// Why it was rejected.
        reason: String,
    },
    /// The value to format is not usable (a date that does not exist, a non-numeric text).
    #[error("{0}")]
    BadValue(String),
    /// A date pattern in the data uses a letter the formatter does not know.
    #[error("pattern {pattern:?}: {reason}")]
    BadPattern {
        /// The pattern.
        pattern: String,
        /// What is wrong.
        reason: String,
    },
}

impl FormatError {
    /// The agent error code: `format_bad_option`, `format_bad_value` or `format_bad_pattern`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadOption { .. } => "format_bad_option",
            Self::BadValue(_) => "format_bad_value",
            Self::BadPattern { .. } => "format_bad_pattern",
        }
    }

    fn option(name: &str, reason: impl Into<String>) -> Self {
        Self::BadOption {
            name: name.to_owned(),
            reason: reason.into(),
        }
    }
}

// --- Locale chain -------------------------------------------------------------------------------

/// `locale` in canonical form: `_` becomes `-`, the language is lower case, a script title case and
/// a region upper case; extensions (`-u-…`, `-x-…`) are dropped. An empty tag or `und` is `root`.
#[must_use]
pub fn canonical_tag(locale: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (i, sub) in locale.split(['-', '_']).enumerate() {
        if sub.is_empty() || (i > 0 && sub.len() == 1) {
            break;
        }
        let part = if i == 0 {
            sub.to_ascii_lowercase()
        } else if sub.len() == 4 && sub.bytes().all(|b| b.is_ascii_alphabetic()) {
            let lower = sub.to_ascii_lowercase();
            let mut chars = lower.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_ascii_uppercase().to_string() + chars.as_str()
            })
        } else if (sub.len() == 2 && sub.bytes().all(|b| b.is_ascii_alphabetic()))
            || (sub.len() == 3 && sub.bytes().all(|b| b.is_ascii_digit()))
        {
            sub.to_ascii_uppercase()
        } else {
            sub.to_ascii_lowercase()
        };
        parts.push(part);
    }
    match parts.first().map(String::as_str) {
        None | Some("und" | "root") => "root".to_owned(),
        _ => parts.join("-"),
    }
}

fn row(tag: &str) -> Option<&'static LocaleData> {
    data::LOCALES
        .binary_search_by(|r| r.tag.cmp(tag))
        .ok()
        .and_then(|i| data::LOCALES.get(i))
}

/// The tags the data lookup of `locale` walks, in order, always ending in `root` (`pt-BR` → `pt` →
/// `root`; `zh-TW` → `zh-Hant` → `root`). A tag without its own row is still listed: it simply
/// contributes nothing.
#[must_use]
pub fn locale_chain(locale: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut tag = canonical_tag(locale);
    // A chain is a handful of tags; the bound only guards against a parent cycle in the data.
    for _ in 0..12 {
        out.push(tag.clone());
        if tag == "root" {
            return out;
        }
        tag = match row(&tag).and_then(|r| r.parent) {
            Some(parent) => parent.to_owned(),
            None => tag
                .rsplit_once('-')
                .map_or_else(|| "root".to_owned(), |(p, _)| p.to_owned()),
        };
    }
    out.push("root".to_owned());
    out
}

/// Whether the table has a row of its own for `tag` (canonical form).
#[must_use]
pub fn has_locale_data(tag: &str) -> bool {
    row(&canonical_tag(tag)).is_some()
}

/// Every tag the table has a row for, sorted (`root` included).
#[must_use]
pub fn data_locales() -> Vec<&'static str> {
    data::LOCALES.iter().map(|r| r.tag).collect()
}

/// The CLDR id of `locale`'s native digits (`arab`, `deva`, `thai`), which
/// `numberingSystem: "native"` selects; `None` when the table has none for it.
#[must_use]
pub fn native_numbering_system(locale: &str) -> Option<&'static str> {
    Culture::of(locale).native.map(|n| n.system)
}

/// The resolved culture of one locale: each field from the first row of the chain that has it.
struct Culture {
    tag: String,
    rows: Vec<&'static LocaleData>,
    number: &'static NumberData,
    native: Option<&'static NativeData>,
    calendar: &'static CalendarData,
    dates: &'static DatePatterns,
    hour12: bool,
    list: &'static ListData,
    relative: &'static RelativeData,
}

impl Culture {
    fn of(locale: &str) -> Self {
        let chain = locale_chain(locale);
        let rows: Vec<&'static LocaleData> = chain.iter().filter_map(|t| row(t)).collect();
        fn pick<T: Copy>(
            rows: &[&'static LocaleData],
            f: impl Fn(&'static LocaleData) -> Option<T>,
            root: T,
        ) -> T {
            rows.iter().copied().find_map(f).unwrap_or(root)
        }
        Self {
            tag: chain.first().cloned().unwrap_or_else(|| "root".to_owned()),
            number: pick(&rows, |r| r.number, &data::ROOT_NUM),
            native: rows.iter().find_map(|r| r.native),
            calendar: pick(&rows, |r| r.calendar, &data::ROOT_CAL),
            dates: pick(&rows, |r| r.dates, &data::ROOT_DATES),
            hour12: pick(&rows, |r| r.hour12, false),
            list: pick(&rows, |r| r.list, &data::ROOT_LIST),
            relative: pick(&rows, |r| r.relative, &data::ROOT_REL),
            rows,
        }
    }

    /// The symbols and digits of `system`.
    fn system(&self, system: NumberingSystem) -> (&'static NumberData, Option<&'static [char; 10]>) {
        match (system, self.native) {
            (NumberingSystem::Native, Some(native)) => {
                (native.number.unwrap_or(self.number), Some(&native.digits))
            }
            _ => (self.number, None),
        }
    }

    fn currency_symbol(&self, currency: &Currency) -> &'static str {
        self.rows
            .iter()
            .find_map(|r| {
                r.currencies
                    .iter()
                    .find(|(code, _)| *code == currency.code)
                    .map(|(_, s)| *s)
            })
            .unwrap_or(currency.symbol)
    }
}

// --- Options ------------------------------------------------------------------------------------

/// One option value, as Fluent named arguments and agent JSON carry them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OptionValue<'a> {
    /// A string (`"currency"`, `"true"`).
    Str(&'a str),
    /// A number (`2`).
    Num(f64),
}

impl OptionValue<'_> {
    fn text(self, name: &str) -> Result<String, FormatError> {
        match self {
            Self::Str(s) => Ok(s.to_owned()),
            Self::Num(n) => Err(FormatError::option(name, format!("expected a string, got {n}"))),
        }
    }

    fn int(self, name: &str, lo: u8, hi: u8) -> Result<u8, FormatError> {
        let n = match self {
            Self::Num(n) => n,
            Self::Str(s) => s
                .trim()
                .parse::<f64>()
                .map_err(|_| FormatError::option(name, format!("expected a number, got {s:?}")))?,
        };
        if n.fract() != 0.0 || !(f64::from(lo)..=f64::from(hi)).contains(&n) {
            return Err(FormatError::option(
                name,
                format!("must be a whole number from {lo} to {hi}, got {n}"),
            ));
        }
        // In range lo..=hi (≤ 100) and whole, so the cast is exact.
        Ok(n as u8)
    }

    fn flag(self, name: &str) -> Result<bool, FormatError> {
        match self {
            Self::Str("true") => Ok(true),
            Self::Str("false") => Ok(false),
            other => Err(FormatError::option(
                name,
                format!("must be \"true\" or \"false\", got {other:?}"),
            )),
        }
    }
}

/// `style` of [`NumberOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NumberStyle {
    /// A plain number.
    #[default]
    Decimal,
    /// A ratio shown as a percentage (`0.25` → `25%`).
    Percent,
    /// An amount of [`NumberOptions::currency`].
    Currency,
}

/// `currencyDisplay` of [`NumberOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CurrencyDisplay {
    /// The locale's symbol (`$` in `en-US`, `US$` in `zh`).
    #[default]
    Symbol,
    /// The narrow symbol (`$` for every dollar).
    NarrowSymbol,
    /// The ISO 4217 code (`USD`).
    Code,
}

/// How a number is rounded to its last shown digit (`roundingMode`, the ECMA-402 names).
///
/// The digits rounded are the shortest decimal that reads back as the same `f64` (what Rust's
/// `Display` prints), so `1.005` with two fraction digits is `1.01` under `HalfExpand`, the number
/// a designer typed, not the binary value just below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoundingMode {
    /// Toward +∞.
    Ceil,
    /// Toward −∞.
    Floor,
    /// Away from zero.
    Expand,
    /// Toward zero.
    Trunc,
    /// Nearest; ties toward +∞.
    HalfCeil,
    /// Nearest; ties toward −∞.
    HalfFloor,
    /// Nearest; ties away from zero (the default, "school" rounding: 2.5 → 3, −2.5 → −3).
    #[default]
    HalfExpand,
    /// Nearest; ties toward zero.
    HalfTrunc,
    /// Nearest; ties to the even digit (banker's rounding: 2.5 → 2, 3.5 → 4).
    HalfEven,
}

impl RoundingMode {
    /// Parses an ECMA-402 name (`halfExpand`).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "ceil" => Self::Ceil,
            "floor" => Self::Floor,
            "expand" => Self::Expand,
            "trunc" => Self::Trunc,
            "halfCeil" => Self::HalfCeil,
            "halfFloor" => Self::HalfFloor,
            "halfExpand" => Self::HalfExpand,
            "halfTrunc" => Self::HalfTrunc,
            "halfEven" => Self::HalfEven,
            _ => return None,
        })
    }
}

/// `useGrouping` of [`NumberOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grouping {
    /// The locale's rule (Spanish and Polish leave four-digit numbers ungrouped).
    #[default]
    Auto,
    /// Group from four digits on.
    Always,
    /// Group from five digits on.
    Min2,
    /// Never group.
    Off,
}

/// `signDisplay` of [`NumberOptions`] (ECMA-402).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignDisplay {
    /// Minus for negative numbers, negative zero included (`-0`).
    #[default]
    Auto,
    /// Plus or minus on every number.
    Always,
    /// No sign.
    Never,
    /// Plus or minus except on zero.
    ExceptZero,
    /// Minus for negative numbers that do not round to zero.
    Negative,
}

/// `numberingSystem`: which digits to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NumberingSystem {
    /// ASCII digits `0`-`9` (the default in every locale).
    #[default]
    Latin,
    /// The locale's native digits when the data has them (Arabic-Indic for `ar`, Devanagari for
    /// `hi`, Thai for `th`); Latin otherwise.
    Native,
}

/// Options of [`format_number`]. The names follow Fluent's `NUMBER()` (ECMA-402).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NumberOptions {
    /// `style`.
    pub style: NumberStyle,
    /// `currency`: an ISO 4217 code (required with [`NumberStyle::Currency`]).
    pub currency: Option<String>,
    /// `currencyDisplay`.
    pub currency_display: CurrencyDisplay,
    /// `useGrouping`.
    pub grouping: Grouping,
    /// `minimumIntegerDigits` (1 when `None`).
    pub minimum_integer_digits: Option<u8>,
    /// `minimumFractionDigits` (style default when `None`: 0, or the currency's digits).
    pub minimum_fraction_digits: Option<u8>,
    /// `maximumFractionDigits` (style default when `None`: 3 decimal, 0 percent, currency digits).
    pub maximum_fraction_digits: Option<u8>,
    /// `minimumSignificantDigits`; with either significant option set, fraction options are ignored.
    pub minimum_significant_digits: Option<u8>,
    /// `maximumSignificantDigits`.
    pub maximum_significant_digits: Option<u8>,
    /// `roundingMode`.
    pub rounding_mode: RoundingMode,
    /// `signDisplay`.
    pub sign_display: SignDisplay,
    /// `numberingSystem`.
    pub numbering: NumberingSystem,
    /// `type`: `cardinal` (the default) selects cardinal plural categories; `ordinal` makes
    /// `NUMBER()` results select ordinal categories (`1st`, `2nd`, `3rd`) in plural selectors
    /// (ADR-3359).
    pub kind: crate::plural::PluralKind,
}

fn numbering(name: &str, v: OptionValue<'_>) -> Result<NumberingSystem, FormatError> {
    match v.text(name)?.as_str() {
        "latn" => Ok(NumberingSystem::Latin),
        "native" | "arab" | "deva" | "thai" => Ok(NumberingSystem::Native),
        other => Err(FormatError::option(
            name,
            format!("unknown numbering system {other:?} (latn or native)"),
        )),
    }
}

impl NumberOptions {
    /// Reads Fluent/ECMA-402 option names: `style`, `currency`, `currencyDisplay`, `useGrouping`,
    /// `minimumIntegerDigits`, `minimumFractionDigits`, `maximumFractionDigits`,
    /// `minimumSignificantDigits`, `maximumSignificantDigits`, `roundingMode`, `signDisplay`,
    /// `numberingSystem` and `type` (`cardinal` | `ordinal`). Other names (`notation`, …) are
    /// returned as ignored, the way Fluent ignores options it does not know.
    ///
    /// # Errors
    ///
    /// A known option with a value it does not accept ([`FormatError::BadOption`]).
    pub fn from_options<'a>(
        options: impl IntoIterator<Item = (&'a str, OptionValue<'a>)>,
    ) -> Result<(Self, Vec<String>), FormatError> {
        let mut out = Self::default();
        let mut ignored = Vec::new();
        for (name, v) in options {
            match name {
                "style" => {
                    out.style = match v.text(name)?.as_str() {
                        "decimal" => NumberStyle::Decimal,
                        "percent" => NumberStyle::Percent,
                        "currency" => NumberStyle::Currency,
                        other => return Err(FormatError::option(name, format!("unknown style {other:?}"))),
                    }
                }
                "currency" => {
                    let code = v.text(name)?;
                    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
                        return Err(FormatError::option(
                            name,
                            format!("{code:?} is not an ISO 4217 code"),
                        ));
                    }
                    out.currency = Some(code.to_ascii_uppercase());
                }
                "currencyDisplay" => {
                    out.currency_display = match v.text(name)?.as_str() {
                        "symbol" => CurrencyDisplay::Symbol,
                        "narrowSymbol" => CurrencyDisplay::NarrowSymbol,
                        "code" => CurrencyDisplay::Code,
                        "name" => {
                            return Err(FormatError::option(
                                name,
                                "\"name\" is not supported (no currency names in the table)",
                            ));
                        }
                        other => return Err(FormatError::option(name, format!("unknown display {other:?}"))),
                    }
                }
                "useGrouping" => {
                    out.grouping = match v {
                        OptionValue::Str("true" | "auto") => Grouping::Auto,
                        OptionValue::Str("always") => Grouping::Always,
                        OptionValue::Str("min2") => Grouping::Min2,
                        OptionValue::Str("false") => Grouping::Off,
                        OptionValue::Num(0.0) => Grouping::Off,
                        OptionValue::Num(1.0) => Grouping::Auto,
                        other => {
                            return Err(FormatError::option(
                                name,
                                format!("must be auto, always, min2, true or false, got {other:?}"),
                            ));
                        }
                    }
                }
                "minimumIntegerDigits" => {
                    out.minimum_integer_digits = Some(v.int(name, 1, MAX_SIGNIFICANT_DIGITS)?);
                }
                "minimumFractionDigits" => {
                    out.minimum_fraction_digits = Some(v.int(name, 0, MAX_FRACTION_DIGITS)?);
                }
                "maximumFractionDigits" => {
                    out.maximum_fraction_digits = Some(v.int(name, 0, MAX_FRACTION_DIGITS)?);
                }
                "minimumSignificantDigits" => {
                    out.minimum_significant_digits = Some(v.int(name, 1, MAX_SIGNIFICANT_DIGITS)?);
                }
                "maximumSignificantDigits" => {
                    out.maximum_significant_digits = Some(v.int(name, 1, MAX_SIGNIFICANT_DIGITS)?);
                }
                "roundingMode" => {
                    let text = v.text(name)?;
                    out.rounding_mode = RoundingMode::parse(&text).ok_or_else(|| {
                        FormatError::option(name, format!("unknown rounding mode {text:?}"))
                    })?;
                }
                "signDisplay" => {
                    out.sign_display = match v.text(name)?.as_str() {
                        "auto" => SignDisplay::Auto,
                        "always" => SignDisplay::Always,
                        "never" => SignDisplay::Never,
                        "exceptZero" => SignDisplay::ExceptZero,
                        "negative" => SignDisplay::Negative,
                        other => {
                            return Err(FormatError::option(
                                name,
                                format!("unknown sign display {other:?}"),
                            ));
                        }
                    }
                }
                "numberingSystem" => out.numbering = numbering(name, v)?,
                "type" => {
                    out.kind = match v.text(name)?.as_str() {
                        "cardinal" => crate::plural::PluralKind::Cardinal,
                        "ordinal" => crate::plural::PluralKind::Ordinal,
                        other => {
                            return Err(FormatError::option(
                                name,
                                format!("must be cardinal or ordinal, got {other:?}"),
                            ));
                        }
                    };
                }
                other => ignored.push(other.to_owned()),
            }
        }
        Ok((out, ignored))
    }
}

// --- Decimal digits and rounding ----------------------------------------------------------------

/// A non-negative decimal `0.d0 d1 d2 … × 10^exp` with no trailing zero digits (zero is no digits).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Decimal {
    digits: Vec<u8>,
    exp: i64,
}

impl Decimal {
    /// The shortest decimal that reads back as `|value|` (finite values only).
    fn of(value: f64) -> Self {
        let text = format!("{:e}", value.abs());
        let (mantissa, exponent) = text.split_once('e').unwrap_or((text.as_str(), "0"));
        let exponent: i64 = exponent.parse().unwrap_or(0);
        let mut digits: Vec<u8> = mantissa
            .bytes()
            .filter(u8::is_ascii_digit)
            .map(|b| b - b'0')
            .collect();
        while digits.last() == Some(&0) {
            digits.pop();
        }
        if digits.is_empty() {
            return Self { digits, exp: 0 };
        }
        Self {
            digits,
            exp: exponent + 1,
        }
    }

    fn is_zero(&self) -> bool {
        self.digits.is_empty()
    }

    /// Keeps the first `keep` digits (counting from `digits[0]`), rounding the rest away with
    /// `mode`; `negative` is the sign of the number for the directed modes.
    fn round(&mut self, keep: i64, mode: RoundingMode, negative: bool) {
        let n = self.digits.len() as i64;
        if self.is_zero() || keep >= n {
            return;
        }
        let digit = |i: i64| -> u8 {
            usize::try_from(i)
                .ok()
                .and_then(|i| self.digits.get(i))
                .copied()
                .unwrap_or(0)
        };
        let first_dropped = if keep < 0 { 0 } else { digit(keep) };
        let rest_nonzero = if keep < 0 {
            true
        } else {
            ((keep + 1)..n).any(|i| digit(i) != 0)
        };
        let last_odd = keep > 0 && digit(keep - 1) % 2 == 1;
        // The dropped part is never zero: digits carry no trailing zeros.
        let above = first_dropped > 5 || (first_dropped == 5 && rest_nonzero);
        let half = first_dropped == 5 && !rest_nonzero;
        let up = match mode {
            RoundingMode::Ceil => !negative,
            RoundingMode::Floor => negative,
            RoundingMode::Expand => true,
            RoundingMode::Trunc => false,
            RoundingMode::HalfCeil => above || (half && !negative),
            RoundingMode::HalfFloor => above || (half && negative),
            RoundingMode::HalfExpand => above || half,
            RoundingMode::HalfTrunc => above,
            RoundingMode::HalfEven => above || (half && last_odd),
        };
        if keep <= 0 {
            self.digits.clear();
            if up {
                self.digits.push(1);
                self.exp = self.exp - keep + 1;
            } else {
                self.exp = 0;
            }
            return;
        }
        self.digits.truncate(usize::try_from(keep).unwrap_or(0));
        if up {
            let mut carry = true;
            for d in self.digits.iter_mut().rev() {
                if *d == 9 {
                    *d = 0;
                } else {
                    *d += 1;
                    carry = false;
                    break;
                }
            }
            if carry {
                self.digits.insert(0, 1);
                self.exp += 1;
            }
        }
        while self.digits.last() == Some(&0) {
            self.digits.pop();
        }
        if self.digits.is_empty() {
            self.exp = 0;
        }
    }

    /// Integer digits and fraction digits as ASCII.
    fn split(&self) -> (String, String) {
        let n = self.digits.len() as i64;
        let ascii = |d: &u8| char::from(b'0' + d);
        let int = if self.exp <= 0 {
            "0".to_owned()
        } else {
            let take = usize::try_from(self.exp.min(n)).unwrap_or(0);
            let mut s: String = self.digits.iter().take(take).map(ascii).collect();
            for _ in n..self.exp {
                s.push('0');
            }
            s
        };
        let frac = if self.exp >= n {
            String::new()
        } else if self.exp >= 0 {
            self.digits
                .iter()
                .skip(usize::try_from(self.exp).unwrap_or(0))
                .map(ascii)
                .collect()
        } else {
            let mut s = "0".repeat(usize::try_from(-self.exp).unwrap_or(0));
            s.extend(self.digits.iter().map(ascii));
            s
        };
        (int, frac)
    }
}

fn group_digits(int: &str, sep: &str, data: &NumberData, grouping: Grouping) -> String {
    let min = match grouping {
        Grouping::Off => return int.to_owned(),
        Grouping::Always => 1,
        Grouping::Min2 => 2,
        Grouping::Auto => usize::from(data.min_grouping),
    };
    let primary = usize::from(data.primary.max(1));
    let secondary = usize::from(data.secondary.max(1));
    if int.len() < primary + min {
        return int.to_owned();
    }
    let bytes = int.as_bytes();
    let mut groups: Vec<&[u8]> = Vec::new();
    let mut end = bytes.len();
    let mut size = primary;
    while end > 0 {
        let start = end.saturating_sub(size);
        groups.push(&bytes[start..end]);
        end = start;
        size = secondary;
    }
    groups.reverse();
    groups
        .iter()
        .map(|g| String::from_utf8_lossy(g).into_owned())
        .collect::<Vec<_>>()
        .join(sep)
}

fn to_native(text: &str, digits: Option<&[char; 10]>) -> String {
    match digits {
        None => text.to_owned(),
        Some(native) => text
            .chars()
            .map(|c| {
                c.to_digit(10)
                    .and_then(|d| native.get(d as usize).copied())
                    .unwrap_or(c)
            })
            .collect(),
    }
}

/// The digits after rounding and padding, before symbols: `(integer, fraction)` in ASCII.
struct Rounded {
    int: String,
    frac: String,
    zero: bool,
}

fn round_digits(
    value: f64,
    options: &NumberOptions,
    default_min: u8,
    default_max: u8,
) -> Result<Rounded, FormatError> {
    let negative = value.is_sign_negative();
    let mut d = Decimal::of(value);
    if options.style == NumberStyle::Percent && !d.is_zero() {
        d.exp += 2;
    }
    let significant =
        options.minimum_significant_digits.is_some() || options.maximum_significant_digits.is_some();
    let min_frac = if significant {
        let min_sig = options.minimum_significant_digits.unwrap_or(1);
        let max_sig = options
            .maximum_significant_digits
            .unwrap_or(MAX_SIGNIFICANT_DIGITS);
        if min_sig > max_sig {
            return Err(FormatError::option(
                "minimumSignificantDigits",
                format!("{min_sig} is more than maximumSignificantDigits {max_sig}"),
            ));
        }
        d.round(i64::from(max_sig), options.rounding_mode, negative);
        let int_digits = if d.is_zero() { 1 } else { d.exp.max(1) };
        (i64::from(min_sig) - int_digits).max(0)
    } else {
        let (lo, hi) = match (options.minimum_fraction_digits, options.maximum_fraction_digits) {
            (None, None) => (default_min, default_max),
            (Some(lo), None) => (lo, lo.max(default_max)),
            (None, Some(hi)) => (default_min.min(hi), hi),
            (Some(lo), Some(hi)) if lo > hi => {
                return Err(FormatError::option(
                    "minimumFractionDigits",
                    format!("{lo} is more than maximumFractionDigits {hi}"),
                ));
            }
            (Some(lo), Some(hi)) => (lo, hi),
        };
        d.round(d.exp + i64::from(hi), options.rounding_mode, negative);
        i64::from(lo)
    };
    let zero = d.is_zero();
    let (mut int, mut frac) = d.split();
    let min_frac = usize::try_from(min_frac.max(0)).unwrap_or(0);
    while frac.len() < min_frac {
        frac.push('0');
    }
    let min_int = usize::from(options.minimum_integer_digits.unwrap_or(1));
    if int.len() < min_int {
        int = "0".repeat(min_int - int.len()) + &int;
    }
    Ok(Rounded { int, frac, zero })
}

fn sign_of(value: f64, zero: bool, display: SignDisplay) -> Option<bool> {
    let negative = value.is_sign_negative();
    match display {
        SignDisplay::Auto => negative.then_some(false),
        SignDisplay::Always => Some(!negative),
        SignDisplay::Never => None,
        SignDisplay::ExceptZero if zero => None,
        SignDisplay::ExceptZero => Some(!negative),
        SignDisplay::Negative => (negative && !zero).then_some(false),
    }
}

/// Formats `value` in `locale` (see the module docs).
///
/// # Errors
///
/// [`FormatError::BadOption`] for inconsistent options (minimum above maximum, a currency style
/// without a currency code).
pub fn format_number(value: f64, locale: &str, options: &NumberOptions) -> Result<String, FormatError> {
    let culture = Culture::of(locale);
    format_number_in(&culture, value, options)
}

fn currency_of(options: &NumberOptions) -> Result<Option<Currency>, FormatError> {
    if options.style != NumberStyle::Currency {
        return Ok(None);
    }
    let code = options
        .currency
        .as_deref()
        .ok_or_else(|| FormatError::option("currency", "style \"currency\" needs a currency code"))?;
    // An unlisted (well-formed) code has 2 digits and shows its code: the empty `code` marks it.
    Ok(Some(data::CURRENCIES.iter().find(|c| c.code == code).map_or(
        Currency {
            code: "",
            digits: 2,
            symbol: "",
            narrow: "",
        },
        |c| Currency {
            code: c.code,
            digits: c.digits,
            symbol: c.symbol,
            narrow: c.narrow,
        },
    )))
}

fn format_number_in(culture: &Culture, value: f64, options: &NumberOptions) -> Result<String, FormatError> {
    let (symbols, native) = culture.system(options.numbering);
    let currency = currency_of(options)?;
    let code = options.currency.clone().unwrap_or_default();
    let (default_min, default_max) = match (options.style, &currency) {
        (NumberStyle::Currency, Some(c)) => (c.digits, c.digits),
        (NumberStyle::Percent, _) => (0, 0),
        _ => (0, 3),
    };
    let (body, zero) = if value.is_nan() {
        (symbols.nan.to_owned(), true)
    } else if value.is_infinite() {
        ("∞".to_owned(), false)
    } else {
        let r = round_digits(value, options, default_min, default_max)?;
        let int = group_digits(&r.int, symbols.group, symbols, options.grouping);
        let mut body = to_native(&int, native);
        if !r.frac.is_empty() {
            body.push_str(symbols.decimal);
            body.push_str(&to_native(&r.frac, native));
        }
        (body, r.zero)
    };
    let sign = if value.is_nan() {
        None
    } else {
        sign_of(value, zero, options.sign_display)
    };
    let pattern = match options.style {
        NumberStyle::Decimal => "#",
        NumberStyle::Percent => symbols.percent,
        NumberStyle::Currency => match (sign, symbols.currency_negative) {
            (Some(false), Some(negative)) => negative,
            _ => symbols.currency,
        },
    };
    let symbol: String = match (&currency, options.currency_display) {
        (None, _) => String::new(),
        (Some(c), _) if c.code.is_empty() => code.clone(),
        (Some(_), CurrencyDisplay::Code) => code.clone(),
        (Some(c), CurrencyDisplay::NarrowSymbol) => c.narrow.to_owned(),
        (Some(c), CurrencyDisplay::Symbol) => culture.currency_symbol(c).to_owned(),
    };
    let pattern_has_minus = pattern.contains('-');
    let mut out = String::new();
    if let Some(positive) = sign
        && !pattern_has_minus
    {
        out.push_str(if positive { symbols.plus } else { symbols.minus });
    }
    let mut chars = pattern.chars().peekable();
    let mut prev: Option<char> = None;
    while let Some(c) = chars.next() {
        match c {
            '#' => {
                if prev == Some('¤') && symbol.chars().next_back().is_some_and(char::is_alphabetic) {
                    out.push('\u{a0}');
                }
                out.push_str(&body);
                if chars.peek() == Some(&'¤') && symbol.chars().next().is_some_and(char::is_alphabetic) {
                    out.push('\u{a0}');
                }
            }
            '¤' => out.push_str(&symbol),
            '-' => out.push_str(symbols.minus),
            other => out.push(other),
        }
        prev = Some(c);
    }
    Ok(out)
}

/// The plain operands text of a rounded number (`1.50`), used for plural selection.
fn operands_text(value: f64, options: &NumberOptions) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    let currency = currency_of(options).ok()?;
    let (lo, hi) = match (options.style, &currency) {
        (NumberStyle::Currency, Some(c)) => (c.digits, c.digits),
        (NumberStyle::Percent, _) => (0, 0),
        _ => (0, 3),
    };
    let r = round_digits(value, options, lo, hi).ok()?;
    Some(if r.frac.is_empty() {
        r.int
    } else {
        format!("{}.{}", r.int, r.frac)
    })
}

// --- Dates --------------------------------------------------------------------------------------

/// A wall-clock date and time in the proleptic Gregorian calendar.
///
/// `offset_seconds` is the UTC offset of this wall time when known (`Some(3600)` for `+01:00`);
/// `None` is a floating local time (a date typed by a designer), shown as is and never converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CivilDateTime {
    /// Year (astronomical: 0 is 1 BC).
    pub year: i32,
    /// Month 1-12.
    pub month: u8,
    /// Day of the month 1-31.
    pub day: u8,
    /// Hour 0-23.
    pub hour: u8,
    /// Minute 0-59.
    pub minute: u8,
    /// Second 0-59.
    pub second: u8,
    /// UTC offset in seconds, when known.
    pub offset_seconds: Option<i32>,
}

// Days from 1970-01-01 to y-m-d and back: the public-domain civil calendar algorithms of Howard
// Hinnant ("chrono-Compatible Low-Level Date Algorithms"), re-derived; see docs/provenance.md.
fn days_from_civil(year: i64, month: u8, day: u8) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    // month is 1..=12 and day 1..=31 by construction.
    (year, month as u8, day as u8)
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

impl CivilDateTime {
    /// A floating date at midnight.
    ///
    /// # Errors
    ///
    /// The date does not exist (`2026-02-30`).
    pub fn date(year: i32, month: u8, day: u8) -> Result<Self, FormatError> {
        let out = Self {
            year,
            month,
            day,
            ..Self::default()
        };
        out.validate()?;
        Ok(out)
    }

    /// Checks every field's range.
    ///
    /// # Errors
    ///
    /// A field is out of range ([`FormatError::BadValue`]).
    pub fn validate(&self) -> Result<(), FormatError> {
        if !(1..=12).contains(&self.month) || self.day == 0 || self.day > days_in_month(self.year, self.month)
        {
            return Err(FormatError::BadValue(format!(
                "{:04}-{:02}-{:02} is not a date",
                self.year, self.month, self.day
            )));
        }
        if self.hour > 23 || self.minute > 59 || self.second > 59 {
            return Err(FormatError::BadValue(format!(
                "{:02}:{:02}:{:02} is not a time",
                self.hour, self.minute, self.second
            )));
        }
        if let Some(offset) = self.offset_seconds
            && offset.abs() > MAX_UTC_OFFSET_SECONDS
        {
            return Err(FormatError::BadValue(format!(
                "UTC offset {offset} s is beyond ±18 h"
            )));
        }
        Ok(())
    }

    /// The wall time `offset_seconds` from UTC at the instant `unix_seconds` (seconds since
    /// 1970-01-01T00:00:00Z). The game passes the player's offset; there is no time-zone database.
    ///
    /// # Errors
    ///
    /// The offset is beyond ±18 hours, or the instant is beyond the representable years.
    pub fn from_unix(unix_seconds: i64, offset_seconds: i32) -> Result<Self, FormatError> {
        if offset_seconds.abs() > MAX_UTC_OFFSET_SECONDS {
            return Err(FormatError::BadValue(format!(
                "UTC offset {offset_seconds} s is beyond ±18 h"
            )));
        }
        let local = unix_seconds
            .checked_add(i64::from(offset_seconds))
            .ok_or_else(|| FormatError::BadValue("time out of range".to_owned()))?;
        let days = local.div_euclid(86_400);
        let secs = local.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let year = i32::try_from(year).map_err(|_| FormatError::BadValue("year out of range".to_owned()))?;
        // secs is 0..86400, so each part fits u8.
        Ok(Self {
            year,
            month,
            day,
            hour: (secs / 3600) as u8,
            minute: (secs / 60 % 60) as u8,
            second: (secs % 60) as u8,
            offset_seconds: Some(offset_seconds),
        })
    }

    /// Seconds since the unix epoch, reading a floating time as UTC.
    #[must_use]
    pub fn to_unix(&self) -> i64 {
        let days = days_from_civil(i64::from(self.year), self.month, self.day);
        days * 86_400 + i64::from(self.hour) * 3600 + i64::from(self.minute) * 60 + i64::from(self.second)
            - i64::from(self.offset_seconds.unwrap_or(0))
    }

    /// The weekday, 0 = Sunday.
    #[must_use]
    pub fn weekday(&self) -> u8 {
        // 1970-01-01 was a Thursday (4).
        (days_from_civil(i64::from(self.year), self.month, self.day) + 4).rem_euclid(7) as u8
    }

    /// The same instant at another UTC offset; a floating time is returned unchanged.
    ///
    /// # Errors
    ///
    /// The offset is beyond ±18 hours.
    pub fn at_offset(&self, offset_seconds: i32) -> Result<Self, FormatError> {
        if self.offset_seconds.is_none() {
            return Ok(*self);
        }
        Self::from_unix(self.to_unix(), offset_seconds)
    }

    /// Parses ISO 8601 / RFC 3339: `2026-11-05`, `2026-11-05T14:30`, `2026-11-05T14:30:05`,
    /// optionally followed by `Z` or `±HH:MM` (fraction seconds are dropped). Without a zone the
    /// result is a floating time.
    ///
    /// # Errors
    ///
    /// The text does not have that shape, or names a date or time that does not exist.
    pub fn parse_iso(text: &str) -> Result<Self, FormatError> {
        let bad = || {
            FormatError::BadValue(format!(
                "{text:?} is not an ISO 8601 date (2026-11-05T14:30:00+01:00)"
            ))
        };
        let text = text.trim();
        let (date, time) = match text.split_once(['T', 't', ' ']) {
            Some((d, t)) => (d, Some(t)),
            None => (text, None),
        };
        let (negative_year, date_body) = match date.strip_prefix('-') {
            Some(body) => (true, body),
            None => (false, date),
        };
        let mut parts = date_body.splitn(3, '-');
        let (Some(year_digits), Some(month), Some(day)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(bad());
        };
        let num = |s: &str, len: usize| -> Option<u32> {
            (s.len() == len && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse().ok())
                .flatten()
        };
        if year_digits.len() < 4 || year_digits.len() > 6 || !year_digits.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(bad());
        }
        let mut year: i32 = year_digits.parse().map_err(|_| bad())?;
        if negative_year {
            year = -year;
        }
        let month = num(month, 2).ok_or_else(bad)?;
        let day = num(day, 2).ok_or_else(bad)?;
        let mut out = Self {
            year,
            month: u8::try_from(month).map_err(|_| bad())?,
            day: u8::try_from(day).map_err(|_| bad())?,
            ..Self::default()
        };
        if let Some(time) = time {
            let (clock, zone) = if let Some(c) = time.strip_suffix(['Z', 'z']) {
                (c, Some(0))
            } else if let Some(i) = time.rfind(['+', '-']) {
                let (c, z) = time.split_at(i);
                (c, Some(parse_offset(z).ok_or_else(bad)?))
            } else {
                (time, None)
            };
            let clock = clock.split_once('.').map_or(clock, |(c, _)| c);
            let fields: Vec<&str> = clock.split(':').collect();
            if !(2..=3).contains(&fields.len()) {
                return Err(bad());
            }
            let field = |i: usize| -> Result<u8, FormatError> {
                match fields.get(i) {
                    None => Ok(0),
                    Some(s) => num(s, 2).and_then(|n| u8::try_from(n).ok()).ok_or_else(bad),
                }
            };
            out.hour = field(0)?;
            out.minute = field(1)?;
            out.second = field(2)?;
            out.offset_seconds = zone;
        }
        out.validate()?;
        Ok(out)
    }

    /// ISO 8601 text (`2026-11-05T14:30:00+01:00`; no zone for a floating time): the form a game
    /// passes to `DATETIME()` through string arguments.
    #[must_use]
    pub fn to_iso(&self) -> String {
        let mut out = if self.year < 0 {
            format!("-{:04}", -i64::from(self.year))
        } else {
            format!("{:04}", self.year)
        };
        out.push_str(&format!(
            "-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.month, self.day, self.hour, self.minute, self.second
        ));
        if let Some(offset) = self.offset_seconds {
            if offset == 0 {
                out.push('Z');
            } else {
                let sign = if offset < 0 { '-' } else { '+' };
                let a = offset.unsigned_abs();
                out.push_str(&format!("{sign}{:02}:{:02}", a / 3600, a / 60 % 60));
            }
        }
        out
    }
}

/// `+HH:MM`, `-HHMM`, `+HH`, `UTC`, `GMT`, `Z` (also `Etc/UTC`) in seconds; `None` otherwise (IANA
/// names are not read: there is no time-zone database).
#[must_use]
pub fn parse_offset(text: &str) -> Option<i32> {
    match text {
        "Z" | "z" | "UTC" | "GMT" | "Etc/UTC" | "Etc/GMT" => return Some(0),
        _ => {}
    }
    let (sign, rest) = match text.as_bytes().first() {
        Some(b'+') => (1, &text[1..]),
        Some(b'-') => (-1, &text[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) || !(digits.len() == 2 || digits.len() == 4) {
        return None;
    }
    if rest.contains(':') && rest.len() != 5 {
        return None;
    }
    let hours: i32 = digits.get(0..2)?.parse().ok()?;
    let minutes: i32 = digits
        .get(2..)
        .filter(|m| !m.is_empty())
        .map_or(Some(0), |m| m.parse().ok())?;
    if hours > 18 || minutes > 59 {
        return None;
    }
    let total = sign * (hours * 3600 + minutes * 60);
    (total.abs() <= MAX_UTC_OFFSET_SECONDS).then_some(total)
}

/// A date or time style (`dateStyle`, `timeStyle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateStyle {
    /// `11/5/26`, `3:45 PM`.
    Short,
    /// `Nov 5, 2026`, `3:45:10 PM`.
    Medium,
    /// `November 5, 2026`, `3:45:10 PM GMT+1`.
    Long,
    /// `Thursday, November 5, 2026`; time as long.
    Full,
}

impl DateStyle {
    /// Parses `short`, `medium`, `long` or `full`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "short" => Self::Short,
            "medium" => Self::Medium,
            "long" => Self::Long,
            "full" => Self::Full,
            _ => return None,
        })
    }
}

/// `hourCycle`: which clock and which first hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HourCycle {
    /// 0-11 (Japanese 午後0:30).
    H11,
    /// 1-12.
    H12,
    /// 0-23.
    H23,
    /// 1-24.
    H24,
}

/// Options of [`format_datetime`]. The names follow Fluent's `DATETIME()` (ECMA-402).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DateOptions {
    /// `dateStyle`; with neither style set the date is shown `Short`.
    pub date_style: Option<DateStyle>,
    /// `timeStyle`.
    pub time_style: Option<DateStyle>,
    /// `hour12`: the locale's own 12- or 24-hour pattern (Japanese 12-hour counts 0-11). It
    /// overrides `hour_cycle`; with neither set the locale's default clock is used.
    pub hour12: Option<bool>,
    /// `hourCycle`: forces the first hour (`h11` 0-11, `h12` 1-12, `h23` 0-23, `h24` 1-24).
    pub hour_cycle: Option<HourCycle>,
    /// `timeZone` as a fixed offset: the instant is shown at this offset (a floating time is not
    /// converted).
    pub time_zone: Option<i32>,
    /// `numberingSystem`.
    pub numbering: NumberingSystem,
}

impl DateOptions {
    /// Reads `dateStyle`, `timeStyle`, `hour12`, `hourCycle`, `timeZone` (a fixed offset such as
    /// `+02:00` or `UTC`) and `numberingSystem`. The component options of ECMA-402 map to the
    /// nearest style: `month: "long"` → long date (`full` with `weekday`), `month: "short"` →
    /// medium, `month: "numeric"`/`"2-digit"`, `year` or `day` alone → short; `hour`/`minute` →
    /// short time, `second` → medium time. Other names are returned as ignored.
    ///
    /// # Errors
    ///
    /// A known option with a value it does not accept, such as an IANA zone name.
    pub fn from_options<'a>(
        options: impl IntoIterator<Item = (&'a str, OptionValue<'a>)>,
    ) -> Result<(Self, Vec<String>), FormatError> {
        let mut out = Self::default();
        let mut ignored = Vec::new();
        let (mut weekday, mut month, mut ymd, mut hm, mut second) =
            (false, None::<String>, false, false, false);
        for (name, v) in options {
            match name {
                "dateStyle" | "timeStyle" => {
                    let text = v.text(name)?;
                    let style = DateStyle::parse(&text).ok_or_else(|| {
                        FormatError::option(
                            name,
                            format!("unknown style {text:?} (short, medium, long, full)"),
                        )
                    })?;
                    if name == "dateStyle" {
                        out.date_style = Some(style);
                    } else {
                        out.time_style = Some(style);
                    }
                }
                "hour12" => {
                    out.hour12 = Some(v.flag(name)?);
                }
                "hourCycle" => {
                    out.hour_cycle = Some(match v.text(name)?.as_str() {
                        "h11" => HourCycle::H11,
                        "h12" => HourCycle::H12,
                        "h23" => HourCycle::H23,
                        "h24" => HourCycle::H24,
                        other => {
                            return Err(FormatError::option(name, format!("unknown hour cycle {other:?}")));
                        }
                    });
                }
                "timeZone" => {
                    let text = v.text(name)?;
                    out.time_zone = Some(parse_offset(&text).ok_or_else(|| {
                        FormatError::option(
                            name,
                            format!(
                                "{text:?} is not a fixed offset (+02:00, UTC); there is no time-zone database"
                            ),
                        )
                    })?);
                }
                "numberingSystem" => out.numbering = numbering(name, v)?,
                "weekday" => {
                    v.text(name)?;
                    weekday = true;
                }
                "month" => month = Some(v.text(name)?),
                "year" | "day" => {
                    v.text(name)?;
                    ymd = true;
                }
                "hour" | "minute" => {
                    v.text(name)?;
                    hm = true;
                }
                "second" => {
                    v.text(name)?;
                    second = true;
                }
                other => ignored.push(other.to_owned()),
            }
        }
        if out.date_style.is_none() && out.time_style.is_none() {
            out.date_style = match month.as_deref() {
                Some("long") if weekday => Some(DateStyle::Full),
                Some("long") => Some(DateStyle::Long),
                Some("short" | "narrow") => Some(DateStyle::Medium),
                Some(_) => Some(DateStyle::Short),
                None if ymd || weekday => Some(DateStyle::Short),
                None => None,
            };
            if second {
                out.time_style = Some(DateStyle::Medium);
            } else if hm {
                out.time_style = Some(DateStyle::Short);
            }
        }
        Ok((out, ignored))
    }
}

/// Expands a date pattern (UTS #35 letters `y M L d E c h H K k m s a`, `'quoted'` literals).
fn expand_pattern(
    pattern: &str,
    dt: &CivilDateTime,
    calendar: &CalendarData,
    digits: Option<&[char; 10]>,
) -> Result<String, FormatError> {
    let mut out = String::new();
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    let num = |n: i64, width: usize| -> String {
        let text = if n < 0 {
            format!("-{:0width$}", -n)
        } else {
            format!("{n:0width$}")
        };
        to_native(&text, digits)
    };
    while let Some(&c) = chars.get(i) {
        if c == '\'' {
            if chars.get(i + 1) == Some(&'\'') {
                out.push('\'');
                i += 2;
                continue;
            }
            i += 1;
            while let Some(&q) = chars.get(i) {
                if q == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        out.push('\'');
                        i += 2;
                        continue;
                    }
                    break;
                }
                out.push(q);
                i += 1;
            }
            i += 1;
            continue;
        }
        if !c.is_ascii_alphabetic() {
            out.push(c);
            i += 1;
            continue;
        }
        let mut run = 1;
        while chars.get(i + run) == Some(&c) {
            run += 1;
        }
        i += run;
        let hour12 = |zero_based: bool| -> i64 {
            let h = i64::from(dt.hour % 12);
            if zero_based || h != 0 { h } else { 12 }
        };
        let month_index = usize::from(dt.month.saturating_sub(1)).min(11);
        let weekday = usize::from(dt.weekday()).min(6);
        match c {
            'y' if run == 2 => out.push_str(&num(i64::from(dt.year).rem_euclid(100), 2)),
            'y' => out.push_str(&num(i64::from(dt.year), run)),
            'M' | 'L' => match run {
                1 | 2 => out.push_str(&num(i64::from(dt.month), run)),
                3 => out.push_str(calendar.months_abbr.get(month_index).copied().unwrap_or("")),
                _ => out.push_str(calendar.months_wide.get(month_index).copied().unwrap_or("")),
            },
            'd' => out.push_str(&num(i64::from(dt.day), run.min(2))),
            'E' | 'c' => out.push_str(if run >= 4 {
                calendar.weekdays_wide.get(weekday).copied().unwrap_or("")
            } else {
                calendar.weekdays_abbr.get(weekday).copied().unwrap_or("")
            }),
            'h' => out.push_str(&num(hour12(false), run.min(2))),
            'K' => out.push_str(&num(hour12(true), run.min(2))),
            'H' => out.push_str(&num(i64::from(dt.hour), run.min(2))),
            'k' => out.push_str(&num(
                if dt.hour == 0 { 24 } else { i64::from(dt.hour) },
                run.min(2),
            )),
            'm' => out.push_str(&num(i64::from(dt.minute), run.min(2))),
            's' => out.push_str(&num(i64::from(dt.second), run.min(2))),
            'a' => out.push_str(if dt.hour < 12 { calendar.am } else { calendar.pm }),
            other => {
                return Err(FormatError::BadPattern {
                    pattern: pattern.to_owned(),
                    reason: format!("unknown pattern letter {other:?}"),
                });
            }
        }
    }
    Ok(out)
}

/// The time pattern of `style` for a clock: short is the locale's pattern, medium adds seconds
/// after the minutes with the same separator.
fn time_pattern(dates: &DatePatterns, style: DateStyle, twelve: bool, cycle: Option<HourCycle>) -> String {
    let base = if twelve { dates.time12 } else { dates.time24 };
    let mut pattern: String = match cycle {
        Some(HourCycle::H11) => base.replace('h', "K"),
        Some(HourCycle::H12) => base.replace('K', "h"),
        Some(HourCycle::H24) => base.replace('H', "k"),
        Some(HourCycle::H23) => base.replace('k', "H"),
        None => base.to_owned(),
    };
    if style != DateStyle::Short
        && let Some(at) = pattern.find("mm")
    {
        let sep = pattern[..at]
            .chars()
            .next_back()
            .filter(|c| !c.is_ascii_alphabetic())
            .unwrap_or(':');
        pattern.insert_str(at + 2, &format!("{sep}ss"));
    }
    pattern
}

fn offset_text(gmt: &str, offset: i32) -> String {
    if offset == 0 {
        return gmt.to_owned();
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let a = offset.unsigned_abs();
    let (h, m) = (a / 3600, a / 60 % 60);
    if m == 0 {
        format!("{gmt}{sign}{h}")
    } else {
        format!("{gmt}{sign}{h}:{m:02}")
    }
}

/// Formats a date and/or time in `locale` (see [`DateOptions`]).
///
/// # Errors
///
/// The date-time does not exist, or an option is out of range.
pub fn format_datetime(
    dt: &CivilDateTime,
    locale: &str,
    options: &DateOptions,
) -> Result<String, FormatError> {
    dt.validate()?;
    let culture = Culture::of(locale);
    let dt = match options.time_zone {
        Some(offset) => dt.at_offset(offset)?,
        None => *dt,
    };
    let digits = culture.system(options.numbering).1;
    let date_style = match (options.date_style, options.time_style) {
        (None, None) => Some(DateStyle::Short),
        (d, _) => d,
    };
    let date = date_style
        .map(|style| {
            let pattern = match style {
                DateStyle::Short => culture.dates.short,
                DateStyle::Medium => culture.dates.medium,
                DateStyle::Long => culture.dates.long,
                DateStyle::Full => culture.dates.full,
            };
            expand_pattern(pattern, &dt, culture.calendar, digits)
        })
        .transpose()?;
    let time = options
        .time_style
        .map(|style| {
            let (twelve, cycle) = match (options.hour12, options.hour_cycle) {
                (Some(twelve), _) => (twelve, None),
                (None, Some(cycle)) => (matches!(cycle, HourCycle::H11 | HourCycle::H12), Some(cycle)),
                (None, None) => (culture.hour12, None),
            };
            let pattern = time_pattern(culture.dates, style, twelve, cycle);
            let mut text = expand_pattern(&pattern, &dt, culture.calendar, digits)?;
            if matches!(style, DateStyle::Long | DateStyle::Full)
                && let Some(offset) = dt.offset_seconds
            {
                text.push(' ');
                text.push_str(&to_native(&offset_text(culture.dates.gmt, offset), digits));
            }
            Ok::<_, FormatError>(text)
        })
        .transpose()?;
    Ok(match (date, time) {
        (Some(d), Some(t)) => culture.dates.glue.replace("{1}", &d).replace("{0}", &t),
        (Some(d), None) => d,
        (None, Some(t)) => t,
        (None, None) => String::new(),
    })
}

/// Formats the date part of `dt` in `style`.
///
/// # Errors
///
/// The date does not exist.
pub fn format_date(dt: &CivilDateTime, locale: &str, style: DateStyle) -> Result<String, FormatError> {
    format_datetime(
        dt,
        locale,
        &DateOptions {
            date_style: Some(style),
            ..DateOptions::default()
        },
    )
}

/// Formats the time part of `dt` in `style`; `hour12` overrides the locale's clock.
///
/// # Errors
///
/// The time does not exist.
pub fn format_time(
    dt: &CivilDateTime,
    locale: &str,
    style: DateStyle,
    hour12: Option<bool>,
) -> Result<String, FormatError> {
    format_datetime(
        dt,
        locale,
        &DateOptions {
            time_style: Some(style),
            hour12,
            ..DateOptions::default()
        },
    )
}

// --- Relative time ------------------------------------------------------------------------------

/// The unit of [`format_relative`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativeUnit {
    /// Seconds.
    Second,
    /// Minutes.
    Minute,
    /// Hours.
    Hour,
    /// Days.
    Day,
    /// Weeks.
    Week,
    /// Months.
    Month,
    /// Years.
    Year,
}

impl RelativeUnit {
    /// Parses a unit name, singular or plural (`day`, `days`).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.strip_suffix('s').unwrap_or(name) {
            "second" => Self::Second,
            "minute" => Self::Minute,
            "hour" => Self::Hour,
            "day" => Self::Day,
            "week" => Self::Week,
            "month" => Self::Month,
            "year" => Self::Year,
            _ => return None,
        })
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// `numeric` of [`format_relative`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RelativeNumeric {
    /// Always a number ("in 1 day").
    #[default]
    Always,
    /// Words where the locale has them ("tomorrow", "yesterday", "today") for −1, 0 and 1 days.
    Auto,
}

/// "in 3 days" / "2 hours ago": a negative value (negative zero included) is in the past. The
/// number is formatted with `number` (default: up to 3 fraction digits) and selects the unit noun
/// by the language's plural rules for the number as shown (`1.0` is not `one` in English).
///
/// # Errors
///
/// The value is NaN or infinite, or a number option is bad.
pub fn format_relative(
    value: f64,
    unit: RelativeUnit,
    locale: &str,
    numeric: RelativeNumeric,
    number: &NumberOptions,
) -> Result<String, FormatError> {
    if !value.is_finite() {
        return Err(FormatError::BadValue(format!("relative time of {value}")));
    }
    let culture = Culture::of(locale);
    let rel = culture.relative;
    if numeric == RelativeNumeric::Auto
        && unit == RelativeUnit::Day
        && let Some(days) = rel.days
    {
        let word = if value == 0.0 {
            days.get(1)
        } else if value == 1.0 {
            days.get(2)
        } else if value == -1.0 {
            days.first()
        } else {
            None
        };
        if let Some(word) = word {
            return Ok((*word).to_owned());
        }
    }
    let magnitude = value.abs();
    let options = NumberOptions {
        style: NumberStyle::Decimal,
        sign_display: SignDisplay::Never,
        ..number.clone()
    };
    let shown = format_number_in(&culture, magnitude, &options)?;
    let operands = operands_text(magnitude, &options).unwrap_or_else(|| "0".to_owned());
    let category = crate::plural::category_of_text(&culture.tag, &operands).map_or("other", |c| c.as_str());
    let forms = rel.units.get(unit.index()).copied().unwrap_or(&[]);
    let noun = forms
        .iter()
        .find(|(cat, _)| *cat == category)
        .or_else(|| forms.iter().find(|(cat, _)| *cat == "other"))
        .map_or("", |(_, noun)| *noun);
    let template = if value.is_sign_negative() {
        rel.past
    } else {
        rel.future
    };
    Ok(template.replace("{0}", &shown).replace("{1}", noun))
}

// --- Lists --------------------------------------------------------------------------------------

/// `and` or `or` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListType {
    /// "a, b, and c".
    #[default]
    And,
    /// "a, b, or c".
    Or,
}

/// Joins `items` the way `locale` writes a list: nothing for none, the item for one, `two` for a
/// pair, `middle` between earlier items and `end` before the last.
#[must_use]
pub fn format_list(items: &[&str], locale: &str, kind: ListType) -> String {
    let culture = Culture::of(locale);
    let patterns = match kind {
        ListType::And => &culture.list.and,
        ListType::Or => &culture.list.or,
    };
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [a, b] => format!("{a}{}{b}", patterns.two),
        [init @ .., last] => {
            let mut out = init.join(patterns.middle);
            out.push_str(patterns.end);
            out.push_str(last);
            out
        }
    }
}

// --- Fluent functions ---------------------------------------------------------------------------

/// The value `NUMBER()` and `DATETIME()` return: the formatted text, plus (for numbers) the digits
/// as shown, which plural selectors read.
#[derive(Debug, Clone, PartialEq)]
pub struct FormattedValue {
    text: String,
    value: f64,
    plural: Option<String>,
    kind: crate::plural::PluralKind,
}

impl FormattedValue {
    /// The formatted text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Which plural categories the value selects in a selector: cardinal, or ordinal when
    /// `NUMBER(…, type: "ordinal")` formatted it (ADR-3359).
    #[must_use]
    pub fn kind(&self) -> crate::plural::PluralKind {
        self.kind
    }
}

impl FluentType for FormattedValue {
    fn duplicate(&self) -> Box<dyn FluentType + Send> {
        Box::new(self.clone())
    }

    fn as_string(&self, _: &intl_memoizer::IntlLangMemoizer) -> Cow<'static, str> {
        Cow::Owned(self.text.clone())
    }

    fn as_string_threadsafe(&self, _: &intl_memoizer::concurrent::IntlLangMemoizer) -> Cow<'static, str> {
        Cow::Owned(self.text.clone())
    }
}

/// The plural operands of a `NUMBER()` result (`1.00` stays `v = 2`); `None` for anything else.
#[must_use]
pub fn formatted_operands(value: &FluentValue<'_>) -> Option<crate::plural::PluralOperands> {
    match value {
        FluentValue::Custom(custom) => (**custom)
            .as_any()
            .downcast_ref::<FormattedValue>()
            .and_then(|f| f.plural.as_deref())
            .and_then(crate::plural::PluralOperands::from_decimal),
        _ => None,
    }
}

/// Whether a `NUMBER()` result was formatted with `type: "ordinal"`, so a plural selector picks
/// the ordinal category; `false` for anything else (ADR-3359).
#[must_use]
pub fn formatted_is_ordinal(value: &FluentValue<'_>) -> bool {
    match value {
        FluentValue::Custom(custom) => (**custom)
            .as_any()
            .downcast_ref::<FormattedValue>()
            .is_some_and(|f| f.kind() == crate::plural::PluralKind::Ordinal),
        _ => false,
    }
}

fn named_options<'a>(named: &'a FluentArgs<'_>) -> Result<Vec<(&'a str, OptionValue<'a>)>, FormatError> {
    named
        .iter()
        .map(|(name, v)| match v {
            FluentValue::String(s) => Ok((name, OptionValue::Str(s.as_ref()))),
            FluentValue::Number(n) => Ok((name, OptionValue::Num(n.value))),
            _ => Err(FormatError::option(name, "must be a string or a number")),
        })
        .collect()
}

fn positional_number(value: Option<&FluentValue<'_>>) -> Result<f64, FormatError> {
    match value {
        Some(FluentValue::Number(n)) => Ok(n.value),
        Some(FluentValue::String(s)) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| FormatError::BadValue(format!("{s:?} is not a number"))),
        Some(FluentValue::Custom(c)) => (**c)
            .as_any()
            .downcast_ref::<FormattedValue>()
            .map(|f| f.value)
            .ok_or_else(|| FormatError::BadValue("not a number".to_owned())),
        _ => Err(FormatError::BadValue("NUMBER() needs a number".to_owned())),
    }
}

/// `NUMBER($n, …)` in `locale`.
///
/// # Errors
///
/// A bad option or a value that is not a number.
pub fn fluent_number(
    locale: &str,
    positional: &[FluentValue<'_>],
    named: &FluentArgs<'_>,
) -> Result<FormattedValue, FormatError> {
    let value = positional_number(positional.first())?;
    let (options, _) = NumberOptions::from_options(named_options(named)?)?;
    let text = format_number(value, locale, &options)?;
    Ok(FormattedValue {
        text,
        value,
        plural: operands_text(value, &options),
        kind: options.kind,
    })
}

/// `DATETIME($d, …)` in `locale`: `$d` is unix seconds (a number, shown at `timeZone` or else
/// `default_offset`) or ISO 8601 text ([`CivilDateTime::parse_iso`]; converted only when it carries
/// an offset and `timeZone` is set).
///
/// # Errors
///
/// A bad option or a value that is not a date.
pub fn fluent_datetime(
    locale: &str,
    positional: &[FluentValue<'_>],
    named: &FluentArgs<'_>,
    default_offset: i32,
) -> Result<FormattedValue, FormatError> {
    let (options, _) = DateOptions::from_options(named_options(named)?)?;
    let unix = |seconds: f64| -> Result<CivilDateTime, FormatError> {
        if !seconds.is_finite() || seconds.abs() > 1e15 {
            return Err(FormatError::BadValue(format!("{seconds} is not a unix time")));
        }
        // Finite and below 1e15, so the floor fits i64.
        CivilDateTime::from_unix(
            seconds.floor() as i64,
            options.time_zone.unwrap_or(default_offset),
        )
    };
    let dt = match positional.first() {
        Some(FluentValue::Number(n)) => unix(n.value)?,
        Some(FluentValue::String(s)) => match s.trim().parse::<f64>() {
            Ok(seconds) if s.bytes().all(|b| b.is_ascii_digit() || b == b'-' || b == b'.') => unix(seconds)?,
            _ => CivilDateTime::parse_iso(s)?,
        },
        _ => return Err(FormatError::BadValue("DATETIME() needs a date".to_owned())),
    };
    let text = format_datetime(&dt, locale, &options)?;
    Ok(FormattedValue {
        text,
        value: dt.to_unix() as f64,
        plural: None,
        kind: crate::plural::PluralKind::Cardinal,
    })
}

/// Registers `NUMBER()` and `DATETIME()` for `locale` on `bundle` (called for every bundle
/// [`crate::Localization`] creates). `utc_offset` is the game's current UTC offset in seconds
/// ([`crate::Localization::set_utc_offset_seconds`]), read at format time. A bad option or value
/// makes the function return Fluent's error value, so the placeable shows `NUMBER()` rather than
/// panicking.
///
/// # Errors
///
/// A function of that name is already registered.
pub fn register_functions(
    bundle: &mut FluentBundle<FluentResource>,
    locale: &str,
    utc_offset: Arc<AtomicI32>,
) -> Result<(), FluentError> {
    let number_locale = locale.to_owned();
    bundle.add_function(NUMBER_FUNCTION, move |positional, named| {
        match fluent_number(&number_locale, positional, named) {
            Ok(v) => FluentValue::Custom(Box::new(v)),
            Err(e) => {
                tracing::debug!(target: "regia_ui", "NUMBER() in {number_locale}: {e}");
                FluentValue::Error
            }
        }
    })?;
    let date_locale = locale.to_owned();
    bundle.add_function(DATETIME_FUNCTION, move |positional, named| match fluent_datetime(
        &date_locale,
        positional,
        named,
        utc_offset.load(Ordering::Relaxed),
    ) {
        Ok(v) => FluentValue::Custom(Box::new(v)),
        Err(e) => {
            tracing::debug!(target: "regia_ui", "DATETIME() in {date_locale}: {e}");
            FluentValue::Error
        }
    })
}

#[cfg(test)]
mod tests;
