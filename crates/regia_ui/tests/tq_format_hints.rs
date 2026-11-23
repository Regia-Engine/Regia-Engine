// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): binding value formatting and hint-glyph expansion on hostile
//! specs and text. Pure functions, no goldens, no GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_ui::binding::{format_value, format_zero_padded};
use regia_ui::focus::UiInputMode;
use regia_ui::hints::{HintAction, HintBar, HintDevice, expand_hints};

#[test]
fn a_huge_width_or_precision_in_a_spec_does_not_panic() {
    // `format!` panics on a runtime width above u16::MAX; an authored spec must not reach that.
    for spec in [
        "{:.70000}",
        "{:70000}",
        "{:070000}",
        "{:.99999999999999999999}",
        "{:0999999999}",
    ] {
        let out = format_value("1.5", Some(spec));
        assert!(out.len() <= 300, "{spec}: {} bytes", out.len());
        let out = format_value("7", Some(spec));
        assert!(out.len() <= 300, "{spec}: {} bytes", out.len());
    }
    assert!(format_zero_padded("7", usize::MAX).len() <= 300);
}

#[test]
fn whole_numbers_keep_every_digit() {
    let big = "18446744073709551615";
    assert_eq!(format_value(big, Some("{}")), big);
    assert_eq!(format_value(big, Some("{:25}")), format!("{big:>25}"));
    assert_eq!(
        format_value("9007199254740993", Some("{:020}")),
        "00009007199254740993"
    );
    assert_eq!(format_value("-9007199254740993", Some("{}")), "-9007199254740993");
    assert_eq!(format_zero_padded(big, 22), format!("00{big}"));
}

#[test]
fn zero_padding_counts_the_sign_and_keeps_the_fraction() {
    assert_eq!(format_value("7.5", Some("{:05}")), "007.5");
    assert_eq!(format_value("-7.5", Some("{:06}")), "-007.5");
    assert_eq!(format_value("-7", Some("{:03}")), "-07");
    assert_eq!(format_value("0", Some("{:03}")), "000");
    assert_eq!(format_value("1234", Some("{:03}")), "1234", "never truncates");
    assert_eq!(
        format_zero_padded("  42 ", 4),
        "0042",
        "surrounding space is ignored"
    );
    assert_eq!(format_zero_padded("", 4), "");
}

#[test]
fn specs_and_values_that_are_not_numbers_pass_through() {
    for (raw, spec) in [
        ("hello", "{:03}"),
        ("hello", "{:.2}"),
        ("", "{:5}"),
        ("true", "{}"),
    ] {
        assert_eq!(format_value(raw, Some(spec)), raw, "{raw:?} {spec}");
    }
    // Specs the formatter does not know leave the number alone.
    assert_eq!(format_value("42", Some("{:x}")), "42");
    assert_eq!(format_value("42", Some("")), "42");
    assert_eq!(format_value("42", Some("{:>5}")), "42");
    assert_eq!(format_value("2.50", Some("{}")), "2.5");
    // Non-finite floats format without panicking.
    assert_eq!(format_value("NaN", Some("{:.2}")), "NaN");
    assert_eq!(format_value("inf", Some("{:.2}")), "inf");
    assert_eq!(format_value("-inf", Some("{:.1}")), "-inf");
}

#[test]
fn precision_rounds_half_to_even_like_rust_display_and_pads_integers() {
    assert_eq!(
        format_value("2.675", Some("{:.2}")),
        "2.67",
        "binary float, as Rust prints it"
    );
    assert_eq!(format_value("7", Some("{:.2}")), "7.00");
    assert_eq!(format_value("-0.04", Some("{:.1}")), "-0.0");
    assert_eq!(format_value("3.9", Some("{:.0}")), "4");
    assert_eq!(
        format_value("3.9", Some("{:.}")),
        "4",
        "an empty precision counts as 0"
    );
}

#[test]
fn hint_placeholders_expand_and_everything_else_is_kept() {
    let d = HintDevice::PlayStation;
    assert_eq!(expand_hints("Press {activate}", d), "Press Cross");
    assert_eq!(expand_hints("{back}{menu}", d), "CircleOptions");
    assert_eq!(expand_hints("{unknown} stays", d), "{unknown} stays");
    // Nested braces are not a placeholder: the text is kept as written.
    assert_eq!(expand_hints("{} and {{activate}}", d), "{} and {{activate}}");
    assert_eq!(expand_hints("open { brace", d), "open { brace");
    assert_eq!(expand_hints("{activate", d), "{activate");
    assert_eq!(expand_hints("close } only", d), "close } only");
    assert_eq!(expand_hints("", d), "");
    assert_eq!(
        expand_hints("{Activate}", d),
        "{Activate}",
        "names are case sensitive"
    );
    assert_eq!(expand_hints("日本 {back} ñ", d), "日本 Circle ñ");
    // Expansion is not recursive: a glyph is never expanded again.
    assert_eq!(expand_hints("{activate}", HintDevice::KeyboardMouse), "Enter");
}

#[test]
fn every_device_has_a_distinct_nonempty_glyph_for_every_action() {
    for device in [
        HintDevice::KeyboardMouse,
        HintDevice::Xbox,
        HintDevice::PlayStation,
        HintDevice::SteamDeck,
    ] {
        let glyphs: Vec<&str> = HintAction::ALL.iter().map(|a| device.glyph(*a)).collect();
        assert!(
            glyphs.iter().all(|g| !g.is_empty() && !g.contains(['{', '}'])),
            "{device:?}"
        );
        let mut unique = glyphs.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            HintAction::ALL.len(),
            "{device:?} reuses a glyph: {glyphs:?}"
        );
        // Every action's own placeholder expands on every device.
        for a in HintAction::ALL {
            assert_eq!(
                expand_hints(&format!("{{{}}}", a.name()), device),
                device.glyph(a)
            );
            assert_eq!(HintAction::parse(a.name()), Some(a));
        }
    }
    assert_eq!(HintAction::parse(""), None);
    assert_eq!(HintAction::parse("ACTIVATE"), None);
}

#[test]
fn hint_bar_shows_only_in_navigation_mode_and_only_with_entries() {
    let mut bar = HintBar::default();
    assert!(!bar.visible(UiInputMode::Navigation), "empty bar");
    bar.set([(HintAction::Activate, "Select"), (HintAction::Back, "Back")]);
    assert!(bar.visible(UiInputMode::Navigation));
    assert!(!bar.visible(UiInputMode::Pointer));
    assert_eq!(bar.lines(HintDevice::Xbox), ["A Select", "B Back"]);
    assert_eq!(bar.lines(HintDevice::KeyboardMouse), ["Enter Select", "Esc Back"]);
    bar.set(Vec::<(HintAction, String)>::new());
    assert!(bar.entries.is_empty() && bar.lines(HintDevice::Xbox).is_empty());
}
