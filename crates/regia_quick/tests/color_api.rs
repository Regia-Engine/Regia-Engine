// SPDX-License-Identifier: MPL-2.0
//! Public colour constructors: alpha pass-through, input clamping, hue wrapping and degenerate
//! inputs. The primaries and reference Oklab values are in the unit tests of `color.rs`.

use regia_core::math::Color;
use regia_quick::color::*;

fn near(a: Color, b: Color) -> bool {
    (a.to_vec4() - b.to_vec4()).abs().max_element() < 2e-3
}

fn srgb_of(c: Color) -> [f32; 3] {
    let s = c.to_srgb();
    [s[0], s[1], s[2]]
}

fn near3(a: [f32; 3], b: [f32; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 2e-3)
}

#[test]
fn alpha_variants_pass_alpha_through_unconverted() {
    assert_eq!(srgba(0.2, 0.4, 0.6, 0.25).a, 0.25);
    assert_eq!(hsla(10.0, 0.5, 0.5, 0.25).a, 0.25);
    assert_eq!(hsva(10.0, 0.5, 0.5, 0.25).a, 0.25);
    assert_eq!(hwba(10.0, 0.2, 0.2, 0.25).a, 0.25);
    assert_eq!(oklaba(0.5, 0.05, 0.05, 0.25).a, 0.25);
    assert_eq!(oklcha(0.5, 0.05, 120.0, 0.25).a, 0.25);
    assert_eq!(linear_rgba(0.1, 0.2, 0.3, 0.25).a, 0.25);
    // Opaque constructors are exactly alpha 1.
    assert_eq!(srgb(0.2, 0.4, 0.6).a, 1.0);
    assert_eq!(hsl(10.0, 0.5, 0.5).a, 1.0);
    assert_eq!(hsv(10.0, 0.5, 0.5).a, 1.0);
    assert_eq!(hwb(10.0, 0.2, 0.2).a, 1.0);
    assert_eq!(oklab(0.5, 0.05, 0.05).a, 1.0);
    assert_eq!(oklch(0.5, 0.05, 120.0).a, 1.0);
    assert_eq!(linear_rgb(0.1, 0.2, 0.3).a, 1.0);
}

#[test]
fn byte_constructors_scale_by_255_and_alpha_is_linear() {
    assert!(near(srgb_u8(255, 255, 255), Color::WHITE));
    assert!(near(srgb_u8(0, 0, 0), Color::BLACK));
    let c = srgba_u8(10, 20, 30, 51);
    assert!(
        (c.a - 0.2).abs() < 1e-6,
        "alpha is a / 255, not gamma-converted: {}",
        c.a
    );
    assert!(near3(srgb_of(c), [10.0 / 255.0, 20.0 / 255.0, 30.0 / 255.0]));
}

#[test]
fn srgb_encoding_is_applied_but_linear_is_not() {
    // Mid sRGB grey is darker in linear light; the linear constructor keeps its numbers.
    let mid = srgb(0.5, 0.5, 0.5);
    assert!(mid.r > 0.2 && mid.r < 0.23, "{}", mid.r);
    assert_eq!(linear_rgb(0.5, 0.5, 0.5).r, 0.5);
}

#[test]
fn hue_wraps_in_both_directions() {
    assert!(near(hsl(450.0, 1.0, 0.5), hsl(90.0, 1.0, 0.5)));
    assert!(near(hsl(-270.0, 1.0, 0.5), hsl(90.0, 1.0, 0.5)));
    assert!(near(hsl(360.0, 1.0, 0.5), hsl(0.0, 1.0, 0.5)));
    assert!(near(hsv(-120.0, 1.0, 1.0), hsv(240.0, 1.0, 1.0)));
    assert!(near(hwb(720.0, 0.1, 0.1), hwb(0.0, 0.1, 0.1)));
    assert!(near(oklch(0.6, 0.1, 360.0), oklch(0.6, 0.1, 0.0)));
    assert!(near(oklch(0.6, 0.1, -90.0), oklch(0.6, 0.1, 270.0)));
}

#[test]
fn out_of_range_saturation_and_lightness_are_clamped() {
    assert!(near(hsl(30.0, 5.0, 0.5), hsl(30.0, 1.0, 0.5)));
    assert!(near(hsl(30.0, -2.0, 0.5), hsl(30.0, 0.0, 0.5)));
    assert!(near(hsl(30.0, 1.0, -3.0), Color::BLACK));
    assert!(near(hsl(30.0, 1.0, 3.0), Color::WHITE));
    assert!(near(hsv(30.0, 9.0, 1.0), hsv(30.0, 1.0, 1.0)));
    assert!(near(hsv(30.0, 1.0, 9.0), hsv(30.0, 1.0, 1.0)));
    assert!(near(hsv(30.0, 1.0, -1.0), Color::BLACK));
    assert!(near(hwb(30.0, -1.0, -1.0), hwb(30.0, 0.0, 0.0)));
}

#[test]
fn hsv_degenerate_corners_do_not_produce_nan() {
    let black = hsv(123.0, 1.0, 0.0);
    assert!(near(black, Color::BLACK), "{black:?}");
    let grey = hsv(123.0, 0.0, 0.5);
    assert!(near3(srgb_of(grey), [0.5, 0.5, 0.5]), "{grey:?}");
    let white = hsv(123.0, 0.0, 1.0);
    assert!(near(white, Color::WHITE), "{white:?}");
    for c in [black, grey, white, hsl(0.0, 0.0, 0.0), hsl(0.0, 1.0, 1.0)] {
        assert!(c.to_vec4().to_array().iter().all(|v| v.is_finite()), "{c:?}");
    }
}

#[test]
fn hwb_with_white_plus_black_over_one_is_the_normalized_grey() {
    assert!(near3(srgb_of(hwb(0.0, 0.6, 0.6)), [0.5, 0.5, 0.5]));
    assert!(near3(srgb_of(hwb(200.0, 1.0, 0.0)), [1.0, 1.0, 1.0]));
    assert!(near3(srgb_of(hwb(200.0, 0.0, 1.0)), [0.0, 0.0, 0.0]));
    // Below the limit the hue shows through: red with 20 % white and 20 % black.
    assert!(near3(srgb_of(hwb(0.0, 0.2, 0.2)), [0.8, 0.2, 0.2]));
}

#[test]
fn oklch_with_no_chroma_ignores_hue() {
    assert!(near(oklch(0.5, 0.0, 0.0), oklch(0.5, 0.0, 200.0)));
    assert!(near(oklch(0.5, 0.0, 0.0), oklab(0.5, 0.0, 0.0)));
    let c = oklch(0.5, 0.0, 77.0);
    assert!(
        (c.r - c.g).abs() < 1e-3 && (c.g - c.b).abs() < 1e-3,
        "neutral: {c:?}"
    );
}
