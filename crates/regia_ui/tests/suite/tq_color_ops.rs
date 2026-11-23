// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): theme colour operations on the whole 8-bit range and on hostile
//! numbers. Exhaustive or fixed sweeps; no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_ui::UiColor;
use regia_ui::theme_color_ops::{
    Oklab, contrast_ratio, darken, from_oklab, lighten, mix, relative_luminance, to_hex, to_oklab, with_alpha,
};

fn all_finite(c: UiColor) -> bool {
    c.0.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v))
}

#[test]
fn hex_round_trips_for_every_eight_bit_value_on_every_channel() {
    for v in 0..=255u32 {
        for h in [
            format!("#{v:02x}0000"),
            format!("#00{v:02x}00"),
            format!("#0000{v:02x}"),
            format!("#{v:02x}{v:02x}{v:02x}"),
        ] {
            let c = UiColor::from_hex(&h).expect("hex");
            assert_eq!(to_hex(c), h);
        }
    }
    // Alpha: ff is dropped, everything else is kept.
    for a in 0..=254u32 {
        let h = format!("#336699{a:02x}");
        assert_eq!(to_hex(UiColor::from_hex(&h).expect("hex")), h);
    }
    assert_eq!(to_hex(UiColor::from_hex("#336699ff").expect("hex")), "#336699");
}

#[test]
fn to_hex_clamps_out_of_range_and_non_finite_channels() {
    assert_eq!(
        to_hex(UiColor::linear(2.0, -1.0, 0.5, 3.0)),
        to_hex(UiColor::linear(1.0, 0.0, 0.5, 1.0))
    );
    assert_eq!(
        to_hex(UiColor::linear(
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN
        )),
        "#00ff0000"
    );
    assert_eq!(to_hex(UiColor::TRANSPARENT), "#00000000");
    assert_eq!(to_hex(UiColor::WHITE), "#ffffff");
    assert_eq!(to_hex(UiColor::BLACK), "#000000");
}

#[test]
fn oklab_round_trips_the_eight_bit_grid() {
    let mut worst = 0.0f32;
    for r in (0..=255u8).step_by(15) {
        for g in (0..=255u8).step_by(15) {
            for b in (0..=255u8).step_by(15) {
                let c = UiColor::from_srgb8(r, g, b, 255);
                let back = from_oklab(to_oklab(c));
                for i in 0..4 {
                    worst = worst.max((c.0[i] - back.0[i]).abs());
                }
                assert_eq!(to_hex(c), to_hex(back), "({r}, {g}, {b})");
            }
        }
    }
    assert!(worst < 2e-3, "worst linear error {worst}");
}

#[test]
fn lighten_and_darken_stay_in_gamut_and_keep_alpha() {
    let c = UiColor::from_hex("#3366cc80").expect("hex");
    for amount in [
        -5.0,
        -1.0,
        -0.3,
        0.0,
        0.3,
        1.0,
        5.0,
        f32::INFINITY,
        f32::NEG_INFINITY,
    ] {
        for result in [lighten(c, amount), darken(c, amount)] {
            assert!(all_finite(result), "amount {amount}: {result:?}");
            assert!((result.0[3] - c.0[3]).abs() < 1e-6, "alpha kept for {amount}");
        }
    }
    let white = lighten(c, 5.0);
    assert!(white.0.iter().take(3).all(|v| *v > 0.99), "{white:?}");
    let black = darken(c, 5.0);
    assert!(black.0.iter().take(3).all(|v| *v < 0.01), "{black:?}");
    // A zero amount changes nothing visible.
    assert_eq!(to_hex(lighten(c, 0.0)), to_hex(c));
    // Lighter really is lighter, in steps.
    let mut last = to_oklab(c).l;
    for step in 1..=5 {
        let l = to_oklab(lighten(c, step as f32 * 0.05)).l;
        assert!(l > last - 1e-4, "step {step}: {l} after {last}");
        last = l;
    }
}

#[test]
fn nan_arguments_change_nothing_instead_of_poisoning_the_colour() {
    let c = UiColor::from_hex("#3366cc").expect("hex");
    assert_eq!(to_hex(lighten(c, f32::NAN)), to_hex(c));
    assert_eq!(to_hex(darken(c, f32::NAN)), to_hex(c));
    assert_eq!(with_alpha(c, f32::NAN), c);
    assert_eq!(to_hex(mix(c, UiColor::WHITE, f32::NAN)), to_hex(c));
    assert!(all_finite(from_oklab(Oklab {
        l: f32::NAN,
        a: f32::NAN,
        b: f32::INFINITY,
        alpha: f32::NAN
    })));
    assert_eq!(with_alpha(c, 7.0).0[3], 1.0);
    assert_eq!(with_alpha(c, -7.0).0[3], 0.0);
}

#[test]
fn mix_ends_are_the_inputs_and_it_is_symmetric_and_clamped() {
    let a = UiColor::from_hex("#cc3322").expect("hex");
    let b = UiColor::from_hex("#2244dd80").expect("hex");
    assert_eq!(to_hex(mix(a, b, 0.0)), to_hex(a));
    assert_eq!(to_hex(mix(a, b, 1.0)), to_hex(b));
    assert_eq!(to_hex(mix(a, b, -4.0)), to_hex(a));
    assert_eq!(to_hex(mix(a, b, 4.0)), to_hex(b));
    assert_eq!(to_hex(mix(a, b, 0.5)), to_hex(mix(b, a, 0.5)));
    let mid = mix(a, b, 0.5);
    assert!(all_finite(mid));
    assert!(
        (mid.0[3] - (a.0[3] + b.0[3]) / 2.0).abs() < 1e-5,
        "alpha mixes too"
    );
    assert_eq!(to_hex(mix(a, a, 0.37)), to_hex(a), "mixing a colour with itself");
}

#[test]
fn out_of_gamut_oklab_keeps_lightness_and_hue_while_chroma_shrinks() {
    let wild = Oklab {
        l: 0.5,
        a: 0.5,
        b: 0.25,
        alpha: 1.0,
    };
    let c = from_oklab(wild);
    assert!(all_finite(c));
    let back = to_oklab(c);
    assert!((back.l - 0.5).abs() < 2e-3, "lightness {}", back.l);
    assert!(back.a.hypot(back.b) < wild.a.hypot(wild.b), "chroma shrank");
    assert!(
        (back.b / back.a - 0.5).abs() < 2e-2,
        "hue kept: {} / {}",
        back.b,
        back.a
    );
    // The extremes of lightness give pure black and white whatever the chroma.
    assert_eq!(
        to_hex(from_oklab(Oklab {
            l: 0.0,
            a: 0.4,
            b: -0.4,
            alpha: 1.0
        })),
        "#000000"
    );
    assert_eq!(
        to_hex(from_oklab(Oklab {
            l: 1.0,
            a: -0.4,
            b: 0.4,
            alpha: 1.0
        })),
        "#ffffff"
    );
    assert_eq!(
        to_hex(from_oklab(Oklab {
            l: 9.0,
            a: 0.0,
            b: 0.0,
            alpha: 1.0
        })),
        "#ffffff"
    );
    assert_eq!(
        to_hex(from_oklab(Oklab {
            l: -9.0,
            a: 0.0,
            b: 0.0,
            alpha: 1.0
        })),
        "#000000"
    );
}

#[test]
fn contrast_ratio_is_symmetric_at_least_one_and_21_for_black_on_white() {
    assert!((contrast_ratio(UiColor::BLACK, UiColor::WHITE) - 21.0).abs() < 1e-4);
    assert_eq!(
        contrast_ratio(UiColor::WHITE, UiColor::BLACK),
        contrast_ratio(UiColor::BLACK, UiColor::WHITE)
    );
    let c = UiColor::from_hex("#778899").expect("hex");
    assert_eq!(contrast_ratio(c, c), 1.0);
    assert!((relative_luminance(UiColor::WHITE) - 1.0).abs() < 1e-6);
    assert_eq!(relative_luminance(UiColor::BLACK), 0.0);
    for r in (0..=255u8).step_by(51) {
        for g in (0..=255u8).step_by(51) {
            let (a, b) = (
                UiColor::from_srgb8(r, g, 0, 255),
                UiColor::from_srgb8(g, r, 255, 255),
            );
            let ratio = contrast_ratio(a, b);
            assert!((1.0..=21.0 + 1e-4).contains(&ratio), "{ratio}");
            assert_eq!(ratio, contrast_ratio(b, a));
        }
    }
}
