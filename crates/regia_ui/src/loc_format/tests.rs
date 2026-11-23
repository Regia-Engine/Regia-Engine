// SPDX-License-Identifier: MPL-2.0
//! Table-driven tests of `loc_format` (ADR-2698). Expected strings come from the CLDR charts the
//! data was typed from; `\u{a0}` is a no-break space and `\u{202f}` a narrow no-break space.

use super::*;

fn opts(pairs: &[(&str, OptionValue<'_>)]) -> NumberOptions {
    NumberOptions::from_options(pairs.iter().copied())
        .expect("valid options")
        .0
}

fn s(v: &str) -> OptionValue<'_> {
    OptionValue::Str(v)
}

fn n(v: f64) -> OptionValue<'static> {
    OptionValue::Num(v)
}

fn num(locale: &str, value: f64, pairs: &[(&str, OptionValue<'_>)]) -> String {
    format_number(value, locale, &opts(pairs)).expect("formats")
}

#[test]
fn table_is_sorted_and_parents_exist() {
    let tags = data_locales();
    assert!(
        tags.windows(2).all(|w| w[0] < w[1]),
        "LOCALES must stay sorted for binary search"
    );
    for r in data::LOCALES {
        if let Some(parent) = r.parent {
            assert!(
                has_locale_data(parent),
                "{} has a parent {parent} without a row",
                r.tag
            );
        }
        for (code, _) in r.currencies {
            assert!(
                data::CURRENCIES.iter().any(|c| c.code == *code),
                "{}: unknown currency {code}",
                r.tag
            );
        }
    }
    let codes: Vec<&str> = data::CURRENCIES.iter().map(|c| c.code).collect();
    assert!(codes.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn shipped_pseudo_and_game_locales_have_data() {
    // The engine's shipped locales, its debug locales and the most used game locales.
    for tag in [
        "en", "ar", "zh", "en-XA", "en-XD", "en-US", "en-GB", "fr-FR", "fr-CA", "de-DE", "es-ES", "es-MX",
        "it-IT", "pt-BR", "pt-PT", "ru-RU", "pl-PL", "ja-JP", "ko-KR", "zh-CN", "zh-TW", "tr-TR", "ar-SA",
        "th-TH", "vi-VN", "id-ID", "nl-NL", "sv-SE", "da-DK", "fi-FI", "nb-NO", "cs-CZ", "hu-HU", "uk-UA",
        "el-GR", "ro-RO", "hi-IN",
    ] {
        let chain = locale_chain(tag);
        assert!(
            chain.iter().any(|t| t != "root" && has_locale_data(t)),
            "{tag} resolves only to root: {chain:?}"
        );
    }
    // The pseudo locale formats exactly as English.
    let o = NumberOptions::default();
    assert_eq!(
        format_number(1234.5, "en-XA", &o),
        format_number(1234.5, "en", &o)
    );
}

#[test]
fn chains_fall_back_to_parent_then_root() {
    let cases: &[(&str, &[&str])] = &[
        ("pt-BR", &["pt-BR", "pt", "root"]),
        ("pt_br", &["pt-BR", "pt", "root"]),
        ("zh-TW", &["zh-TW", "zh-Hant", "root"]),
        ("zh-hant-tw", &["zh-Hant-TW", "zh-Hant", "root"]),
        ("zh-CN", &["zh-CN", "zh", "root"]),
        ("es-MX", &["es-MX", "es-419", "es", "root"]),
        ("en-GB", &["en-GB", "en-001", "en", "root"]),
        ("no", &["no", "nb", "root"]),
        ("en-US-u-nu-arab", &["en-US", "en", "root"]),
        ("", &["root"]),
        ("und", &["root"]),
        ("xx-YY", &["xx-YY", "xx", "root"]),
    ];
    for (tag, want) in cases {
        assert_eq!(locale_chain(tag), *want, "{tag}");
    }
    // pt-BR has no row of its own (CLDR's default content): every field comes from pt.
    assert!(!has_locale_data("pt-BR"));
    let brl = opts(&[("style", s("currency")), ("currency", s("BRL"))]);
    assert_eq!(
        format_number(1234.5, "pt-BR", &brl).as_deref(),
        Ok("R$\u{a0}1.234,50")
    );
    // A field pt lacks (none) or a locale without relative data reaches root.
    let rel = format_relative(
        3.0,
        RelativeUnit::Day,
        "nl-NL",
        RelativeNumeric::Always,
        &NumberOptions::default(),
    );
    assert_eq!(rel.as_deref(), Ok("+3 d"));
    assert_eq!(num("xx-YY", 1234.5, &[]), "1,234.5");
}

#[test]
fn decimal_separators_and_grouping() {
    let cases: &[(&str, f64, &str)] = &[
        ("en-US", 1234.5, "1,234.5"),
        ("de-DE", 1234.5, "1.234,5"),
        ("fr-FR", 1234.5, "1\u{202f}234,5"),
        ("fr-CA", 1234.5, "1\u{a0}234,5"),
        ("hi-IN", 1234567.0, "12,34,567"),
        ("en-IN", 1234567.891, "12,34,567.891"),
        ("hi-IN", 123.0, "123"),
        ("hi-IN", 1234.0, "1,234"),
        ("es-ES", 1234.0, "1234"),
        ("es-ES", 12345.0, "12.345"),
        ("es-MX", 1234.0, "1,234"),
        ("pl-PL", 1234.0, "1234"),
        ("pl-PL", 12345.0, "12\u{a0}345"),
        ("pt-PT", 1234.0, "1234"),
        ("ru-RU", 1234.5, "1\u{a0}234,5"),
        ("ja-JP", 1234.5, "1,234.5"),
        ("tr-TR", 1234.5, "1.234,5"),
        ("ar", 1234.5, "1,234.5"),
        ("en-US", -1234.5, "-1,234.5"),
        ("sv-SE", -5.0, "\u{2212}5"),
        ("de-DE", -0.5, "-0,5"),
        ("en-US", 0.0, "0"),
        ("en-US", 1.23456, "1.235"),
        ("en-US", 1e21, "1,000,000,000,000,000,000,000"),
        ("en-US", 0.000_000_1, "0"),
    ];
    for (locale, value, want) in cases {
        assert_eq!(num(locale, *value, &[]), *want, "{locale} {value}");
    }
}

#[test]
fn huge_tiny_nan_and_infinity() {
    let big = num("en-US", 1e300, &[]);
    assert_eq!(big.chars().filter(char::is_ascii_digit).count(), 301);
    assert!(big.starts_with("1,000,000"));
    let max = num("en-US", f64::MAX, &[]);
    assert!(max.starts_with("179,769,313,486,231,570,000,"), "{max}");
    assert_eq!(num("en-US", f64::MIN_POSITIVE, &[]), "0");
    let tiny = num("en-US", 5e-324, &[("maximumSignificantDigits", n(2.0))]);
    assert_eq!(tiny.len(), 2 + 324, "{tiny}");
    assert!(tiny.starts_with("0.000") && tiny.ends_with("05"), "{tiny}");
    assert_eq!(num("en-US", f64::NAN, &[]), "NaN");
    assert_eq!(num("ru-RU", f64::NAN, &[]), "не\u{a0}число");
    assert_eq!(num("en-US", f64::INFINITY, &[]), "∞");
    assert_eq!(num("en-US", f64::NEG_INFINITY, &[]), "-∞");
    assert_eq!(num("en-US", f64::NAN, &[("style", s("percent"))]), "NaN%");
    assert_eq!(
        num(
            "de-DE",
            f64::INFINITY,
            &[("style", s("currency")), ("currency", s("EUR"))]
        ),
        "∞\u{a0}€"
    );
    // Negative zero follows ECMA-402: `auto` shows it, `negative` does not.
    assert_eq!(num("en-US", -0.0, &[]), "-0");
    assert_eq!(num("en-US", -0.0001, &[]), "-0");
    assert_eq!(num("en-US", -0.0001, &[("signDisplay", s("negative"))]), "0");
}

#[test]
fn rounding_modes() {
    let cases: &[(&str, f64, &str)] = &[
        ("halfExpand", 2.5, "3"),
        ("halfExpand", -2.5, "-3"),
        ("halfExpand", 2.4, "2"),
        ("halfEven", 2.5, "2"),
        ("halfEven", 3.5, "4"),
        ("halfEven", -2.5, "-2"),
        ("halfEven", 2.51, "3"),
        ("halfTrunc", 2.5, "2"),
        ("halfTrunc", -2.5, "-2"),
        ("halfTrunc", 2.6, "3"),
        ("halfCeil", 2.5, "3"),
        ("halfCeil", -2.5, "-2"),
        ("halfFloor", 2.5, "2"),
        ("halfFloor", -2.5, "-3"),
        ("ceil", 2.1, "3"),
        ("ceil", -2.9, "-2"),
        ("floor", 2.9, "2"),
        ("floor", -2.1, "-3"),
        ("expand", 2.1, "3"),
        ("expand", -2.1, "-3"),
        ("trunc", 2.9, "2"),
        ("trunc", -2.9, "-2"),
        ("halfExpand", 999.9, "1,000"),
        ("ceil", 0.0001, "1"),
        ("floor", -0.0001, "-1"),
        ("trunc", -0.0001, "-0"),
    ];
    for (mode, value, want) in cases {
        let got = num(
            "en-US",
            *value,
            &[("maximumFractionDigits", n(0.0)), ("roundingMode", s(mode))],
        );
        assert_eq!(got, *want, "{mode} {value}");
    }
    // The rounded digits are the shortest decimal of the f64: 1.005 is "1.005", so it rounds up.
    assert_eq!(num("en-US", 1.005, &[("maximumFractionDigits", n(2.0))]), "1.01");
    assert_eq!(
        num(
            "en-US",
            0.125,
            &[("maximumFractionDigits", n(2.0)), ("roundingMode", s("halfEven"))]
        ),
        "0.12"
    );
    assert_eq!(num("en-US", 9.995, &[("maximumFractionDigits", n(2.0))]), "10");
    assert_eq!(num("en-US", 0.0005, &[]), "0.001");
    assert_eq!(num("en-US", 0.00049, &[]), "0");
    assert_eq!(RoundingMode::default(), RoundingMode::HalfExpand);
}

#[test]
fn digit_options() {
    let cases: &[(f64, &[(&str, OptionValue<'_>)], &str)] = &[
        (
            1234.5,
            &[("minimumFractionDigits", OptionValue::Num(2.0))],
            "1,234.50",
        ),
        (1.0, &[("minimumFractionDigits", OptionValue::Num(2.0))], "1.00"),
        (
            1.23456,
            &[("minimumFractionDigits", OptionValue::Num(5.0))],
            "1.23456",
        ),
        (
            1.234567,
            &[
                ("minimumFractionDigits", OptionValue::Num(1.0)),
                ("maximumFractionDigits", OptionValue::Num(2.0)),
            ],
            "1.23",
        ),
        (5.0, &[("minimumIntegerDigits", OptionValue::Num(3.0))], "005"),
        (
            123_456.0,
            &[("maximumSignificantDigits", OptionValue::Num(2.0))],
            "120,000",
        ),
        (
            0.001_234_56,
            &[("maximumSignificantDigits", OptionValue::Num(3.0))],
            "0.00123",
        ),
        (
            1.5,
            &[("minimumSignificantDigits", OptionValue::Num(4.0))],
            "1.500",
        ),
        (
            0.0,
            &[("minimumSignificantDigits", OptionValue::Num(3.0))],
            "0.00",
        ),
        (1234.0, &[("useGrouping", OptionValue::Str("false"))], "1234"),
        (1234.0, &[("useGrouping", OptionValue::Str("min2"))], "1234"),
        (12345.0, &[("useGrouping", OptionValue::Str("min2"))], "12,345"),
        (5.0, &[("signDisplay", OptionValue::Str("always"))], "+5"),
        (0.0, &[("signDisplay", OptionValue::Str("always"))], "+0"),
        (0.0, &[("signDisplay", OptionValue::Str("exceptZero"))], "0"),
        (-5.0, &[("signDisplay", OptionValue::Str("never"))], "5"),
        (-5.0, &[("signDisplay", OptionValue::Str("exceptZero"))], "-5"),
    ];
    for (value, pairs, want) in cases {
        assert_eq!(num("en-US", *value, pairs), *want, "{value} {pairs:?}");
    }
    assert_eq!(num("es-ES", 1234.0, &[("useGrouping", s("always"))]), "1.234");
}

#[test]
fn percent() {
    let p = [("style", s("percent"))];
    let cases: &[(&str, f64, &str)] = &[
        ("en-US", 0.256, "26%"),
        ("en-US", -0.25, "-25%"),
        ("de-DE", 0.25, "25\u{a0}%"),
        ("fr-FR", 0.25, "25\u{202f}%"),
        ("tr-TR", 0.25, "%25"),
        ("en-US", 12.5, "1,250%"),
    ];
    for (locale, value, want) in cases {
        assert_eq!(num(locale, *value, &p), *want, "{locale} {value}");
    }
    assert_eq!(
        num(
            "en-US",
            0.2567,
            &[("style", s("percent")), ("maximumFractionDigits", n(1.0))]
        ),
        "25.7%"
    );
}

#[test]
fn currency_placement_and_symbols() {
    let cases: &[(&str, &str, &str, f64, &str)] = &[
        ("en-US", "USD", "symbol", 1234.5, "$1,234.50"),
        ("de-DE", "EUR", "symbol", 1234.5, "1.234,50\u{a0}€"),
        ("fr-FR", "EUR", "symbol", 1234.5, "1\u{202f}234,50\u{a0}€"),
        ("ja-JP", "JPY", "symbol", 1234.5, "￥1,235"),
        ("ja-JP", "USD", "symbol", 1234.5, "$1,234.50"),
        ("en-US", "EUR", "symbol", 1234.5, "€1,234.50"),
        ("en-US", "JPY", "symbol", 1234.5, "¥1,235"),
        ("en-US", "USD", "code", 1234.5, "USD\u{a0}1,234.50"),
        ("en-US", "CHF", "symbol", 1234.5, "CHF\u{a0}1,234.50"),
        ("de-DE", "USD", "code", 1234.5, "1.234,50\u{a0}USD"),
        ("fr-FR", "USD", "symbol", 1234.5, "1\u{202f}234,50\u{a0}$US"),
        ("fr-FR", "USD", "narrowSymbol", 1234.5, "1\u{202f}234,50\u{a0}$"),
        ("zh-CN", "USD", "symbol", 1234.5, "US$1,234.50"),
        ("zh-CN", "CNY", "symbol", 1234.5, "¥1,234.50"),
        ("zh-TW", "TWD", "symbol", 1234.5, "$1,234.50"),
        ("ko-KR", "KRW", "symbol", 1234.5, "₩1,235"),
        ("en-US", "KWD", "symbol", 1.5, "KWD\u{a0}1.500"),
        ("en-US", "USD", "symbol", -5.0, "-$5.00"),
        ("de-DE", "EUR", "symbol", -5.0, "-5,00\u{a0}€"),
        ("nl-NL", "EUR", "symbol", -5.0, "€\u{a0}-5,00"),
        ("nl-NL", "EUR", "symbol", 5.0, "€\u{a0}5,00"),
        ("pt-BR", "BRL", "symbol", 1234.5, "R$\u{a0}1.234,50"),
        ("en-CA", "CAD", "symbol", 3.0, "$3.00"),
        ("en-US", "CAD", "symbol", 3.0, "CA$3.00"),
        ("en-US", "xyz", "symbol", 1234.5, "XYZ\u{a0}1,234.50"),
        ("tr-TR", "TRY", "symbol", 1234.5, "₺1.234,50"),
    ];
    for (locale, code, display, value, want) in cases {
        let got = num(
            locale,
            *value,
            &[
                ("style", s("currency")),
                ("currency", s(code)),
                ("currencyDisplay", s(display)),
            ],
        );
        assert_eq!(got, *want, "{locale} {code} {display}");
    }
    let missing = format_number(1.0, "en", &opts(&[("style", s("currency"))]));
    assert_eq!(missing.map_err(|e| e.code()), Err("format_bad_option"));
}

#[test]
fn native_digits_are_opt_in() {
    let native = [("numberingSystem", s("native"))];
    assert_eq!(num("ar", 1234.5, &[]), "1,234.5");
    assert_eq!(num("ar", 1234.5, &native), "١٬٢٣٤٫٥");
    assert_eq!(num("ar-SA", 1234.5, &[("numberingSystem", s("arab"))]), "١٬٢٣٤٫٥");
    assert_eq!(num("hi-IN", 1234567.0, &native), "१२,३४,५६७");
    assert_eq!(num("th-TH", 42.0, &native), "๔๒");
    assert_eq!(native_numbering_system("ar-EG"), Some("arab"));
    assert_eq!(native_numbering_system("en"), None);
    // A locale without native digits keeps Latin ones.
    assert_eq!(num("en-US", 1234.5, &native), "1,234.5");
    let dt = CivilDateTime::date(2026, 11, 5).expect("date");
    let o = DateOptions {
        date_style: Some(DateStyle::Long),
        numbering: NumberingSystem::Native,
        ..DateOptions::default()
    };
    assert_eq!(format_datetime(&dt, "ar", &o).as_deref(), Ok("٥ نوفمبر ٢٠٢٦"));
}

#[test]
fn bad_number_options_are_errors() {
    let bad: &[(&str, OptionValue<'_>)] = &[
        ("style", s("bogus")),
        ("currency", s("EURO")),
        ("currency", n(5.0)),
        ("currencyDisplay", s("name")),
        ("useGrouping", s("maybe")),
        ("minimumFractionDigits", s("lots")),
        ("minimumFractionDigits", n(1.5)),
        ("maximumFractionDigits", n(101.0)),
        ("maximumFractionDigits", n(-1.0)),
        ("minimumSignificantDigits", n(0.0)),
        ("maximumSignificantDigits", n(22.0)),
        ("minimumIntegerDigits", n(0.0)),
        ("roundingMode", s("bankers")),
        ("signDisplay", s("sometimes")),
        ("numberingSystem", s("roman")),
    ];
    for (name, value) in bad {
        let err = NumberOptions::from_options([(*name, *value)]).expect_err(name);
        assert_eq!(err.code(), "format_bad_option", "{name}");
    }
    let both = opts(&[
        ("minimumFractionDigits", n(3.0)),
        ("maximumFractionDigits", n(1.0)),
    ]);
    assert_eq!(
        format_number(1.0, "en", &both).map_err(|e| e.code()),
        Err("format_bad_option")
    );
    let sig = opts(&[
        ("minimumSignificantDigits", n(5.0)),
        ("maximumSignificantDigits", n(2.0)),
    ]);
    assert_eq!(
        format_number(1.0, "en", &sig).map_err(|e| e.code()),
        Err("format_bad_option")
    );
    // Names Fluent does not know are ignored and reported; `type` is known since ADR-3359.
    let (opts, ignored) =
        NumberOptions::from_options([("type", s("ordinal")), ("notation", s("compact"))]).expect("ok");
    assert_eq!(opts.kind, crate::plural::PluralKind::Ordinal);
    assert_eq!(ignored, vec!["notation".to_owned()]);
    let (opts, _) = NumberOptions::from_options([("type", s("cardinal"))]).expect("ok");
    assert_eq!(opts.kind, crate::plural::PluralKind::Cardinal);
    assert_eq!(
        NumberOptions::from_options([("type", s("fraction"))]).map_err(|e| e.code()),
        Err("format_bad_option")
    );
}

fn nov5(hour: u8, minute: u8, second: u8, offset: Option<i32>) -> CivilDateTime {
    CivilDateTime {
        year: 2026,
        month: 11,
        day: 5,
        hour,
        minute,
        second,
        offset_seconds: offset,
    }
}

#[test]
fn date_styles() {
    let dt = nov5(15, 45, 10, None);
    let cases: &[(&str, DateStyle, &str)] = &[
        ("ja-JP", DateStyle::Long, "2026年11月5日"),
        ("ja-JP", DateStyle::Full, "2026年11月5日木曜日"),
        ("ja-JP", DateStyle::Short, "2026/11/05"),
        ("en-US", DateStyle::Short, "11/5/26"),
        ("en-US", DateStyle::Medium, "Nov 5, 2026"),
        ("en-US", DateStyle::Long, "November 5, 2026"),
        ("en-US", DateStyle::Full, "Thursday, November 5, 2026"),
        ("en-GB", DateStyle::Short, "05/11/2026"),
        ("en-GB", DateStyle::Long, "5 November 2026"),
        ("de-DE", DateStyle::Short, "05.11.26"),
        ("de-DE", DateStyle::Long, "5. November 2026"),
        ("de-DE", DateStyle::Full, "Donnerstag, 5. November 2026"),
        ("fr-FR", DateStyle::Long, "5 novembre 2026"),
        ("fr-FR", DateStyle::Full, "jeudi 5 novembre 2026"),
        ("es-ES", DateStyle::Long, "5 de noviembre de 2026"),
        ("pt-BR", DateStyle::Long, "5 de novembro de 2026"),
        ("ru-RU", DateStyle::Long, "5 ноября 2026 г."),
        ("pl-PL", DateStyle::Long, "5 listopada 2026"),
        ("zh-CN", DateStyle::Long, "2026年11月5日"),
        ("ko-KR", DateStyle::Long, "2026년 11월 5일"),
        ("fi-FI", DateStyle::Long, "5. marraskuuta 2026"),
        ("tr-TR", DateStyle::Full, "5 Kasım 2026 Perşembe"),
        ("da-DK", DateStyle::Full, "torsdag den 5. november 2026"),
        ("hu-HU", DateStyle::Long, "2026. november 5."),
        ("xx", DateStyle::Medium, "2026 M11 5"),
    ];
    for (locale, style, want) in cases {
        assert_eq!(
            format_date(&dt, locale, *style).as_deref(),
            Ok(*want),
            "{locale} {style:?}"
        );
    }
}

#[test]
fn time_styles_and_clocks() {
    let pm = nov5(15, 45, 10, None);
    let midnight = nov5(0, 5, 0, None);
    let cases: &[(&CivilDateTime, &str, DateStyle, Option<bool>, &str)] = &[
        (&pm, "en-US", DateStyle::Short, None, "3:45\u{202f}PM"),
        (&pm, "en-US", DateStyle::Short, Some(false), "15:45"),
        (&pm, "en-US", DateStyle::Medium, None, "3:45:10\u{202f}PM"),
        (&pm, "en-GB", DateStyle::Short, None, "15:45"),
        (&pm, "de-DE", DateStyle::Short, None, "15:45"),
        (&pm, "de-DE", DateStyle::Short, Some(true), "3:45\u{202f}PM"),
        (&pm, "ja-JP", DateStyle::Short, None, "15:45"),
        (&pm, "ja-JP", DateStyle::Short, Some(true), "午後3:45"),
        (&pm, "ko-KR", DateStyle::Short, None, "오후 3:45"),
        (&pm, "fi-FI", DateStyle::Short, None, "15.45"),
        (&pm, "fi-FI", DateStyle::Medium, None, "15.45.10"),
        (&pm, "es-ES", DateStyle::Short, None, "15:45"),
        (&midnight, "en-US", DateStyle::Short, None, "12:05\u{202f}AM"),
        (&midnight, "ja-JP", DateStyle::Short, Some(true), "午前0:05"),
        (&midnight, "es-ES", DateStyle::Short, None, "0:05"),
    ];
    for (dt, locale, style, h12, want) in cases {
        assert_eq!(
            format_time(dt, locale, *style, *h12).as_deref(),
            Ok(*want),
            "{locale} {style:?} {h12:?}"
        );
    }
    let cycle = |c: HourCycle| DateOptions {
        time_style: Some(DateStyle::Short),
        hour_cycle: Some(c),
        ..DateOptions::default()
    };
    assert_eq!(
        format_datetime(&midnight, "de", &cycle(HourCycle::H24)).as_deref(),
        Ok("24:05")
    );
    assert_eq!(
        format_datetime(&midnight, "ja", &cycle(HourCycle::H12)).as_deref(),
        Ok("午前12:05")
    );
    assert_eq!(
        format_datetime(&midnight, "en", &cycle(HourCycle::H11)).as_deref(),
        Ok("0:05\u{202f}AM")
    );
    // hour12 overrides hourCycle (ECMA-402).
    let both = DateOptions {
        hour12: Some(false),
        ..cycle(HourCycle::H11)
    };
    assert_eq!(format_datetime(&midnight, "en", &both).as_deref(), Ok("00:05"));
    // Long times show the UTC offset when it is known.
    let plus1 = nov5(15, 45, 10, Some(3600));
    assert_eq!(
        format_time(&plus1, "en-US", DateStyle::Long, None).as_deref(),
        Ok("3:45:10\u{202f}PM GMT+1")
    );
    assert_eq!(
        format_time(&nov5(15, 45, 10, Some(19_800)), "en", DateStyle::Long, None).as_deref(),
        Ok("3:45:10\u{202f}PM GMT+5:30")
    );
    assert_eq!(
        format_time(&nov5(15, 45, 10, Some(0)), "fr-FR", DateStyle::Long, None).as_deref(),
        Ok("15:45:10 UTC")
    );
    assert_eq!(
        format_time(&pm, "en-US", DateStyle::Long, None).as_deref(),
        Ok("3:45:10\u{202f}PM")
    );
    let both = DateOptions {
        date_style: Some(DateStyle::Medium),
        time_style: Some(DateStyle::Short),
        ..DateOptions::default()
    };
    assert_eq!(
        format_datetime(&pm, "en-US", &both).as_deref(),
        Ok("Nov 5, 2026, 3:45\u{202f}PM")
    );
    assert_eq!(
        format_datetime(&pm, "ja", &both).as_deref(),
        Ok("2026/11/05 15:45")
    );
}

#[test]
fn every_data_pattern_expands() {
    let dt = nov5(9, 5, 7, Some(-18_000));
    for tag in data_locales() {
        for style in [
            DateStyle::Short,
            DateStyle::Medium,
            DateStyle::Long,
            DateStyle::Full,
        ] {
            for cycle in [HourCycle::H11, HourCycle::H12, HourCycle::H23, HourCycle::H24] {
                let o = DateOptions {
                    date_style: Some(style),
                    time_style: Some(style),
                    hour_cycle: Some(cycle),
                    numbering: NumberingSystem::Native,
                    ..DateOptions::default()
                };
                let text = format_datetime(&dt, tag, &o).unwrap_or_else(|e| panic!("{tag} {style:?}: {e}"));
                assert!(!text.is_empty());
            }
        }
    }
}

#[test]
fn unix_seconds_and_offsets() {
    let epoch = CivilDateTime::from_unix(0, 0).expect("epoch");
    assert_eq!(
        (epoch.year, epoch.month, epoch.day, epoch.weekday()),
        (1970, 1, 1, 4)
    );
    let billion = CivilDateTime::from_unix(1_000_000_000, 0).expect("t");
    assert_eq!(billion.to_iso(), "2001-09-09T01:46:40Z");
    assert_eq!(
        CivilDateTime::from_unix(-1, 0).expect("t").to_iso(),
        "1969-12-31T23:59:59Z"
    );
    assert_eq!(
        CivilDateTime::from_unix(0, -18_000).expect("t").to_iso(),
        "1969-12-31T19:00:00-05:00"
    );
    assert_eq!(
        CivilDateTime::from_unix(0, 19_800).expect("t").to_iso(),
        "1970-01-01T05:30:00+05:30"
    );
    assert!(CivilDateTime::from_unix(0, 19 * 3600).is_err());
    for t in [
        -62_135_596_800_i64,
        -1,
        0,
        951_782_400,
        1_762_300_000,
        4_102_444_800,
    ] {
        for off in [-43_200, 0, 3600, 20_700] {
            let dt = CivilDateTime::from_unix(t, off).expect("t");
            assert_eq!(dt.to_unix(), t, "{t} {off}");
            assert_eq!(CivilDateTime::parse_iso(&dt.to_iso()), Ok(dt));
        }
    }
    // Leap years.
    assert!(CivilDateTime::date(2024, 2, 29).is_ok());
    assert!(CivilDateTime::date(2000, 2, 29).is_ok());
    assert!(CivilDateTime::date(1900, 2, 29).is_err());
    assert_eq!(
        CivilDateTime::date(2026, 2, 29).map_err(|e| e.code()),
        Err("format_bad_value")
    );
    // The game's offset converts an instant; a floating time stays.
    let utc = CivilDateTime::parse_iso("2026-11-05T14:30:00Z").expect("iso");
    let tokyo = DateOptions {
        time_style: Some(DateStyle::Short),
        time_zone: Some(9 * 3600),
        ..DateOptions::default()
    };
    assert_eq!(format_datetime(&utc, "ja-JP", &tokyo).as_deref(), Ok("23:30"));
    let floating = CivilDateTime::parse_iso("2026-11-05T14:30").expect("iso");
    assert_eq!(
        format_datetime(&floating, "ja-JP", &tokyo).as_deref(),
        Ok("14:30")
    );
}

#[test]
fn iso_and_offset_parsing() {
    let ok: &[(&str, (i32, u8, u8, u8, u8, u8, Option<i32>))] = &[
        ("2026-11-05", (2026, 11, 5, 0, 0, 0, None)),
        ("2026-11-05T14:30", (2026, 11, 5, 14, 30, 0, None)),
        ("2026-11-05T14:30:00+01:00", (2026, 11, 5, 14, 30, 0, Some(3600))),
        ("2026-11-05T14:30Z", (2026, 11, 5, 14, 30, 0, Some(0))),
        (
            "2026-11-05 14:30:59.123-05:00",
            (2026, 11, 5, 14, 30, 59, Some(-18_000)),
        ),
        ("-0044-03-15", (-44, 3, 15, 0, 0, 0, None)),
    ];
    for (text, (y, mo, d, h, mi, se, off)) in ok {
        let dt = CivilDateTime::parse_iso(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(
            (
                dt.year,
                dt.month,
                dt.day,
                dt.hour,
                dt.minute,
                dt.second,
                dt.offset_seconds
            ),
            (*y, *mo, *d, *h, *mi, *se, *off),
            "{text}"
        );
    }
    for bad in [
        "2026-13-01",
        "2026-11-5",
        "tomorrow",
        "2026-11-05T25:00",
        "2026-11-05T14",
        "2026-11-05T14:30+25:00",
        "",
        "20-11-05",
        "2026-11-05T14:60",
    ] {
        assert_eq!(
            CivilDateTime::parse_iso(bad).map_err(|e| e.code()),
            Err("format_bad_value"),
            "{bad}"
        );
    }
    let offsets: &[(&str, Option<i32>)] = &[
        ("+05:30", Some(19_800)),
        ("-0800", Some(-28_800)),
        ("+09", Some(32_400)),
        ("+18:00", Some(64_800)),
        ("UTC", Some(0)),
        ("+19:00", None),
        ("+5:30", None),
        ("Europe/Paris", None),
    ];
    for (text, want) in offsets {
        assert_eq!(parse_offset(text), *want, "{text}");
    }
}

#[test]
fn date_options_from_names() {
    let (o, ignored) = DateOptions::from_options([
        ("dateStyle", s("long")),
        ("timeStyle", s("short")),
        ("hour12", s("false")),
        ("timeZone", s("+02:00")),
        ("timeZoneName", s("short")),
    ])
    .expect("ok");
    assert_eq!(o.date_style, Some(DateStyle::Long));
    assert_eq!(o.time_style, Some(DateStyle::Short));
    assert_eq!((o.hour12, o.hour_cycle), (Some(false), None));
    assert_eq!(o.time_zone, Some(7200));
    assert_eq!(ignored, vec!["timeZoneName".to_owned()]);
    let (o, _) = DateOptions::from_options([
        ("month", s("long")),
        ("weekday", s("long")),
        ("hour", s("numeric")),
    ])
    .expect("ok");
    assert_eq!(
        (o.date_style, o.time_style),
        (Some(DateStyle::Full), Some(DateStyle::Short))
    );
    let (o, _) = DateOptions::from_options([("month", s("short")), ("second", s("numeric"))]).expect("ok");
    assert_eq!(
        (o.date_style, o.time_style),
        (Some(DateStyle::Medium), Some(DateStyle::Medium))
    );
    for (name, value) in [
        ("dateStyle", s("huge")),
        ("timeStyle", n(1.0)),
        ("hour12", s("yes")),
        ("hourCycle", s("h13")),
        ("timeZone", s("Europe/Paris")),
        ("numberingSystem", s("hanidec")),
    ] {
        let err = DateOptions::from_options([(name, value)]).expect_err(name);
        assert_eq!(err.code(), "format_bad_option", "{name}");
    }
}

#[test]
fn relative_time() {
    let d = NumberOptions::default();
    let cases: &[(&str, f64, RelativeUnit, RelativeNumeric, &str)] = &[
        ("en", 3.0, RelativeUnit::Day, RelativeNumeric::Always, "in 3 days"),
        (
            "en",
            -2.0,
            RelativeUnit::Hour,
            RelativeNumeric::Always,
            "2 hours ago",
        ),
        ("en", 1.0, RelativeUnit::Day, RelativeNumeric::Always, "in 1 day"),
        ("en", -1.0, RelativeUnit::Day, RelativeNumeric::Auto, "yesterday"),
        ("en", 1.0, RelativeUnit::Day, RelativeNumeric::Auto, "tomorrow"),
        ("en", 0.0, RelativeUnit::Day, RelativeNumeric::Auto, "today"),
        ("en", 2.0, RelativeUnit::Day, RelativeNumeric::Auto, "in 2 days"),
        (
            "en",
            1.5,
            RelativeUnit::Day,
            RelativeNumeric::Always,
            "in 1.5 days",
        ),
        (
            "en",
            -0.0,
            RelativeUnit::Minute,
            RelativeNumeric::Always,
            "0 minutes ago",
        ),
        (
            "en",
            1000.0,
            RelativeUnit::Year,
            RelativeNumeric::Always,
            "in 1,000 years",
        ),
        (
            "ru",
            1.0,
            RelativeUnit::Day,
            RelativeNumeric::Always,
            "через 1 день",
        ),
        (
            "ru",
            2.0,
            RelativeUnit::Day,
            RelativeNumeric::Always,
            "через 2 дня",
        ),
        (
            "ru",
            5.0,
            RelativeUnit::Day,
            RelativeNumeric::Always,
            "через 5 дней",
        ),
        (
            "ru",
            21.0,
            RelativeUnit::Day,
            RelativeNumeric::Always,
            "через 21 день",
        ),
        (
            "ru",
            -5.0,
            RelativeUnit::Hour,
            RelativeNumeric::Always,
            "5 часов назад",
        ),
        (
            "ru",
            1.5,
            RelativeUnit::Day,
            RelativeNumeric::Always,
            "через 1,5 дня",
        ),
        (
            "pl",
            2.0,
            RelativeUnit::Year,
            RelativeNumeric::Always,
            "za 2 lata",
        ),
        ("pl", 5.0, RelativeUnit::Year, RelativeNumeric::Always, "za 5 lat"),
        (
            "pl",
            22.0,
            RelativeUnit::Year,
            RelativeNumeric::Always,
            "za 22 lata",
        ),
        ("ja", 3.0, RelativeUnit::Day, RelativeNumeric::Always, "3 日後"),
        ("zh", -2.0, RelativeUnit::Hour, RelativeNumeric::Always, "2小时前"),
        (
            "de",
            -3.0,
            RelativeUnit::Day,
            RelativeNumeric::Always,
            "vor 3 Tagen",
        ),
        (
            "de",
            1000.0,
            RelativeUnit::Day,
            RelativeNumeric::Always,
            "in 1.000 Tagen",
        ),
        (
            "fr",
            -2.0,
            RelativeUnit::Day,
            RelativeNumeric::Always,
            "il y a 2 jours",
        ),
        ("fr", 1.0, RelativeUnit::Day, RelativeNumeric::Auto, "demain"),
        ("zh-TW", 3.0, RelativeUnit::Day, RelativeNumeric::Always, "+3 d"),
    ];
    for (locale, value, unit, numeric, want) in cases {
        assert_eq!(
            format_relative(*value, *unit, locale, *numeric, &d).as_deref(),
            Ok(*want),
            "{locale} {value} {unit:?}"
        );
    }
    // The noun follows the number as shown: "1.0" is not `one` in English.
    let one_place = opts(&[("minimumFractionDigits", n(1.0))]);
    assert_eq!(
        format_relative(1.0, RelativeUnit::Day, "en", RelativeNumeric::Always, &one_place).as_deref(),
        Ok("in 1.0 days")
    );
    assert_eq!(
        format_relative(f64::NAN, RelativeUnit::Day, "en", RelativeNumeric::Always, &d).map_err(|e| e.code()),
        Err("format_bad_value")
    );
    assert!(
        format_relative(
            f64::INFINITY,
            RelativeUnit::Day,
            "en",
            RelativeNumeric::Always,
            &d
        )
        .is_err()
    );
    assert_eq!(RelativeUnit::parse("days"), Some(RelativeUnit::Day));
    assert_eq!(RelativeUnit::parse("fortnight"), None);
}

#[test]
fn lists() {
    let abc = ["a", "b", "c"];
    let cases: &[(&str, &[&str], ListType, &str)] = &[
        ("en-US", &abc, ListType::And, "a, b, and c"),
        ("en-US", &abc, ListType::Or, "a, b, or c"),
        ("en-GB", &abc, ListType::And, "a, b and c"),
        ("en-US", &["a", "b"], ListType::And, "a and b"),
        ("en-US", &["a"], ListType::And, "a"),
        ("en-US", &[], ListType::And, ""),
        ("en-US", &["a", "b", "c", "d"], ListType::And, "a, b, c, and d"),
        ("de-DE", &abc, ListType::And, "a, b und c"),
        ("fr-FR", &abc, ListType::Or, "a, b ou c"),
        ("ja-JP", &abc, ListType::And, "a、b、c"),
        ("zh-CN", &abc, ListType::And, "a、b和c"),
        ("ar", &abc, ListType::And, "a وb وc"),
        ("xx", &abc, ListType::And, "a, b, c"),
    ];
    for (locale, items, kind, want) in cases {
        assert_eq!(format_list(items, locale, *kind), *want, "{locale} {kind:?}");
    }
}

// --- Through a real Fluent bundle ---------------------------------------------------------------

fn loc(locale: &str, ftl: &str) -> crate::Localization {
    crate::Localization::new(vec![(locale.to_owned(), ftl.to_owned())], None).expect("loads")
}

fn args(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn fluent_number_and_datetime_resolve() {
    let ftl = "price = { NUMBER($n, minimumFractionDigits: 2) }\n\
               when = { DATETIME($d, dateStyle: \"long\") }\n\
               cost = { NUMBER($n, style: \"currency\", currency: \"EUR\") }\n\
               at = { DATETIME($t, timeStyle: \"short\", hour12: \"false\") }\n";
    let en = loc("en-US", ftl);
    assert_eq!(en.resolve("price", &args(&[("n", "1234.5")])), "1,234.50");
    assert_eq!(
        en.resolve("when", &args(&[("d", "2026-11-05")])),
        "November 5, 2026"
    );
    let ja = loc("ja-JP", ftl);
    assert_eq!(ja.resolve("when", &args(&[("d", "2026-11-05")])), "2026年11月5日");
    let de = loc("de-DE", ftl);
    assert_eq!(de.resolve("cost", &args(&[("n", "1234.5")])), "1.234,50\u{a0}€");
    assert_eq!(de.resolve("price", &args(&[("n", "-1234.5")])), "-1.234,50");
    // Unix seconds use the game's UTC offset.
    let mut en = en;
    assert_eq!(en.resolve("at", &args(&[("t", "0")])), "00:00");
    assert!(en.set_utc_offset_seconds(3600));
    assert_eq!(en.utc_offset_seconds(), 3600);
    assert_eq!(en.resolve("at", &args(&[("t", "0")])), "01:00");
    assert!(!en.set_utc_offset_seconds(3600));
    assert!(!en.set_utc_offset_seconds(30 * 3600));
    // NaN and infinity reach NUMBER() as text and format as symbols.
    assert_eq!(en.resolve("price", &args(&[("n", "NaN")])), "NaN");
    assert_eq!(en.resolve("price", &args(&[("n", "inf")])), "∞");
    assert_eq!(
        en.resolve("price", &args(&[("n", "1e21")])),
        "1,000,000,000,000,000,000,000.00"
    );
}

#[test]
fn fluent_bad_options_are_fluent_errors_not_panics() {
    let ftl = "a = x{ NUMBER($n, minimumFractionDigits: \"lots\") }y\n\
               b = { NUMBER($n, style: \"bogus\") }\n\
               c = { DATETIME($d, timeZone: \"Europe/Paris\") }\n\
               d = { DATETIME($d) }\n\
               e = { NUMBER($n, maximumFractionDigits: 500) }\n\
               f = { NUMBER($n) }\n\
               g = { NUMBER() }\n";
    let en = loc("en", ftl);
    assert_eq!(en.resolve("a", &args(&[("n", "1")])), "xNUMBER()y");
    assert_eq!(en.resolve("b", &args(&[("n", "1")])), "NUMBER()");
    assert_eq!(en.resolve("c", &args(&[("d", "2026-11-05")])), "DATETIME()");
    assert_eq!(en.resolve("d", &args(&[("d", "soon")])), "DATETIME()");
    assert_eq!(en.resolve("e", &args(&[("n", "1")])), "NUMBER()");
    assert_eq!(en.resolve("f", &args(&[("n", "many")])), "NUMBER()");
    assert_eq!(en.resolve("g", &[]), "NUMBER()");
}

#[test]
fn fluent_plural_selectors_read_the_formatted_number() {
    let ftl = "items = { NUMBER($n) ->\n    [one] one item\n   *[other] { NUMBER($n) } items\n}\n\
               fixed = { NUMBER($n, minimumFractionDigits: 1) ->\n    [one] one\n   *[other] other\n}\n\
               zero = { NUMBER($n) ->\n    [0] none\n    [one] one\n   *[other] many\n}\n";
    let en = loc("en", ftl);
    assert_eq!(en.resolve("items", &args(&[("n", "1")])), "one item");
    assert_eq!(en.resolve("items", &args(&[("n", "1234")])), "1,234 items");
    assert_eq!(en.resolve("fixed", &args(&[("n", "1")])), "other");
    assert_eq!(en.resolve("zero", &args(&[("n", "0")])), "none");
    assert_eq!(en.resolve("zero", &args(&[("n", "1")])), "one");
    assert_eq!(en.resolve("zero", &args(&[("n", "7")])), "many");
    let ru = loc(
        "ru",
        "days = { NUMBER($n) ->\n    [one] { NUMBER($n) } день\n    [few] { NUMBER($n) } дня\n   *[many] { NUMBER($n) } дней\n}\n",
    );
    assert_eq!(ru.resolve("days", &args(&[("n", "21")])), "21 день");
    assert_eq!(ru.resolve("days", &args(&[("n", "1234")])), "1\u{a0}234 дня");
    assert_eq!(ru.resolve("days", &args(&[("n", "5")])), "5 дней");
}

#[test]
fn pseudo_mode_keeps_formatted_values() {
    let mut en = loc("en", "price = Price { NUMBER($n, minimumFractionDigits: 2) }\n");
    assert!(en.set_pseudo(true));
    let out = en.resolve("price", &args(&[("n", "1234.5")]));
    assert!(out.contains("1,234.50"), "{out}");
}
