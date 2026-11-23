// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): boundary and hostile inputs for the small pure helpers: colour
//! parsing, scale modes, text-edit boundaries, plural operands and pseudo-localization. Headless,
//! deterministic, no goldens, no GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_ui::UiColor;
use regia_ui::plural::{self, PluralCategory, PluralOperands};
use regia_ui::pseudo::{self, PSEUDO_FILL};
use regia_ui::scale::{ScaleMode, UiAccessibility, UiDevicePolicy, UiScales};
use regia_ui::text_edit::{
    InputFilter, line_bounds, next_grapheme, next_word, prev_grapheme, prev_word, segment_at, snap_grapheme,
};

// ---- colour -------------------------------------------------------------------------------

#[test]
fn hex_rejects_signs_and_non_hex_digits_that_integer_parsing_would_accept() {
    // `u32::from_str_radix` accepts a leading `+`; a colour literal must not.
    assert_eq!(UiColor::from_hex("#+FFFFF"), None);
    assert_eq!(UiColor::from_hex("#+FFFFFFF"), None);
    assert_eq!(UiColor::from_hex("#-FFFFF"), None);
    assert_eq!(UiColor::from_hex("# FFFFF"), None);
    assert_eq!(UiColor::from_hex("#"), None);
    assert_eq!(UiColor::from_hex(""), None);
    assert_eq!(UiColor::from_hex("#FFFFFFF"), None, "7 digits");
    assert_eq!(UiColor::from_hex("#FFFFFFFFF"), None, "9 digits");
    // Six bytes that are not six digits (multi-byte characters).
    assert_eq!(UiColor::from_hex("#éééé"), None);
    assert_eq!(UiColor::from_hex("#ééé"), None);
}

#[test]
fn hex_accepts_both_cases_and_orders_channels() {
    let c = UiColor::from_hex("#ff8000").expect("lower case");
    assert_eq!(Some(c), UiColor::from_hex("#FF8000"));
    assert_eq!(c.0[0], 1.0);
    assert!(c.0[1] > 0.2 && c.0[1] < 0.22, "0x80 green is about 0.216 linear");
    assert_eq!(c.0[2], 0.0);
    assert_eq!(c.0[3], 1.0, "six digits are opaque");
    let a = UiColor::from_hex("#00000000").expect("transparent");
    assert_eq!(a, UiColor::from_srgb8(0, 0, 0, 0));
}

#[test]
fn lerp_clamps_t_and_survives_nan() {
    let a = UiColor::linear(0.0, 0.2, 0.4, 1.0);
    let b = UiColor::linear(1.0, 0.6, 0.8, 0.0);
    assert_eq!(a.lerp(b, -3.0), a);
    assert_eq!(a.lerp(b, 3.0), b);
    assert_eq!(a.lerp(b, f32::INFINITY), b);
    assert_eq!(a.lerp(b, f32::NEG_INFINITY), a);
    let n = a.lerp(b, f32::NAN);
    assert!(
        n.0.iter().all(|c| c.is_finite()),
        "NaN t must not poison a colour: {n:?}"
    );
    assert_eq!(n, a, "NaN t counts as the start colour");
}

#[test]
fn color_deserialize_reports_bad_hex_and_accepts_both_forms() {
    let hex: UiColor = ron::from_str("\"#336699\"").expect("hex");
    assert_eq!(Some(hex), UiColor::from_hex("#336699"));
    let lin: UiColor = ron::from_str("(0.1, 0.2, 0.3, 0.4)").expect("channels");
    assert_eq!(lin, UiColor::linear(0.1, 0.2, 0.3, 0.4));
    let err = ron::from_str::<UiColor>("\"#+FFFFF\"").expect_err("signed hex is invalid");
    assert!(err.to_string().contains("invalid color"), "{err}");
    assert!(ron::from_str::<UiColor>("\"red\"").is_err());
    assert!(
        ron::from_str::<UiColor>("(1.0, 1.0, 1.0)").is_err(),
        "three channels"
    );
    // Serialization round-trips exactly through the linear form.
    let text = ron::to_string(&hex).expect("serialize");
    assert_eq!(ron::from_str::<UiColor>(&text).expect("round trip"), hex);
}

// ---- scale --------------------------------------------------------------------------------

#[test]
fn scale_with_screen_never_collapses_or_goes_nan() {
    let mode = ScaleMode::default();
    for size in [
        [0.0, 0.0],
        [-5.0, -5.0],
        [f32::NAN, f32::NAN],
        [1.0, 1.0],
        [1e-9, 1e9],
    ] {
        let s = mode.scale_for(size, 1.0);
        assert!(s.is_finite() && s >= 0.05, "{size:?} -> {s}");
    }
    let wild = ScaleMode::ScaleWithScreen {
        reference: [0.0, -1.0],
        match_width: f32::NAN,
    };
    let s = wild.scale_for([1280.0, 800.0], 1.0);
    assert!(s.is_finite() && s >= 0.05, "degenerate mode -> {s}");
}

#[test]
fn match_width_blends_between_the_axes() {
    let at = |match_width| ScaleMode::ScaleWithScreen {
        reference: [1000.0, 500.0],
        match_width,
    };
    let size = [2000.0, 500.0];
    assert_eq!(at(1.0).scale_for(size, 1.0), 2.0, "match width");
    assert_eq!(at(0.0).scale_for(size, 1.0), 1.0, "match height");
    assert_eq!(at(0.5).scale_for(size, 1.0), 1.5);
    assert_eq!(at(7.0).scale_for(size, 1.0), 2.0, "above 1 clamps to width");
    assert_eq!(at(-7.0).scale_for(size, 1.0), 1.0, "below 0 clamps to height");
}

#[test]
fn physical_size_mode_ignores_a_bogus_os_scale_factor() {
    for factor in [0.0, -2.0, f64::NAN, f64::INFINITY] {
        let s = ScaleMode::ConstantPhysicalSize.scale_for([1920.0, 1080.0], factor);
        assert_eq!(s, 1.0, "factor {factor}");
    }
    assert_eq!(ScaleMode::ConstantPhysicalSize.scale_for([1.0, 1.0], 1.25), 1.25);
}

#[test]
fn fit_scale_and_overflow_agree() {
    let mode = ScaleMode::HANDHELD;
    assert_eq!(mode.fit_scale([1280.0, 800.0]), Some(1.0));
    assert_eq!(mode.fit_scale([2560.0, 800.0]), Some(1.0), "height limits");
    assert_eq!(mode.fit_scale([0.0, 0.0]).map(|s| s >= 0.05), Some(true));
    assert_eq!(ScaleMode::ConstantPixelSize.fit_scale([10.0, 10.0]), None);
    assert_eq!(ScaleMode::ConstantPhysicalSize.fit_scale([10.0, 10.0]), None);
    // A layout that fills the reference never overflows at the fitted scale.
    let size = [1280.0, 720.0];
    let fit = mode.fit_scale(size).expect("fit");
    assert_eq!(mode.reference_overflow(size, fit), [0.0, 0.0]);
    // The documented 16:10 case: a 1920x1080 reference at the blended scale on 1280x800.
    let default = ScaleMode::default();
    let blended = default.scale_for([1280.0, 800.0], 1.0);
    let over = default.reference_overflow([1280.0, 800.0], blended);
    assert!((over[0] - 71.0).abs() < 2.0, "{over:?}");
    assert_eq!(over[1], 0.0);
    assert_eq!(
        ScaleMode::ConstantPixelSize.reference_overflow([1.0, 1.0], 9.0),
        [0.0; 2]
    );
}

#[test]
fn policy_caps_only_when_fit_reference_is_on() {
    let size = [1280.0, 800.0];
    let mode = ScaleMode::default();
    let free = UiDevicePolicy::DESKTOP.scale_for(&mode, size, 1.0);
    let fitted = UiDevicePolicy::HANDHELD.scale_for(&mode, size, 1.0);
    assert!(fitted <= free);
    assert_eq!(Some(fitted), mode.fit_scale(size).map(|f| f.min(free)));
    // Modes without a reference are untouched by the policy.
    assert_eq!(
        UiDevicePolicy::HANDHELD.scale_for(&ScaleMode::ConstantPixelSize, size, 2.0),
        1.0
    );
    let scales = UiScales {
        window_size: size,
        scale_factor: 1.0,
    };
    assert_eq!(scales.scale_with_policy(&mode, None), scales.scale_of(&mode));
    assert_eq!(
        scales.scale_with_policy(&mode, Some(&UiDevicePolicy::HANDHELD)),
        fitted
    );
}

#[test]
fn title_safe_insets_survive_hostile_values() {
    let at = |title_safe| UiDevicePolicy {
        title_safe,
        ..UiDevicePolicy::DESKTOP
    };
    for v in [f32::NAN, f32::NEG_INFINITY, -1.0, 0.0] {
        assert_eq!(at(v).title_safe_insets([1920.0, 1080.0]), [0.0; 4], "{v}");
    }
    assert_eq!(
        at(f32::INFINITY).title_safe_insets([100.0, 200.0]),
        [25.0, 50.0, 25.0, 50.0]
    );
}

#[test]
fn device_policy_env_is_exact() {
    let env = |v: &'static str| move |k: &str| (k == "SteamDeck").then(|| v.to_owned());
    for v in ["0", "", "true", "01", " 1"] {
        assert_eq!(UiDevicePolicy::from_env(env(v)), UiDevicePolicy::DESKTOP, "{v:?}");
    }
    assert_eq!(UiDevicePolicy::from_env(env("1")), UiDevicePolicy::HANDHELD);
    assert_eq!(
        UiDevicePolicy::from_env(|k| (k == "SteamDeckX").then(|| "1".to_owned())),
        UiDevicePolicy::DESKTOP
    );
}

#[test]
fn text_scale_boundaries_are_inclusive_and_default_is_identity() {
    let at = |text_scale| UiAccessibility {
        text_scale,
        ..UiAccessibility::default()
    };
    assert_eq!(UiAccessibility::default().effective_text_scale(), 1.0);
    assert_eq!(at(UiAccessibility::MIN_TEXT_SCALE).effective_text_scale(), 0.5);
    assert_eq!(at(UiAccessibility::MAX_TEXT_SCALE).effective_text_scale(), 3.0);
    assert_eq!(at(f32::NEG_INFINITY).effective_text_scale(), 1.0);
    assert_eq!(at(-1.0).effective_text_scale(), UiAccessibility::MIN_TEXT_SCALE);
}

// ---- text edit ----------------------------------------------------------------------------

#[test]
fn boundary_helpers_never_panic_inside_a_multibyte_character() {
    let s = "aé😀b\nñ";
    for i in 0..=s.len() + 3 {
        // None of these may panic for any byte index, including ones inside a character.
        let (a, b) = line_bounds(s, i);
        assert!(a <= b && b <= s.len(), "line_bounds({i}) = ({a}, {b})");
        assert!(
            s.is_char_boundary(a) && s.is_char_boundary(b),
            "line_bounds({i}) = ({a}, {b})"
        );
        for p in [prev_grapheme(s, i), next_grapheme(s, i), snap_grapheme(s, i)] {
            assert!(s.is_char_boundary(p), "grapheme boundary {p} for {i}");
        }
        for p in [prev_word(s, i), next_word(s, i)] {
            assert!(s.is_char_boundary(p), "word boundary {p} for {i}");
        }
        let (a, b) = segment_at(s, i);
        assert!(a <= b && b <= s.len() && s.is_char_boundary(a) && s.is_char_boundary(b));
    }
}

#[test]
fn line_bounds_handles_crlf_trailing_break_and_blank_lines() {
    let s = "ab\r\n\ncd\n";
    assert_eq!(
        line_bounds(s, 0),
        (0, 3),
        "a CR before the break stays in the line"
    );
    assert_eq!(line_bounds(s, 4), (4, 4), "blank line");
    assert_eq!(line_bounds(s, 5), (5, 7));
    assert_eq!(
        line_bounds(s, s.len()),
        (s.len(), s.len()),
        "after the final break"
    );
    assert_eq!(line_bounds(s, 999), (s.len(), s.len()), "past the end clamps");
}

#[test]
fn boundary_helpers_on_empty_and_out_of_range_input() {
    assert_eq!(prev_grapheme("", 5), 0);
    assert_eq!(next_grapheme("", 0), 0);
    assert_eq!(snap_grapheme("", 3), 0);
    assert_eq!(prev_word("", 0), 0);
    assert_eq!(next_word("", 0), 0);
    assert_eq!(segment_at("", 0), (0, 0));
    assert_eq!(
        prev_grapheme("abc", 99),
        2,
        "an index past the end counts as the end"
    );
    assert_eq!(next_grapheme("abc", 99), 3);
    // Punctuation-only text has no words: both jumps run to the ends.
    assert_eq!(prev_word("!?!", 2), 0);
    assert_eq!(next_word("!?!", 0), 3);
}

#[test]
fn digits_and_apostrophes_count_as_words() {
    let s = "it's 42, ok";
    assert_eq!(next_word(s, 0), 4, "it's is one word");
    assert_eq!(next_word(s, 4), 7, "42 is a word");
    assert_eq!(prev_word(s, 7), 5);
}

#[test]
fn filters_on_signs_and_unicode_digits() {
    let table: [(InputFilter, &str, bool); 12] = [
        (InputFilter::Integer, "+", true),
        (InputFilter::Integer, "+5", true),
        (InputFilter::Integer, "-+5", false),
        (InputFilter::Integer, "٣", false),
        (InputFilter::Integer, " 5", false),
        (InputFilter::Numeric, "+.5", true),
        (InputFilter::Numeric, "-", true),
        (InputFilter::Numeric, "..", false),
        (InputFilter::Numeric, "1,5", false),
        (InputFilter::Numeric, "1e5", false),
        (InputFilter::Numeric, "-.", true),
        (InputFilter::Any, "", true),
    ];
    for (filter, text, ok) in table {
        assert_eq!(filter.accepts(text), ok, "{} on {text:?}", filter.name());
    }
    assert_eq!(InputFilter::default(), InputFilter::Any);
    let names: Vec<_> = [
        InputFilter::Any,
        InputFilter::Integer,
        InputFilter::Numeric,
        InputFilter::Custom(|_| true),
    ]
    .iter()
    .map(InputFilter::name)
    .collect();
    assert_eq!(names, ["any", "integer", "numeric", "custom"]);
    fn only_a(s: &str) -> bool {
        s.chars().all(|c| c == 'a')
    }
    fn only_b(s: &str) -> bool {
        s.chars().all(|c| c == 'b')
    }
    assert_eq!(InputFilter::Custom(only_a), InputFilter::Custom(only_a));
    assert_ne!(InputFilter::Custom(only_a), InputFilter::Custom(only_b));
    assert_ne!(InputFilter::Custom(only_a), InputFilter::Any);
}

// ---- plural -------------------------------------------------------------------------------

#[test]
fn operands_on_hostile_text() {
    for bad in [
        "-", "+1", "--1", "1.2.3", ".5", "5.", " 1", "1 ", "٣", "1_000", "NaN", "inf", "0x10", "1e3", "1,5",
        "-.5",
    ] {
        assert_eq!(PluralOperands::from_decimal(bad), None, "{bad:?}");
    }
    let o = PluralOperands::from_decimal("-0").expect("-0");
    assert_eq!((o.n, o.i, o.v), (0.0, 0, 0));
    assert!(o.n.is_sign_positive(), "n is the absolute value");
    let o = PluralOperands::from_decimal("007.100").expect("padded");
    assert_eq!((o.i, o.v, o.w, o.f, o.t), (7, 3, 1, 100, 1));
    let o = PluralOperands::from_decimal("2.000").expect("zero fraction");
    assert_eq!((o.i, o.v, o.w, o.f, o.t), (2, 3, 0, 0, 0));
}

#[test]
fn operands_from_floats_round_trip_the_shortest_form() {
    for (value, v) in [
        (0.0, 0),
        (-0.0, 0),
        (1.5, 1),
        (0.1, 1),
        (1e21, 0),
        (1e-7, 7),
        (f64::MAX, 0),
    ] {
        let o = PluralOperands::from_f64(value).unwrap_or_else(|| panic!("{value}"));
        assert_eq!(o.v, v, "{value}");
        assert!(o.n >= 0.0);
    }
    assert_eq!(PluralOperands::from_f64(f64::INFINITY), None);
    assert_eq!(PluralOperands::from_f64(f64::NEG_INFINITY), None);
    assert_eq!(PluralOperands::from_f64(-3.0), Some(PluralOperands::integer(3)));
}

#[test]
fn huge_numbers_are_not_mistaken_for_small_ones() {
    // 10^18 + 1 has the same last 18 digits as 1, but it is not one.
    let big = "1000000000000000001";
    assert_eq!(plural::category_of_text("en", big), Some(PluralCategory::Other));
    assert_eq!(plural::category_of_text("fr", big), Some(PluralCategory::Other));
    assert_eq!(plural::category_of_text("cs", big), Some(PluralCategory::Other));
    assert_eq!(plural::category_of_text("he", big), Some(PluralCategory::Other));
    assert_eq!(
        plural::category_of_text("pl", big),
        Some(PluralCategory::Many),
        "pl: not 1, ends in 1"
    );
    assert_eq!(
        plural::category_of_text("ru", big),
        Some(PluralCategory::One),
        "ru: ends in 1 not 11"
    );
    // Rules written on `n` read the digits, not a rounded float: 2^53 + 1 is odd, and 1 + 10^-19
    // is not 1.
    assert_eq!(
        plural::category_of_text("ar", "9007199254740993"),
        Some(PluralCategory::Many),
        "...93 % 100 = 93"
    );
    assert_eq!(
        plural::category_of_text("es", "1.0000000000000000001"),
        Some(PluralCategory::Other)
    );
    assert_eq!(plural::category_of_text("es", "1.000"), Some(PluralCategory::One));
    assert_eq!(
        plural::category_of_text("he", "1000000000000000010"),
        Some(PluralCategory::Many)
    );
    // Leading zeros do not count as digits.
    assert_eq!(
        plural::category_of_text("en", "0000000000000000000001"),
        Some(PluralCategory::One)
    );
    let max = format!("{}", u64::MAX);
    assert!(plural::category_of_text("ar", &max).is_some());
    let long = "9".repeat(400);
    assert!(
        plural::category_of_text("en", &long).is_some(),
        "400 digits do not overflow"
    );
}

#[test]
fn locale_tags_are_normalized_and_unknown_ones_fall_back() {
    for tag in ["en", "EN", "en-US", "en_US", "en_gb"] {
        assert!(plural::is_known(tag), "{tag}");
    }
    for tag in ["", "-", "xx", "tlh-US", "e"] {
        assert!(!plural::is_known(tag), "{tag:?}");
        assert_eq!(plural::categories(tag), None);
        assert_eq!(
            plural::category(tag, &PluralOperands::integer(1)),
            PluralCategory::Other
        );
    }
    // Portuguese: Brazil counts 0 as singular, Portugal does not.
    assert_eq!(plural::category_of_text("pt-BR", "0"), Some(PluralCategory::One));
    assert_eq!(
        plural::category_of_text("pt_PT", "0"),
        Some(PluralCategory::Other)
    );
    assert_eq!(plural::category_of_text("pt", "1.5"), Some(PluralCategory::One));
    assert_eq!(
        plural::category_of_text("pt-PT", "1.5"),
        Some(PluralCategory::Other)
    );
}

#[test]
fn every_category_a_rule_returns_is_listed_for_its_locale() {
    let locales = [
        "en", "es", "fr", "pt", "ru", "pl", "cs", "ar", "he", "ja", "tr", "uk", "sk",
    ];
    let numbers = [
        "0", "1", "2", "3", "4", "5", "10", "11", "12", "14", "20", "21", "22", "25", "100", "101", "102",
        "111", "0.5", "1.0", "1.5", "2.5", "1000000", "1000001",
    ];
    for locale in locales {
        let listed = plural::categories(locale).expect("known");
        assert_eq!(listed.last(), Some(&PluralCategory::Other), "{locale}");
        for n in numbers {
            let got = plural::category_of_text(locale, n).expect("number");
            assert!(listed.contains(&got), "{locale} {n} -> {got} not in {listed:?}");
        }
        // Deterministic: asking again gives the same answer.
        assert_eq!(
            plural::category_of_text(locale, "21"),
            plural::category_of_text(locale, "21")
        );
    }
}

#[test]
fn czech_slovak_hebrew_and_east_slavic_decimals() {
    let cat = |l, n| plural::category_of_text(l, n).expect("number");
    assert_eq!(cat("cs", "1.5"), PluralCategory::Many);
    assert_eq!(cat("cs", "2"), PluralCategory::Few);
    assert_eq!(cat("cs", "5"), PluralCategory::Other);
    assert_eq!(cat("sk", "4"), PluralCategory::Few);
    assert_eq!(cat("he", "2"), PluralCategory::Two);
    assert_eq!(cat("he", "20"), PluralCategory::Many);
    assert_eq!(cat("he", "10"), PluralCategory::Other);
    assert_eq!(cat("he", "1.0"), PluralCategory::Other, "visible fraction digits");
    assert_eq!(cat("uk", "21"), PluralCategory::One);
    assert_eq!(cat("be", "13"), PluralCategory::Many);
    assert_eq!(cat("ru", "21.0"), PluralCategory::Other);
    assert_eq!(cat("ar", "1002"), PluralCategory::Other);
    assert_eq!(cat("ar", "1003"), PluralCategory::Few);
    assert_eq!(cat("ar", "3.0"), PluralCategory::Few, "3.0 is still n = 3");
}

// ---- pseudo-localization ------------------------------------------------------------------

#[test]
fn pseudo_text_is_deterministic_and_keeps_spacing() {
    assert_eq!(pseudo::pseudo_text(""), "");
    // Three spaces: two fill characters, then the trailing spaces.
    assert_eq!(pseudo::pseudo_text("   "), "~~   ");
    assert_eq!(pseudo::pseudo_text("a"), format!("á{PSEUDO_FILL}{PSEUDO_FILL}"));
    // Leading space stays, trailing spaces stay after the fill.
    let out = pseudo::pseudo_text("  ab  ");
    assert!(out.starts_with("  "), "{out:?}");
    assert!(out.ends_with(&format!("{PSEUDO_FILL}  ")), "{out:?}");
    // Non-ASCII letters count as letters (they get a fill) but are not accented.
    let out = pseudo::pseudo_text("日本");
    assert_eq!(out, format!("日本{PSEUDO_FILL}{PSEUDO_FILL}"));
    assert_eq!(
        pseudo::pseudo_text("héllo wörld"),
        pseudo::pseudo_text("héllo wörld")
    );
    assert_eq!(pseudo::padding_for(0), 2);
    assert_eq!(pseudo::padding_for(5), 2);
    assert_eq!(pseudo::padding_for(6), 3);
    assert_eq!(pseudo::wrap(""), "[]");
}

#[test]
fn pseudo_ftl_edge_cases() {
    assert_eq!(pseudo::pseudo_ftl("").expect("empty"), "");
    // Comments and attribute-only messages survive.
    let src = "# note\nonly-attr =\n    .title = Hello\n-brand = Regia\nref = { -brand } rocks\n";
    let out = pseudo::pseudo_ftl(src).expect("parse");
    assert!(out.contains("# note"), "{out}");
    assert!(out.contains(".title = Ĥéľľö~~"), "{out}");
    assert!(out.contains("{ -brand }"), "term reference kept: {out}");
    assert!(out.contains("-brand = "), "{out}");
    // Deterministic and the output is valid Fluent again.
    assert_eq!(out, pseudo::pseudo_ftl(src).expect("again"));
    assert!(pseudo::pseudo_ftl(&out).is_ok(), "{out}");
    // An error names a byte offset instead of panicking.
    let err = pseudo::pseudo_ftl("a = { $x").expect_err("broken");
    assert!(err.contains("at byte"), "{err}");
}
