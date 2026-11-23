// SPDX-License-Identifier: MPL-2.0
//! Public-behavior tests for the pure parts of the UI crate: widget value math, UI scale modes,
//! the device policy, colors (hex and serde) and localization fallbacks. No GPU, no world.

use regia_ui::color::UiColor;
use regia_ui::localization::Localization;
use regia_ui::scale::{HANDHELD_MIN_TEXT_PX, ScaleMode, UiDevicePolicy};
use regia_ui::scroll::decay;
use regia_ui::widgets::{Dropdown, Modal, ProgressBar, Slider, TextInput, Tooltip, VirtualStick};

fn slider(min: f32, max: f32, step: f32) -> Slider {
    Slider {
        value: min,
        min,
        max,
        step,
    }
}

#[test]
fn slider_fraction_input_is_clamped() {
    let s = slider(0.0, 10.0, 0.0);
    assert_eq!(s.value_at_fraction(-3.0), 0.0);
    assert_eq!(s.value_at_fraction(7.0), 10.0);
    assert_eq!(s.value_at_fraction(0.0), 0.0);
    assert_eq!(s.value_at_fraction(1.0), 10.0);
}

#[test]
fn slider_negative_range_midpoint() {
    let s = slider(-10.0, 10.0, 0.0);
    assert_eq!(s.value_at_fraction(0.5), 0.0);
    assert_eq!(s.value_at_fraction(0.25), -5.0);
}

#[test]
fn slider_step_never_exceeds_max() {
    // 1.0 / 0.6 rounds up to 2 steps (1.2); the result must still be clamped to the maximum.
    let s = slider(0.0, 1.0, 0.6);
    assert_eq!(s.value_at_fraction(1.0), 1.0);
    // A fraction that rounds down lands on a step.
    let v = s.value_at_fraction(0.4);
    assert!((v - 0.6).abs() < 1e-5, "{v}");
}

#[test]
fn slider_fraction_of_value_edge_cases() {
    // A zero-width range reports 0 instead of dividing by zero.
    let flat = Slider {
        value: 3.0,
        min: 3.0,
        max: 3.0,
        step: 0.0,
    };
    assert_eq!(flat.fraction(), 0.0);
    // Out-of-range values clamp to the ends.
    let high = Slider {
        value: 99.0,
        ..slider(0.0, 10.0, 0.0)
    };
    assert_eq!(high.fraction(), 1.0);
    let low = Slider {
        value: -99.0,
        ..slider(0.0, 10.0, 0.0)
    };
    assert_eq!(low.fraction(), 0.0);
}

#[test]
fn slider_default_is_unit_range_at_half() {
    let s = Slider::default();
    assert_eq!((s.value, s.min, s.max, s.step), (0.5, 0.0, 1.0, 0.0));
    assert_eq!(s.fraction(), 0.5);
}

#[test]
fn text_input_selection_is_ordered_either_way() {
    let mut forward = TextInput::default();
    forward.text = "hello".to_owned();
    forward.caret = 4;
    forward.selection_anchor = 1;
    assert_eq!(forward.selection(), (1, 4));
    let mut backward = forward.clone();
    backward.caret = 1;
    backward.selection_anchor = 4;
    assert_eq!(backward.selection(), (1, 4));
    let mut collapsed = forward;
    collapsed.caret = 2;
    collapsed.selection_anchor = 2;
    assert_eq!(collapsed.selection(), (2, 2));
}

#[test]
fn text_input_mask_counts_characters_not_bytes() {
    let mut input = TextInput::default();
    input.text = "héllo日".to_owned();
    input.secret = Some('•');
    assert_eq!(input.display_text().chars().count(), 6);
    assert!(input.display_text().chars().all(|c| c == '•'));
    let mut empty = TextInput::default();
    empty.secret = Some('*');
    assert_eq!(empty.display_text(), "");
}

#[test]
fn text_input_plain_display_is_the_text() {
    let mut input = TextInput::default();
    input.text = "plain".to_owned();
    input.placeholder = "hint".to_owned();
    assert_eq!(input.display_text(), "plain");
}

#[test]
fn widget_defaults_are_inert() {
    assert_eq!(ProgressBar::default().value, 0.0);
    let modal = Modal::default();
    assert!(modal.open && modal.close_on_cancel);
    let tip = Tooltip::default();
    assert!(!tip.visible && tip.text.is_empty());
    assert_eq!(tip.delay, 0.5);
    let stick = VirtualStick::default();
    assert_eq!((stick.radius, stick.value), (64.0, [0.0, 0.0]));
    let dd = Dropdown::default();
    assert!(dd.options.is_empty() && dd.selected.is_none() && !dd.open);
}

#[test]
fn inertia_decay_is_monotonic_and_scale_free() {
    let v = [40.0, -80.0];
    let a = decay(v, 0.05, 0.25);
    let b = decay(a, 0.05, 0.25);
    let both = decay(v, 0.10, 0.25);
    for ((v, a), (b, both)) in v.into_iter().zip(a).zip(b.into_iter().zip(both)) {
        assert!(a.abs() < v.abs() && b.abs() < a.abs());
        // Two half steps equal one full step (exponential decay composes).
        assert!((b - both).abs() < 1e-3, "{b} vs {both}");
        // The sign of the velocity is preserved.
        assert_eq!(a.is_sign_negative(), v.is_sign_negative());
    }
    // A negative time constant kills the velocity as well.
    assert_eq!(decay(v, 0.1, -1.0), [0.0, 0.0]);
    // Zero elapsed time changes nothing.
    assert_eq!(decay(v, 0.0, 0.25), v);
}

#[test]
fn scale_constant_modes() {
    assert_eq!(ScaleMode::ConstantPixelSize.scale_for([3840.0, 2160.0], 2.0), 1.0);
    assert_eq!(
        ScaleMode::ConstantPhysicalSize.scale_for([3840.0, 2160.0], 1.5),
        1.5
    );
}

#[test]
fn scale_with_screen_blends_width_and_height() {
    let at = |match_width: f32| {
        ScaleMode::ScaleWithScreen {
            reference: [1920.0, 1080.0],
            match_width,
        }
        .scale_for([3840.0, 1080.0], 1.0)
    };
    // Width ratio is 2, height ratio is 1.
    assert_eq!(at(1.0), 2.0);
    assert_eq!(at(0.0), 1.0);
    assert_eq!(at(0.5), 1.5);
    // The blend factor is clamped to 0..=1.
    assert_eq!(at(9.0), 2.0);
    assert_eq!(at(-9.0), 1.0);
}

#[test]
fn scale_with_screen_degenerate_inputs_stay_finite() {
    let mode = ScaleMode::ScaleWithScreen {
        reference: [1920.0, 1080.0],
        match_width: 0.5,
    };
    // A zero-sized (minimized) window is floored at 0.05.
    assert_eq!(mode.scale_for([0.0, 0.0], 1.0), 0.05);
    // A zero reference is treated as one pixel and never divides by zero.
    let zero_ref = ScaleMode::ScaleWithScreen {
        reference: [0.0, 0.0],
        match_width: 0.5,
    };
    let s = zero_ref.scale_for([100.0, 100.0], 1.0);
    assert!(s.is_finite() && s > 0.0);
}

#[test]
fn handheld_reference_is_scale_one_on_the_deck_panel() {
    assert_eq!(ScaleMode::HANDHELD.scale_for([1280.0, 800.0], 1.0), 1.0);
    assert_eq!(ScaleMode::HANDHELD.scale_for([2560.0, 1600.0], 1.0), 2.0);
    assert_eq!(ScaleMode::default().scale_for([1920.0, 1080.0], 1.0), 1.0);
}

#[test]
fn device_policy_follows_steamdeck_variable_only() {
    let env = |value: Option<&'static str>| {
        move |k: &str| {
            if k == "SteamDeck" {
                value.map(str::to_owned)
            } else {
                None
            }
        }
    };
    assert_eq!(UiDevicePolicy::from_env(env(Some("1"))), UiDevicePolicy::HANDHELD);
    assert_eq!(UiDevicePolicy::from_env(env(Some("0"))), UiDevicePolicy::DESKTOP);
    assert_eq!(UiDevicePolicy::from_env(env(Some(""))), UiDevicePolicy::DESKTOP);
    assert_eq!(UiDevicePolicy::from_env(env(None)), UiDevicePolicy::DESKTOP);
    // Other variables do not trigger it.
    assert_eq!(
        UiDevicePolicy::from_env(|k| (k == "STEAMDECK").then(|| "1".to_owned())),
        UiDevicePolicy::DESKTOP
    );
    assert_eq!(UiDevicePolicy::HANDHELD.min_text_px, HANDHELD_MIN_TEXT_PX);
}

#[test]
fn title_safe_insets_are_clamped() {
    let size = [1000.0, 400.0];
    let policy = |title_safe| UiDevicePolicy {
        min_text_px: 0.0,
        title_safe,
        fit_reference: false,
    };
    assert_eq!(policy(0.0).title_safe_insets(size), [0.0; 4]);
    assert_eq!(policy(0.1).title_safe_insets(size), [100.0, 40.0, 100.0, 40.0]);
    // Capped at a quarter of the window on each side.
    assert_eq!(policy(0.9).title_safe_insets(size), [250.0, 100.0, 250.0, 100.0]);
    // A negative margin never grows the UI.
    assert_eq!(policy(-0.5).title_safe_insets(size), [0.0; 4]);
}

#[test]
fn hex_rejects_malformed_strings() {
    for bad in [
        "",
        "#",
        "#FFF",
        "#FFFFFFF",
        "#FFFFFFFFF",
        "# FFFFF",
        "#12345G",
        "0xFFFFFF",
    ] {
        assert!(UiColor::from_hex(bad).is_none(), "{bad:?} should be rejected");
    }
}

#[test]
fn hex_alpha_forms_and_case() {
    let upper = UiColor::from_hex("#ABCDEF").expect("test setup");
    let lower = UiColor::from_hex("#abcdef").expect("test setup");
    assert_eq!(upper, lower);
    assert_eq!(upper.0[3], 1.0);
    let clear = UiColor::from_hex("#11223300").expect("test setup");
    assert_eq!(clear.0[3], 0.0);
    // The 6-digit form equals the 8-digit form with FF alpha.
    assert_eq!(
        UiColor::from_hex("#336699").expect("test setup"),
        UiColor::from_hex("#336699FF").expect("test setup")
    );
    // Alpha is linear, not gamma-decoded.
    let half = UiColor::from_hex("#00000080").expect("test setup");
    assert!((half.0[3] - 128.0 / 255.0).abs() < 1e-6);
}

#[test]
fn lerp_clamps_t_and_with_alpha_keeps_rgb() {
    let a = UiColor::linear(0.0, 0.25, 0.5, 1.0);
    let b = UiColor::linear(1.0, 0.75, 1.0, 0.0);
    assert_eq!(a.lerp(b, -5.0), a);
    assert_eq!(a.lerp(b, 5.0), b);
    let faded = a.with_alpha(0.25);
    assert_eq!(faded.0, [0.0, 0.25, 0.5, 0.25]);
    assert_eq!(UiColor::default(), UiColor::WHITE);
}

#[test]
fn color_deserializes_from_hex_or_channels() {
    let hex: UiColor = serde_json::from_str("\"#FFFFFF\"").expect("test setup");
    assert!(hex.0.iter().all(|&c| (c - 1.0).abs() < 1e-4));
    let channels: UiColor = serde_json::from_str("[0.25, 0.5, 0.75, 1.0]").expect("test setup");
    assert_eq!(channels, UiColor::linear(0.25, 0.5, 0.75, 1.0));
}

#[test]
fn color_bad_hex_error_names_the_input() {
    let err = serde_json::from_str::<UiColor>("\"#nothex\"").expect_err("expected an error");
    assert!(err.to_string().contains("#nothex"), "{err}");
    // Wrong channel count is rejected instead of padded.
    assert!(serde_json::from_str::<UiColor>("[0.1, 0.2, 0.3]").is_err());
}

#[test]
fn color_serialization_round_trips_exactly() {
    let c = UiColor::linear(0.125, 0.25, 0.5, 0.75);
    let json = serde_json::to_string(&c).expect("test setup");
    assert_eq!(json, "[0.125,0.25,0.5,0.75]");
    assert_eq!(serde_json::from_str::<UiColor>(&json).expect("test setup"), c);
}

fn loc_en_de() -> Localization {
    Localization::new(
        vec![
            ("en".to_owned(), "a = Apple\nb = Banana\n".to_owned()),
            ("de".to_owned(), "a = Apfel\n".to_owned()),
        ],
        Some("en".to_owned()),
    )
    .expect("test setup")
}

#[test]
fn localization_falls_back_per_key() {
    let mut l = loc_en_de();
    assert_eq!(l.available, ["de", "en"]);
    assert!(l.set_locale("de"));
    assert_eq!(l.resolve("a", &[]), "Apfel");
    // `b` only exists in the fallback bundle.
    assert_eq!(l.resolve("b", &[]), "Banana");
}

#[test]
fn localization_missing_key_marker_matches_build_profile() {
    let l = loc_en_de();
    let expected = if cfg!(debug_assertions) {
        "\u{27E6}zzz\u{27E7}"
    } else {
        "zzz"
    };
    assert_eq!(l.resolve("zzz", &[]), expected);
}

#[test]
fn localization_generation_only_moves_on_real_switches() {
    let mut l = loc_en_de();
    assert_eq!(l.generation.0, 0);
    // Re-selecting the current locale or an unknown one is a no-op.
    assert!(!l.set_locale("en"));
    assert!(!l.set_locale("fr"));
    assert_eq!(l.generation.0, 0);
    assert!(l.set_locale("de"));
    assert_eq!(l.generation.0, 1);
    assert_eq!(l.locale(), "de");
}

#[test]
fn localization_single_locale_cannot_cycle() {
    let mut l = Localization::new(vec![("en".to_owned(), "k = v".to_owned())], Some("en".to_owned()))
        .expect("test setup");
    assert_eq!(l.cycle_locale(), "en");
    assert_eq!(l.generation.0, 0);
}

#[test]
fn localization_empty_sources_resolve_to_the_marker() {
    let l = Localization::new(Vec::new(), None).expect("test setup");
    assert!(l.available.is_empty());
    assert!(!l.has("anything"));
    assert!(l.resolve("anything", &[]).contains("anything"));
}

#[test]
fn localization_ftl_errors_name_the_locale() {
    let err = Localization::new(
        vec![("en".to_owned(), "broken = { $x".to_owned())],
        Some("en".to_owned()),
    )
    .map(|_| ())
    .expect_err("expected an error");
    let text = err.to_string();
    assert!(text.contains("ftl parse errors in en"), "{text}");
}

#[test]
fn localization_resolves_without_bidi_isolation_marks() {
    let l = Localization::new(
        vec![("en".to_owned(), "hello = Hi { $name }!".to_owned())],
        Some("en".to_owned()),
    )
    .expect("test setup");
    let out = l.resolve("hello", &[("name".to_owned(), "Sam".to_owned())]);
    assert_eq!(out, "Hi Sam!");
    assert!(!out.contains('\u{2068}') && !out.contains('\u{2069}'));
}
