// SPDX-License-Identifier: MPL-2.0
//! Cardinal plural categories (`docs/specs/content-data.md` § 1.5, ADR-1466).
//!
//! The Fluent runtime picks plural variants with its own bundled CLDR data. The localization tools
//! (validator, gather, agents) need the same answers without building a bundle, so this module holds
//! Regia's own table, written from the Unicode CLDR specification (UTS #35 part 3 § 5.1, "Language
//! Plural Rules": the operands `n i v w f t` and the rule syntax) and the published CLDR rule tables.
//!
//! The table follows the CLDR release the Fluent runtime uses (CLDR 37), so a category the runtime
//! can never select (the `many` that later CLDR releases added for exact millions in French,
//! Spanish, Italian and Portuguese) is not listed. A test compares every rule with the runtime on
//! integers 0..=1000 and on decimals. Two bundled rules disagree with that release and are pinned
//! instead of copied: Arabic `few`/`many` test the integer digits where CLDR tests `n % 100`, and
//! Belarusian `few` tests the whole integer `i` against `2..=4` where CLDR tests `n % 10`.
//!
//! Ordinal rules ("1st, 2nd, 3rd") follow in the same file: [`ordinal_category`] reads the CLDR
//! `ordinals.xml` table (UTS #35 part 3 § 5.2). Fluent's `NUMBER(…, type: "ordinal")` selects
//! through them, and `regia loc plural --type ordinal` prints them.

use std::fmt;

/// A CLDR plural category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PluralCategory {
    /// `zero` (Arabic 0).
    Zero,
    /// `one`.
    One,
    /// `two`.
    Two,
    /// `few`.
    Few,
    /// `many`.
    Many,
    /// `other`: every locale has it, and it is the category of anything no rule matched.
    Other,
}

impl PluralCategory {
    /// Every category, in CLDR order.
    pub const ALL: [PluralCategory; 6] = [
        PluralCategory::Zero,
        PluralCategory::One,
        PluralCategory::Two,
        PluralCategory::Few,
        PluralCategory::Many,
        PluralCategory::Other,
    ];

    /// The category name as Fluent variant keys write it (`"one"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PluralCategory::Zero => "zero",
            PluralCategory::One => "one",
            PluralCategory::Two => "two",
            PluralCategory::Few => "few",
            PluralCategory::Many => "many",
            PluralCategory::Other => "other",
        }
    }

    /// Parses a category name (`"few"`); `None` for anything else.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == name)
    }
}

impl fmt::Display for PluralCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The CLDR plural operands of one number as written (`"1.50"` keeps its trailing zero).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PluralOperands {
    /// `n`: the absolute value.
    pub n: f64,
    /// `i`: the integer digits of `n`.
    pub i: u64,
    /// `v`: the number of visible fraction digits, with trailing zeros.
    pub v: u32,
    /// `w`: the number of visible fraction digits, without trailing zeros.
    pub w: u32,
    /// `f`: the visible fraction digits, with trailing zeros, as an integer.
    pub f: u64,
    /// `t`: the visible fraction digits, without trailing zeros, as an integer.
    pub t: u64,
}

impl PluralOperands {
    /// The operands of a whole number.
    #[must_use]
    pub fn integer(value: u64) -> Self {
        Self {
            n: value as f64,
            i: value,
            v: 0,
            w: 0,
            f: 0,
            t: 0,
        }
    }

    /// The operands of a decimal number as text: an optional sign, digits, and optionally a `.` and
    /// more digits (`"3"`, `"-1.50"`). `None` for anything else (exponents, empty parts, `NaN`).
    #[must_use]
    pub fn from_decimal(text: &str) -> Option<Self> {
        let unsigned = text.strip_prefix('-').unwrap_or(text);
        let (int_part, frac_part) = match unsigned.split_once('.') {
            Some((int_part, frac_part)) => (int_part, frac_part),
            None => (unsigned, ""),
        };
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !digits(int_part) || (unsigned.contains('.') && !digits(frac_part)) {
            return None;
        }
        // Long digit runs would overflow the integer operands; CLDR rules only look at the last
        // few digits of `i`, `f` and `t`, so keep the last 18.
        // Beyond 18 significant integer digits `i` is `10^18 +` the last 18 digits: the rules that
        // compare `i` with 0..2 must not mistake 10^18 + 1 for 1, and `10^18` leaves `i % 10` and
        // `i % 100` unchanged.
        let beyond_tail = int_part.trim_start_matches('0').len() > 18;
        let tail = |s: &str| -> u64 {
            let start = s.len().saturating_sub(18);
            s[start..].parse().unwrap_or(0)
        };
        let trimmed = frac_part.trim_end_matches('0');
        Some(Self {
            n: unsigned.parse().ok()?,
            i: tail(int_part) + if beyond_tail { 1_000_000_000_000_000_000 } else { 0 },
            v: u32::try_from(frac_part.len()).ok()?,
            w: u32::try_from(trimmed.len()).ok()?,
            f: if frac_part.is_empty() { 0 } else { tail(frac_part) },
            t: if trimmed.is_empty() { 0 } else { tail(trimmed) },
        })
    }

    /// The operands of a finite float, written the shortest way Rust prints it (`1.5`, `2`). The
    /// Fluent runtime turns numbers into operands the same way.
    #[must_use]
    pub fn from_f64(value: f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        Self::from_decimal(&value.to_string())
    }

    /// `n` is a whole number in `lo..=hi` (a CLDR range on `n` never matches a fraction). Read from
    /// the integer digits: `n` as a float rounds above 2^53.
    fn n_in(&self, lo: u64, hi: u64) -> bool {
        self.t == 0 && self.i >= lo && self.i <= hi
    }

    /// `n % m` is a whole number in `lo..=hi`. Past 2^53 `n` has lost its last digits, and those are
    /// what the rule reads, so a whole number uses the exact `i` (which keeps them).
    fn n_mod_in(&self, m: u64, lo: u64, hi: u64) -> bool {
        if self.t == 0 {
            return (lo..=hi).contains(&(self.i % m));
        }
        let r = self.n % m as f64;
        r.fract() == 0.0 && r >= lo as f64 && r <= hi as f64
    }

    /// `n` is exactly one of the listed values (`n = 1,3`, `n = 11,8,80,800`). CLDR ordinal
    /// samples are `@integer` only, so a visible fraction (`1.0`) never matches.
    fn n_in_list(&self, list: &[u64]) -> bool {
        self.v == 0 && list.contains(&self.i)
    }

    /// `n % m` equals one of the listed values (`n % 10 = 1,3`), with the exact digits of `i`.
    fn n_mod_in_list(&self, m: u64, list: &[u64]) -> bool {
        if self.t == 0 {
            return list.contains(&(self.i % m));
        }
        let r = self.n % m as f64;
        r.fract() == 0.0 && list.contains(&(r as u64))
    }
}

/// How one locale family picks a category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rules {
    /// `one: i = 1 and v = 0` (en, de, nl, sv, it, pt-PT, ...).
    OneIntegerOne,
    /// `one: n = 1 or t != 0 and i = 0,1` (da).
    Danish,
    /// `one: n = 1` (es, tr).
    OneExactlyOne,
    /// `one: i = 0,1` (fr).
    OneZeroOrOne,
    /// `one: i = 0..1` (pt).
    OneIntegerZeroToOne,
    /// one / few / many by the last digits; decimals are `other` (ru, uk).
    EastSlavic,
    /// one / few / many by `n` (no `v = 0` gate) (be).
    Belarusian,
    /// one / few / many by the last digits, `one` only for 1 itself (pl).
    Polish,
    /// one / few for 2..4 / many for decimals (cs, sk).
    Czech,
    /// zero / one / two / few / many / other (ar).
    Arabic,
    /// one / two / many for whole tens above 10 (he).
    Hebrew,
    /// Every number is `other` (ja, zh, ko, ...).
    OtherOnly,
}

impl Rules {
    fn categories(self) -> &'static [PluralCategory] {
        use PluralCategory::*;
        match self {
            Rules::OneIntegerOne
            | Rules::Danish
            | Rules::OneExactlyOne
            | Rules::OneZeroOrOne
            | Rules::OneIntegerZeroToOne => &[One, Other],
            Rules::EastSlavic | Rules::Belarusian | Rules::Polish | Rules::Czech => &[One, Few, Many, Other],
            Rules::Arabic => &[Zero, One, Two, Few, Many, Other],
            Rules::Hebrew => &[One, Two, Many, Other],
            Rules::OtherOnly => &[Other],
        }
    }

    fn select(self, o: &PluralOperands) -> PluralCategory {
        use PluralCategory::*;
        let (i10, i100) = (o.i % 10, o.i % 100);
        let whole = o.v == 0;
        match self {
            Rules::OneIntegerOne => {
                if o.i == 1 && whole {
                    One
                } else {
                    Other
                }
            }
            Rules::Danish => {
                if o.n == 1.0 || (o.t != 0 && o.i <= 1) {
                    One
                } else {
                    Other
                }
            }
            Rules::OneExactlyOne => {
                if o.n_in(1, 1) {
                    One
                } else {
                    Other
                }
            }
            Rules::OneZeroOrOne | Rules::OneIntegerZeroToOne => {
                if o.i <= 1 {
                    One
                } else {
                    Other
                }
            }
            Rules::EastSlavic => {
                if !whole {
                    Other
                } else if i10 == 1 && i100 != 11 {
                    One
                } else if (2..=4).contains(&i10) && !(12..=14).contains(&i100) {
                    Few
                } else {
                    // i % 10 = 0, i % 10 = 5..9, or i % 100 = 11..14.
                    Many
                }
            }
            Rules::Belarusian => {
                // CLDR reads `n`, including decimals. 22 and 22.0 are both few.
                if o.n_mod_in(10, 1, 1) && !o.n_mod_in(100, 11, 11) {
                    One
                } else if o.n_mod_in(10, 2, 4) && !o.n_mod_in(100, 12, 14) {
                    Few
                } else if o.n_mod_in(10, 0, 0) || o.n_mod_in(10, 5, 9) || o.n_mod_in(100, 11, 14) {
                    Many
                } else {
                    Other
                }
            }
            Rules::Polish => {
                if !whole {
                    Other
                } else if o.i == 1 {
                    One
                } else if (2..=4).contains(&i10) && !(12..=14).contains(&i100) {
                    Few
                } else {
                    // i != 1 and i % 10 = 0..1, i % 10 = 5..9, or i % 100 = 12..14.
                    Many
                }
            }
            Rules::Czech => {
                if !whole {
                    Many
                } else if o.i == 1 {
                    One
                } else if (2..=4).contains(&o.i) {
                    Few
                } else {
                    Other
                }
            }
            Rules::Arabic => {
                if o.n_in(0, 0) {
                    Zero
                } else if o.n_in(1, 1) {
                    One
                } else if o.n_in(2, 2) {
                    Two
                } else if o.n_mod_in(100, 3, 10) {
                    Few
                } else if o.n_mod_in(100, 11, 99) {
                    Many
                } else {
                    Other
                }
            }
            Rules::Hebrew => {
                if o.i == 1 && whole {
                    One
                } else if o.i == 2 && whole {
                    Two
                } else if whole && !o.n_in(0, 10) && o.n_mod_in(10, 0, 0) {
                    Many
                } else {
                    Other
                }
            }
            Rules::OtherOnly => Other,
        }
    }
}

/// The rule family of a locale tag (`"fr-CA"` uses French), `None` for a language not in the table.
fn rules_for(locale: &str) -> Option<Rules> {
    let lower = locale.to_ascii_lowercase().replace('_', "-");
    let language = lower.split('-').next().unwrap_or("");
    let rules = match language {
        "en" | "de" | "nl" | "sv" | "it" | "fi" | "et" | "ca" | "gl" => Rules::OneIntegerOne,
        "da" => Rules::Danish,
        "pt" if lower == "pt-pt" => Rules::OneIntegerOne,
        "pt" => Rules::OneIntegerZeroToOne,
        "es" | "tr" | "el" | "hu" | "bg" | "nb" | "no" | "nn" => Rules::OneExactlyOne,
        "fr" => Rules::OneZeroOrOne,
        "ru" | "uk" => Rules::EastSlavic,
        "be" => Rules::Belarusian,
        "pl" => Rules::Polish,
        "cs" | "sk" => Rules::Czech,
        "ar" => Rules::Arabic,
        "he" | "iw" => Rules::Hebrew,
        "ja" | "zh" | "ko" | "th" | "vi" | "id" | "ms" => Rules::OtherOnly,
        _ => return None,
    };
    Some(rules)
}

/// Which CLDR plural rule family a selection uses (`docs/specs/content-data.md` § 1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PluralKind {
    /// Cardinal rules: "1 item, 2 items" (the default everywhere).
    #[default]
    Cardinal,
    /// Ordinal rules: "1st, 2nd, 3rd".
    Ordinal,
}

/// How one locale family picks an ordinal category. The data is the CLDR `ordinals.xml`
/// supplemental table (UTS #35 part 3 § 5.2, "Ordinal Plural Rules"); every language the
/// cardinal table lists has an entry here, and the ones CLDR gives no ordinal rules
/// (de, nl, ru, ja, zh, ...) are `OtherOnly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrdinalRules {
    /// CLDR lists the language with the single rule `other` (de, nl, da, fi, nb, no, pt,
    /// ru, pl, cs, sk, ar, he, ja, zh, ko, th, id, gl, el, tr, et, ...).
    OtherOnly,
    /// `one: n = 1` (fr, ms, vi, ...).
    OneExactlyOne,
    /// `one: n = 1,5` (hu).
    OneOneFive,
    /// `one: n % 10 = 1,2 and n % 100 != 11,12` (sv).
    Swedish,
    /// `one: n % 10 = 1,3 and n % 100 != 11` (es).
    Spanish,
    /// `one/two/few: n % 10 = 1/2/3 and n % 100 != 11/12/13` (en).
    English,
    /// `few: n % 10 = 2,3 and n % 100 != 12,13` (be).
    Belarusian,
    /// `few: n % 10 = 3 and n % 100 != 13` (uk).
    Ukrainian,
    /// `many: n = 8, 11, 80, 800` (it).
    Italian,
    /// `one: i % 10 = 1 and i % 100 != 11; two: i % 10 = 2 and i % 100 != 12; few: i % 10 =
    /// 3,4 and i % 100 != 13,14` (bg).
    Bulgarian,
    /// `one: n = 1,3; two: n = 2; few: n = 4` (ca).
    Catalan,
}

impl OrdinalRules {
    fn categories(self) -> &'static [PluralCategory] {
        use PluralCategory::*;
        match self {
            OrdinalRules::OtherOnly => &[Other],
            OrdinalRules::OneExactlyOne | OrdinalRules::OneOneFive => &[One, Other],
            OrdinalRules::Swedish | OrdinalRules::Spanish => &[One, Other],
            OrdinalRules::English | OrdinalRules::Bulgarian | OrdinalRules::Catalan => {
                &[One, Two, Few, Other]
            }
            OrdinalRules::Belarusian | OrdinalRules::Ukrainian => &[Few, Other],
            OrdinalRules::Italian => &[Many, Other],
        }
    }

    fn select(self, o: &PluralOperands) -> PluralCategory {
        use PluralCategory::*;
        match self {
            OrdinalRules::OtherOnly => Other,
            OrdinalRules::OneExactlyOne => {
                if o.n_in_list(&[1]) {
                    One
                } else {
                    Other
                }
            }
            OrdinalRules::OneOneFive => {
                if o.n_in_list(&[1, 5]) {
                    One
                } else {
                    Other
                }
            }
            OrdinalRules::Swedish => {
                if o.n_mod_in_list(10, &[1, 2]) && !o.n_mod_in_list(100, &[11, 12]) {
                    One
                } else {
                    Other
                }
            }
            OrdinalRules::Spanish => {
                if o.n_mod_in_list(10, &[1, 3]) && !o.n_mod_in_list(100, &[11]) {
                    One
                } else {
                    Other
                }
            }
            OrdinalRules::English => {
                if o.n_mod_in_list(10, &[1]) && !o.n_mod_in_list(100, &[11]) {
                    One
                } else if o.n_mod_in_list(10, &[2]) && !o.n_mod_in_list(100, &[12]) {
                    Two
                } else if o.n_mod_in_list(10, &[3]) && !o.n_mod_in_list(100, &[13]) {
                    Few
                } else {
                    Other
                }
            }
            OrdinalRules::Belarusian => {
                if o.n_mod_in_list(10, &[2, 3]) && !o.n_mod_in_list(100, &[12, 13]) {
                    Few
                } else {
                    Other
                }
            }
            OrdinalRules::Ukrainian => {
                if o.n_mod_in_list(10, &[3]) && !o.n_mod_in_list(100, &[13]) {
                    Few
                } else {
                    Other
                }
            }
            OrdinalRules::Italian => {
                if o.n_in_list(&[8, 11, 80, 800]) {
                    Many
                } else {
                    Other
                }
            }
            OrdinalRules::Bulgarian => {
                if o.t == 0 && i_mod_in(o, 10, &[1]) && !i_mod_in(o, 100, &[11]) {
                    One
                } else if o.t == 0 && i_mod_in(o, 10, &[2]) && !i_mod_in(o, 100, &[12]) {
                    Two
                } else if o.t == 0 && i_mod_in(o, 10, &[3, 4]) && !i_mod_in(o, 100, &[13, 14]) {
                    Few
                } else {
                    Other
                }
            }
            OrdinalRules::Catalan => {
                if o.n_in_list(&[1, 3]) {
                    One
                } else if o.n_in_list(&[2]) {
                    Two
                } else if o.n_in_list(&[4]) {
                    Few
                } else {
                    Other
                }
            }
        }
    }
}

/// `i % m` is one of `list` (the `i` operand, for rules written on `i` such as Bulgarian's).
fn i_mod_in(o: &PluralOperands, m: u64, list: &[u64]) -> bool {
    list.contains(&(o.i % m))
}

/// The ordinal rule family of a locale tag, `None` for a language not in the table. Every
/// language [`rules_for`] knows has an entry: the ones CLDR lists under a single `other`
/// ordinal rule map to [`OrdinalRules::OtherOnly`].
fn ordinal_rules_for(locale: &str) -> Option<OrdinalRules> {
    let lower = locale.to_ascii_lowercase().replace('_', "-");
    let language = lower.split('-').next().unwrap_or("");
    let rules = match language {
        "en" => OrdinalRules::English,
        "sv" => OrdinalRules::Swedish,
        "es" => OrdinalRules::Spanish,
        "fr" | "ms" | "vi" => OrdinalRules::OneExactlyOne,
        "hu" => OrdinalRules::OneOneFive,
        "be" => OrdinalRules::Belarusian,
        "uk" => OrdinalRules::Ukrainian,
        "it" => OrdinalRules::Italian,
        "bg" => OrdinalRules::Bulgarian,
        "ca" => OrdinalRules::Catalan,
        // CLDR ordinals.xml lists these languages with the single rule `other` (and the
        // cardinal table above already gates on the language being known).
        "de" | "nl" | "nb" | "no" | "fi" | "et" | "gl" | "da" | "pt" | "tr" | "el" | "ru" | "pl" | "cs"
        | "sk" | "ar" | "he" | "iw" | "ja" | "zh" | "ko" | "th" | "id" => OrdinalRules::OtherOnly,
        _ => return None,
    };
    Some(rules)
}

/// Whether the table knows `locale`'s ordinal rules.
#[must_use]
pub fn is_known_ordinal(locale: &str) -> bool {
    ordinal_rules_for(locale).is_some()
}

/// The ordinal categories `locale` uses, in CLDR order (always ending in `other`); `None` when
/// the table does not know the language.
#[must_use]
pub fn ordinal_categories(locale: &str) -> Option<&'static [PluralCategory]> {
    ordinal_rules_for(locale).map(OrdinalRules::categories)
}

/// The ordinal category of `operands` in `locale`; `other` for a language the table does not
/// know. Non-integers are always `other`: every CLDR ordinal rule is written on integer values.
#[must_use]
pub fn ordinal_category(locale: &str, operands: &PluralOperands) -> PluralCategory {
    ordinal_rules_for(locale).map_or(PluralCategory::Other, |rules| rules.select(operands))
}

/// The ordinal category of a number written as text (`"21"`, `"1.5"`); `None` when the text is
/// not a plain decimal number.
#[must_use]
pub fn ordinal_category_of_text(locale: &str, number: &str) -> Option<PluralCategory> {
    PluralOperands::from_decimal(number).map(|o| ordinal_category(locale, &o))
}

/// The categories `locale` uses for `kind`, in CLDR order; `None` when the table does not know
/// the language.
#[must_use]
pub fn categories_of_kind(locale: &str, kind: PluralKind) -> Option<&'static [PluralCategory]> {
    match kind {
        PluralKind::Cardinal => categories(locale),
        PluralKind::Ordinal => ordinal_categories(locale),
    }
}

/// The category of `operands` in `locale` for `kind`; `other` for a language the table does not
/// know.
#[must_use]
pub fn category_of_kind(locale: &str, kind: PluralKind, operands: &PluralOperands) -> PluralCategory {
    match kind {
        PluralKind::Cardinal => category(locale, operands),
        PluralKind::Ordinal => ordinal_category(locale, operands),
    }
}

/// Whether the table knows `locale`'s plural rules.
#[must_use]
pub fn is_known(locale: &str) -> bool {
    rules_for(locale).is_some()
}

/// The categories `locale` uses, in CLDR order (always ending in `other`); `None` when the table does
/// not know the language.
#[must_use]
pub fn categories(locale: &str) -> Option<&'static [PluralCategory]> {
    rules_for(locale).map(Rules::categories)
}

/// The category of `operands` in `locale`; `other` for a language the table does not know.
#[must_use]
pub fn category(locale: &str, operands: &PluralOperands) -> PluralCategory {
    rules_for(locale).map_or(PluralCategory::Other, |rules| rules.select(operands))
}

/// The category of a number written as text (`"21"`, `"1.5"`); `None` when the text is not a plain
/// decimal number.
#[must_use]
pub fn category_of_text(locale: &str, number: &str) -> Option<PluralCategory> {
    PluralOperands::from_decimal(number).map(|o| category(locale, &o))
}

#[cfg(test)]
mod tests {
    use super::PluralCategory::*;
    use super::*;

    fn cat(locale: &str, n: &str) -> PluralCategory {
        category_of_text(locale, n).expect("number")
    }

    #[test]
    fn operands_follow_the_cldr_definitions() {
        let o = PluralOperands::from_decimal("1.50").expect("1.50");
        assert_eq!((o.i, o.v, o.w, o.f, o.t), (1, 2, 1, 50, 5));
        let o = PluralOperands::from_decimal("-12").expect("-12");
        assert_eq!((o.n, o.i, o.v), (12.0, 12, 0));
        assert_eq!(PluralOperands::from_decimal("1e3"), None);
        assert_eq!(PluralOperands::from_decimal("1."), None);
        assert_eq!(PluralOperands::from_decimal(""), None);
        assert_eq!(PluralOperands::from_f64(f64::NAN), None);
        assert_eq!(PluralOperands::from_f64(2.0), Some(PluralOperands::integer(2)));
    }

    #[test]
    fn english_like() {
        assert_eq!(cat("en", "1"), One);
        assert_eq!(cat("en-US", "1.0"), Other);
        assert_eq!(cat("de", "0"), Other);
        assert_eq!(cat("de", "2"), Other);
        assert_eq!(categories("en"), Some(&[One, Other][..]));
    }

    /// Levels audit M1: Danish is `one: n = 1 or t != 0 and i = 0,1`, not the English rule; `1.0` is
    /// the `n = 1` arm and `1.00` keeps it.
    #[test]
    fn danish_has_its_own_rule_for_fractions() {
        for (n, want) in [
            ("0", Other),
            ("1", One),
            ("2", Other),
            ("0.5", One),
            ("1.5", One),
            ("1.0", One),
            ("0.0", Other),
            ("2.5", Other),
        ] {
            assert_eq!(cat("da", n), want, "{n}");
        }
        assert_eq!(categories("da"), Some(&[One, Other][..]));
    }

    /// Levels audit M1: fractions in ru, uk and pl are `other` in CLDR (the `many` arms all start with
    /// `v = 0`), so the audit's `many` was wrong and this pins the published rule.
    #[test]
    fn slavic_fractions_are_other() {
        for locale in ["ru", "uk", "pl"] {
            assert_eq!(cat(locale, "1.5"), Other, "{locale}");
            assert_eq!(cat(locale, "21.5"), Other, "{locale}");
        }
    }

    /// Levels audit M1: `n % 100` read a rounded float: 2^53 + ... loses its last digit, and that is
    /// what Arabic's categories look at.
    #[test]
    fn arabic_reads_the_exact_digits_of_a_huge_integer() {
        assert_eq!(
            cat("ar", "9007199254741099"),
            Many,
            "n % 100 = 99, the float rounds to ..100"
        );
        assert_eq!(cat("ar", "103"), Few);
    }

    #[test]
    fn french_and_spanish_differ_on_zero_and_decimals() {
        assert_eq!(cat("fr", "0"), One);
        assert_eq!(cat("fr-CA", "1.5"), One);
        assert_eq!(cat("fr", "2"), Other);
        assert_eq!(cat("es", "0"), Other);
        assert_eq!(cat("es", "1"), One);
        assert_eq!(cat("es", "1.0"), One);
        assert_eq!(cat("es", "1.5"), Other);
    }

    #[test]
    fn russian_and_polish() {
        for (n, want) in [
            ("1", One),
            ("21", One),
            ("11", Many),
            ("2", Few),
            ("24", Few),
            ("12", Many),
        ] {
            assert_eq!(cat("ru", n), want, "ru {n}");
        }
        assert_eq!(cat("ru", "5"), Many);
        assert_eq!(cat("ru", "1.5"), Other);
        for (n, want) in [
            ("1", One),
            ("21", Many),
            ("22", Few),
            ("12", Many),
            ("5", Many),
            ("0", Many),
        ] {
            assert_eq!(cat("pl", n), want, "pl {n}");
        }
        assert_eq!(cat("pl", "0.5"), Other);
    }

    #[test]
    fn arabic_has_six_categories() {
        for (n, want) in [
            ("0", Zero),
            ("1", One),
            ("2", Two),
            ("3", Few),
            ("10", Few),
            ("11", Many),
            ("99", Many),
            ("100", Other),
            ("103", Few),
            ("0.5", Other),
        ] {
            assert_eq!(cat("ar", n), want, "ar {n}");
        }
        assert_eq!(categories("ar").map(<[_]>::len), Some(6));
    }

    #[test]
    fn japanese_and_unknown() {
        assert_eq!(cat("ja", "1"), Other);
        assert_eq!(categories("ja"), Some(&[Other][..]));
        assert!(!is_known("tlh"));
        assert_eq!(categories("tlh"), None);
        assert_eq!(cat("tlh", "1"), Other);
    }

    /// CLDR ordinals.xml: every language the cardinal table knows has ordinal data; sampled
    /// categories for en, fr, it, sv and a default-other locale pin the table.
    #[test]
    fn ordinals_follow_the_cldr_table() {
        let ord = |locale: &str, n: &str| ordinal_category_of_text(locale, n).expect("number");
        // en: one/two/few by the last digit, 11/12/13 excluded.
        for (n, want) in [
            ("1", One),
            ("2", Two),
            ("3", Few),
            ("4", Other),
            ("11", Other),
            ("12", Other),
            ("13", Other),
            ("21", One),
            ("22", Two),
            ("23", Few),
            ("101", One),
        ] {
            assert_eq!(ord("en", n), want, "en {n}");
        }
        assert_eq!(
            categories_of_kind("en", PluralKind::Ordinal),
            Some(&[One, Two, Few, Other][..])
        );
        // fr: one is exactly 1.
        assert_eq!(ord("fr", "1"), One);
        assert_eq!(ord("fr", "2"), Other);
        // it: many is 8, 11, 80, 800.
        for (n, want) in [
            ("8", Many),
            ("11", Many),
            ("80", Many),
            ("800", Many),
            ("1", Other),
            ("7", Other),
        ] {
            assert_eq!(ord("it", n), want, "it {n}");
        }
        // sv: one is n % 10 = 1,2 except 11, 12.
        for (n, want) in [
            ("1", One),
            ("2", One),
            ("3", Other),
            ("11", Other),
            ("12", Other),
            ("21", One),
            ("22", One),
        ] {
            assert_eq!(ord("sv", n), want, "sv {n}");
        }
        // de is in CLDR's single-`other` list: never one.
        for n in ["0", "1", "2", "3", "21"] {
            assert_eq!(ord("de", n), Other, "de {n}");
        }
        assert_eq!(ordinal_categories("de"), Some(&[Other][..]));
        // Non-integers never match an ordinal rule.
        assert_eq!(ord("en", "1.5"), Other);
        assert_eq!(ord("fr", "1.0"), Other);
        // Unknown languages are `other`, and the table says so.
        assert!(!is_known_ordinal("tlh"));
        assert_eq!(ord("tlh", "1"), Other);
        // es and ca pin the 2- and 3-rule families (CLDR's es samples list 13 as `one`).
        assert_eq!(ord("es", "1"), One);
        assert_eq!(ord("es", "3"), One);
        assert_eq!(ord("es", "13"), One);
        assert_eq!(ord("ca", "2"), Two);
        assert_eq!(ord("ca", "4"), Few);
        assert_eq!(ord("bg", "4"), Few);
        assert_eq!(ord("hu", "5"), One);
        assert_eq!(ord("uk", "3"), Few);
        assert_eq!(ord("be", "22"), Few);
        // Cardinals: nb, no and nn use n = 1, so 1.0 is one. be has no v = 0 gate.
        assert_eq!(category_of_text("nb", "1.0"), Some(One));
        assert_eq!(category_of_text("no", "1.0"), Some(One));
        assert_eq!(category_of_text("nn", "1"), Some(One));
        assert_eq!(category_of_text("nn", "1.0"), Some(One));
        assert_eq!(category_of_text("be", "22"), Some(Few));
        assert_eq!(category_of_text("be", "13"), Some(Many));
        assert_eq!(category_of_text("be", "1.0"), Some(One));
        assert_eq!(category_of_text("ru", "1.0"), Some(Other));
        // Cardinals are untouched by the ordinal table.
        assert_eq!(
            category_of_kind("en", PluralKind::Cardinal, &PluralOperands::integer(2)),
            Other
        );
    }

    #[test]
    fn names_round_trip() {
        for c in PluralCategory::ALL {
            assert_eq!(PluralCategory::parse(c.as_str()), Some(c));
        }
        assert_eq!(PluralCategory::parse("several"), None);
    }
}
