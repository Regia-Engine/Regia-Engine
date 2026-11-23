// SPDX-License-Identifier: MPL-2.0
//! Colour operations for theme expressions (`docs/specs/ui-default-design.md` § 6.2): `lighten`,
//! `darken`, `alpha` and `mix` in OKLab, plus the WCAG 2.x contrast ratio and hex output.
//!
//! OKLab is Björn Ottosson's perceptual space ("A perceptual color space for image processing",
//! 2020, public): the two matrices below are the published constants. Lightness operations keep
//! the hue because only `L` moves. Results outside the sRGB gamut are pulled back by scaling the
//! chroma (`a`, `b`) at constant lightness, which keeps the hue instead of clipping channels.

use crate::color::{UiColor, linear_to_srgb8};

/// A colour in OKLab with straight alpha.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklab {
    /// Perceptual lightness, `0.0` (black) to `1.0` (white).
    pub l: f32,
    /// Green–red axis.
    pub a: f32,
    /// Blue–yellow axis.
    pub b: f32,
    /// Alpha, `0.0..=1.0`.
    pub alpha: f32,
}

/// Converts a linear-light colour to OKLab.
pub fn to_oklab(c: UiColor) -> Oklab {
    let [r, g, b, alpha] = c.0.map(f64::from);
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    Oklab {
        l: (0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s) as f32,
        a: (1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s) as f32,
        b: (0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s) as f32,
        alpha: alpha as f32,
    }
}

/// Linear sRGB of an OKLab triple, unclamped.
fn oklab_to_linear(l: f64, a: f64, b: f64) -> [f64; 3] {
    let l_ = l + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = l - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = l - 0.0894841775 * a - 1.2914855480 * b;
    let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
    ]
}

fn in_gamut(rgb: [f64; 3]) -> bool {
    rgb.iter().all(|c| (-1e-6..=1.0 + 1e-6).contains(c))
}

/// Converts OKLab back to a linear-light colour, scaling chroma into the sRGB gamut.
pub fn from_oklab(c: Oklab) -> UiColor {
    // A NaN or infinite component (a NaN in a `.theme.ron` expression) would give a NaN colour:
    // lightness counts as 0, chroma as 0, alpha as opaque.
    let finite = |v: f32, otherwise: f32| if v.is_finite() { v } else { otherwise };
    let l = f64::from(finite(c.l, 0.0).clamp(0.0, 1.0));
    let (a, b) = (f64::from(finite(c.a, 0.0)), f64::from(finite(c.b, 0.0)));
    let mut rgb = oklab_to_linear(l, a, b);
    if !in_gamut(rgb) {
        let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
        for _ in 0..32 {
            let mid = 0.5 * (lo + hi);
            if in_gamut(oklab_to_linear(l, a * mid, b * mid)) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        rgb = oklab_to_linear(l, a * lo, b * lo);
    }
    UiColor([
        rgb[0].clamp(0.0, 1.0) as f32,
        rgb[1].clamp(0.0, 1.0) as f32,
        rgb[2].clamp(0.0, 1.0) as f32,
        finite(c.alpha, 1.0).clamp(0.0, 1.0),
    ])
}

/// Raises OKLab lightness by `amount` (`0..1` lightness units); hue and alpha are kept.
pub fn lighten(c: UiColor, amount: f32) -> UiColor {
    let mut lab = to_oklab(c);
    // A NaN amount moves nothing.
    lab.l = (lab.l + if amount.is_nan() { 0.0 } else { amount }).clamp(0.0, 1.0);
    from_oklab(lab)
}

/// Lowers OKLab lightness by `amount`.
pub fn darken(c: UiColor, amount: f32) -> UiColor {
    lighten(c, -amount)
}

/// Replaces the alpha of `c` (it does not multiply the existing alpha).
pub fn with_alpha(c: UiColor, alpha: f32) -> UiColor {
    // A NaN alpha keeps the colour as it is.
    if alpha.is_nan() {
        return c;
    }
    c.with_alpha(alpha.clamp(0.0, 1.0))
}

/// Interpolates two colours in OKLab, alpha included; `t` is clamped to `0..=1`.
pub fn mix(a: UiColor, b: UiColor, t: f32) -> UiColor {
    let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
    let (x, y) = (to_oklab(a), to_oklab(b));
    from_oklab(Oklab {
        l: x.l + (y.l - x.l) * t,
        a: x.a + (y.a - x.a) * t,
        b: x.b + (y.b - x.b) * t,
        alpha: x.alpha + (y.alpha - x.alpha) * t,
    })
}

/// WCAG 2.x relative luminance of a linear-light colour (alpha ignored).
pub fn relative_luminance(c: UiColor) -> f32 {
    0.2126 * c.0[0] + 0.7152 * c.0[1] + 0.0722 * c.0[2]
}

/// WCAG 2.x contrast ratio `(L1 + 0.05) / (L2 + 0.05)` of two colours, lighter over darker.
/// Alpha is ignored: composite translucent colours first.
pub fn contrast_ratio(a: UiColor, b: UiColor) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Formats a colour as `#rrggbb`, or `#rrggbbaa` when the quantized alpha byte is below 255
/// (sRGB, 8 bits).
pub fn to_hex(c: UiColor) -> String {
    let [r, g, b, a] = c.0;
    let (r, g, b) = (linear_to_srgb8(r), linear_to_srgb8(g), linear_to_srgb8(b));
    let a8 = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    if a8 == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a8:02x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> UiColor {
        UiColor::from_hex(s).expect("test hex")
    }

    /// Per-channel distance of two hex strings, in 8-bit steps.
    fn hex_distance(a: &str, b: &str) -> u32 {
        let ch = |s: &str, i: usize| i64::from(u8::from_str_radix(&s[1 + 2 * i..3 + 2 * i], 16).unwrap_or(0));
        (0..3)
            .map(|i| (ch(a, i) - ch(b, i)).unsigned_abs() as u32)
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn oklab_of_white_and_black() {
        let w = to_oklab(UiColor::WHITE);
        assert!((w.l - 1.0).abs() < 1e-3 && w.a.abs() < 1e-3 && w.b.abs() < 1e-3);
        let k = to_oklab(UiColor::BLACK);
        assert!(k.l.abs() < 1e-6);
    }

    #[test]
    fn oklab_of_red_matches_the_published_value() {
        // Ottosson's reference for sRGB red: L 0.6280, a 0.2249, b 0.1258.
        let r = to_oklab(hex("#ff0000"));
        assert!((r.l - 0.628).abs() < 2e-3, "{r:?}");
        assert!((r.a - 0.2249).abs() < 2e-3, "{r:?}");
        assert!((r.b - 0.1258).abs() < 2e-3, "{r:?}");
    }

    #[test]
    fn oklab_round_trips_within_one_code() {
        for h in ["#3d9df0", "#e5534b", "#171c23", "#ffffff", "#000000", "#9cd3ff"] {
            let back = to_hex(from_oklab(to_oklab(hex(h))));
            assert!(hex_distance(h, &back) <= 1, "{h} -> {back}");
        }
    }

    #[test]
    fn lighten_black_by_half_is_a_hand_computed_grey() {
        // A grey has a = b = 0, so each linear channel is L^3 = 0.125, which encodes to sRGB 0x63.
        let c = lighten(hex("#000000"), 0.5);
        assert_eq!(to_hex(c), "#636363");
    }

    #[test]
    fn darken_is_lighten_with_the_sign_flipped() {
        let base = hex("#3d9df0");
        assert_eq!(to_hex(darken(base, 0.08)), to_hex(lighten(base, -0.08)));
        assert!(hex_distance(&to_hex(darken(base, 0.08)), "#1c84d5") <= 2);
    }

    #[test]
    fn lighten_accent_golden_and_hue_is_kept() {
        let base = hex("#3d9df0");
        let up = lighten(base, 0.06);
        assert!(hex_distance(&to_hex(up), "#5ab0ff") <= 2, "{}", to_hex(up));
        let (a, b) = (to_oklab(base), to_oklab(up));
        assert!((b.l - a.l - 0.06).abs() < 5e-3);
        let hue = |l: Oklab| l.b.atan2(l.a);
        assert!((hue(a) - hue(b)).abs() < 0.03, "hue moved");
    }

    #[test]
    fn lighten_clamps_at_white_and_black() {
        assert_eq!(to_hex(lighten(hex("#ffffff"), 0.3)), "#ffffff");
        assert_eq!(to_hex(darken(hex("#000000"), 0.3)), "#000000");
    }

    #[test]
    fn alpha_replaces_and_quantizes_like_the_spec_table() {
        // `color.selection`: alpha(#3d9df0, 0.28) is #3d9df047 in the spec table (0.28 * 255 = 71.4).
        assert_eq!(to_hex(with_alpha(hex("#3d9df0"), 0.28)), "#3d9df047");
        assert_eq!(to_hex(with_alpha(hex("#3d9df080"), 1.0)), "#3d9df0");
    }

    #[test]
    fn mix_endpoints_and_midpoint() {
        let (a, b) = (hex("#102030"), hex("#e0d0c0"));
        assert_eq!(to_hex(mix(a, b, 0.0)), "#102030");
        assert_eq!(to_hex(mix(a, b, 1.0)), "#e0d0c0");
        // Black to white in OKLab: midpoint lightness 0.5, i.e. the same 0x63 grey as above.
        assert_eq!(to_hex(mix(hex("#000000"), hex("#ffffff"), 0.5)), "#636363");
        // Alpha interpolates too.
        let half = mix(hex("#ffffff00"), hex("#ffffffff"), 0.5);
        assert!((half.0[3] - 0.5).abs() < 1e-5);
    }

    #[test]
    fn contrast_ratio_matches_wcag_extremes_and_symmetry() {
        let r = contrast_ratio(UiColor::WHITE, UiColor::BLACK);
        assert!((r - 21.0).abs() < 0.01, "{r}");
        assert!((contrast_ratio(UiColor::BLACK, UiColor::WHITE) - r).abs() < 1e-5);
        assert!((contrast_ratio(hex("#777777"), hex("#777777")) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn hex_output_round_trips_through_from_hex() {
        for h in ["#0f1318", "#3d9df047", "#000000a6", "#ffffff"] {
            assert_eq!(to_hex(hex(h)), h);
        }
    }
}
