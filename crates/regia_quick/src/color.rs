// SPDX-License-Identifier: MPL-2.0
//! Colour constructors for the draw API: sRGB, linear, HSL / HSV / HWB (sRGB-based, hue in
//! degrees) and Oklab / Oklch. Every function returns Regia Engine's linear [`Color`].
//!
//! HSL, HSV and HWB follow the CSS Color 4 definitions (<https://www.w3.org/TR/css-color-4/>);
//! Oklab follows Björn Ottosson, "A perceptual color space for image processing" (2020,
//! <https://bottosson.github.io/posts/oklab/>).

use regia_core::math::Color;

/// Opaque sRGB-encoded colour (`0..1` channels).
#[must_use]
pub fn srgb(r: f32, g: f32, b: f32) -> Color {
    Color::from_srgb(r, g, b, 1.0)
}

/// sRGB-encoded colour with alpha.
#[must_use]
pub fn srgba(r: f32, g: f32, b: f32, a: f32) -> Color {
    Color::from_srgb(r, g, b, a)
}

/// Opaque sRGB colour from bytes.
#[must_use]
pub fn srgb_u8(r: u8, g: u8, b: u8) -> Color {
    Color::from_srgb_u8(r, g, b, 255)
}

/// sRGB colour with alpha from bytes.
#[must_use]
pub fn srgba_u8(r: u8, g: u8, b: u8, a: u8) -> Color {
    Color::from_srgb_u8(r, g, b, a)
}

/// Opaque linear colour.
#[must_use]
pub fn linear_rgb(r: f32, g: f32, b: f32) -> Color {
    Color::new(r, g, b, 1.0)
}

/// Linear colour with alpha.
#[must_use]
pub fn linear_rgba(r: f32, g: f32, b: f32, a: f32) -> Color {
    Color::new(r, g, b, a)
}

/// Hue (degrees) to the sRGB channels of a fully saturated, mid-light colour (CSS `hsl()`).
fn hue_rgb(hue_degrees: f32, s: f32, l: f32) -> [f32; 3] {
    let h = hue_degrees.rem_euclid(360.0);
    let f = |n: f32| {
        let k = (n + h / 30.0) % 12.0;
        let a = s * l.min(1.0 - l);
        l - a * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0)
    };
    [f(0.0), f(8.0), f(4.0)]
}

/// Opaque colour from hue (degrees), saturation and lightness (`0..1`), sRGB-based.
#[must_use]
pub fn hsl(hue: f32, s: f32, l: f32) -> Color {
    hsla(hue, s, l, 1.0)
}

/// HSL with alpha.
#[must_use]
pub fn hsla(hue: f32, s: f32, l: f32, a: f32) -> Color {
    let [r, g, b] = hue_rgb(hue, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0));
    srgba(r, g, b, a)
}

/// Opaque colour from hue (degrees), saturation and value / brightness (`0..1`), sRGB-based.
#[must_use]
pub fn hsv(hue: f32, s: f32, v: f32) -> Color {
    hsva(hue, s, v, 1.0)
}

/// HSV with alpha.
#[must_use]
pub fn hsva(hue: f32, s: f32, v: f32, a: f32) -> Color {
    let (s, v) = (s.clamp(0.0, 1.0), v.clamp(0.0, 1.0));
    let l = v * (1.0 - s / 2.0);
    let sl = if l <= 0.0 || l >= 1.0 {
        0.0
    } else {
        (v - l) / l.min(1.0 - l)
    };
    hsla(hue, sl, l, a)
}

/// Opaque colour from hue (degrees), whiteness and blackness (`0..1`), sRGB-based.
#[must_use]
pub fn hwb(hue: f32, w: f32, b: f32) -> Color {
    hwba(hue, w, b, 1.0)
}

/// HWB with alpha.
#[must_use]
pub fn hwba(hue: f32, w: f32, b: f32, a: f32) -> Color {
    let (w, b) = (w.clamp(0.0, 1.0), b.clamp(0.0, 1.0));
    if w + b >= 1.0 {
        let grey = w / (w + b);
        return srgba(grey, grey, grey, a);
    }
    let [r, g, bl] = hue_rgb(hue, 1.0, 0.5);
    let k = 1.0 - w - b;
    srgba(r * k + w, g * k + w, bl * k + w, a)
}

/// Opaque colour from Oklab lightness `l` (`0..1`) and opponent axes `a`, `b`.
#[must_use]
pub fn oklab(l: f32, a: f32, b: f32) -> Color {
    oklaba(l, a, b, 1.0)
}

/// Oklab with alpha.
#[must_use]
pub fn oklaba(l: f32, a: f32, b: f32, alpha: f32) -> Color {
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    Color::new(
        4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_94 * s3,
        -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_38 * s3,
        -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3,
        alpha,
    )
}

/// Opaque colour from Oklch lightness, chroma and hue (degrees).
#[must_use]
pub fn oklch(l: f32, c: f32, hue: f32) -> Color {
    oklcha(l, c, hue, 1.0)
}

/// Oklch with alpha.
#[must_use]
pub fn oklcha(l: f32, c: f32, hue: f32, alpha: f32) -> Color {
    let (sin, cos) = hue.to_radians().sin_cos();
    oklaba(l, c * cos, c * sin, alpha)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: Color, b: Color) -> bool {
        (a.to_vec4() - b.to_vec4()).abs().max_element() < 2e-3
    }

    #[test]
    fn hue_models_hit_the_primaries() {
        assert!(near(hsl(0.0, 1.0, 0.5), Color::RED));
        assert!(near(hsl(120.0, 1.0, 0.5), Color::GREEN));
        assert!(near(hsl(240.0, 1.0, 0.5), Color::BLUE));
        assert!(near(hsv(60.0, 1.0, 1.0), Color::YELLOW));
        assert!(near(hsv(0.0, 0.0, 1.0), Color::WHITE));
        assert!(near(hsl(-60.0, 1.0, 0.5), Color::MAGENTA));
        assert!(near(hwb(180.0, 0.0, 0.0), Color::CYAN));
        assert!(near(hwb(0.0, 1.0, 1.0), srgb(0.5, 0.5, 0.5)));
        assert!(near(srgb_u8(255, 0, 0), Color::RED));
        assert_eq!(linear_rgba(0.1, 0.2, 0.3, 0.4), Color::new(0.1, 0.2, 0.3, 0.4));
    }

    #[test]
    fn oklab_white_black_and_red() {
        assert!(near(oklab(1.0, 0.0, 0.0), Color::WHITE));
        assert!(near(oklab(0.0, 0.0, 0.0), Color::BLACK));
        // sRGB red in Oklab (Ottosson's reference values).
        assert!(near(oklab(0.627_955, 0.224_863, 0.125_846), Color::RED));
        assert!(near(oklch(0.627_955, 0.257_683, 29.233_885), Color::RED));
    }
}
