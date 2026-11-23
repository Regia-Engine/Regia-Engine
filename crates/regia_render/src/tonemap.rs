// SPDX-License-Identifier: MPL-2.0
//! AgX display transform and the default look — CPU mirror of `utils.wgsl`.
//!
//! ## Provenance
//!
//! - **AgX**: Troy Sobotka's publicly documented display rendering transform (public
//!   blog posts and colour-science presentations).
//! - **Minimal form** (inset matrix → log2 encode over `[−12.47393, +4.026069]` EV →
//!   sixth-order sigmoid fit → outset matrix): B. Wrensch, "Minimal AgX Implementation"
//!   (2023, public blog post). The polynomial fits the AgX sigmoid **in the normalised
//!   log2 domain**, not on linear input.
//!
//! No GPL-licensed code (including Blender's AgX implementation) was consulted.
//!
//! ## Why this was rewritten (2026-10-06)
//!
//! The previous port fed *linear* values straight into the log-domain polynomial. That
//! curve mapped middle grey (0.18) to display 0.17, crushed everything below 0.03 linear
//! to black and clipped everything above 1.0 to white — the "washed-out highlights,
//! black interiors" look of the DO27 beauty frames. The correct transform maps 0.18 to
//! ≈ 0.50 and rolls highlights off up to +4 EV over middle grey.

/// AgX inset matrix (rows), applied to linear Rec.709 before the log encode.
const INSET: [[f32; 3]; 3] = [
    [0.842_479_06, 0.078_433_6, 0.079_223_745],
    [0.042_328_242, 0.878_468_64, 0.079_166_13],
    [0.042_375_655, 0.078_433_6, 0.879_143],
];

/// AgX outset matrix (rows), the inverse of [`INSET`], applied after the sigmoid.
const OUTSET: [[f32; 3]; 3] = [
    [1.196_879, -0.098_020_88, -0.099_029_74],
    [-0.052_896_85, 1.151_903_1, -0.098_961_18],
    [-0.052_971_635, -0.098_043_45, 1.151_073_7],
];

const MIN_EV: f32 = -12.473_93;
const MAX_EV: f32 = 4.026_069;

fn mul(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|r| m[r][0] * v[0] + m[r][1] * v[1] + m[r][2] * v[2])
}

fn sigmoid(x: f32) -> f32 {
    let x2 = x * x;
    let x4 = x2 * x2;
    15.5 * x4 * x2 - 40.14 * x4 * x + 31.96 * x4 - 6.868 * x2 * x + 0.4298 * x2 + 0.1191 * x - 0.00232
}

/// AgX display rendering transform.
///
/// Input: linear, exposed, scene-referred Rec.709 RGB. Output: display-encoded RGB
/// (≈ 2.2 gamma) in `[0, 1]`, ready for an 8-bit non-sRGB target.
///
/// ## Properties
///
/// - Middle grey `0.18` maps to ≈ `0.50`.
/// - Output is finite and in `[0, 1]` for any input (negatives and NaN-free infinities
///   clamp to the ends of the log range).
/// - Monotonic per channel for neutral input.
#[must_use]
pub fn tonemap_agx(rgb: [f32; 3]) -> [f32; 3] {
    let clean = rgb.map(|c| if c.is_finite() { c.max(1.0e-10) } else { 1.0e30 });
    let inset = mul(&INSET, clean);
    let encoded = inset.map(|c| {
        let log = c.max(1.0e-10).log2().clamp(MIN_EV, MAX_EV);
        sigmoid((log - MIN_EV) / (MAX_EV - MIN_EV))
    });
    mul(&OUTSET, encoded).map(|c| c.clamp(0.0, 1.0))
}

/// Inverse of [`tonemap_agx`]: the exposed scene-referred RGB that AgX prints as
/// `display` (display-encoded, `[0, 1]`). Exact up to the clamps (the matrices are
/// inverses of each other and the sigmoid is monotonic on `[0, 1]`, inverted by
/// bisection); values AgX cannot reach saturate at the ends of its log range.
///
/// For colours given in display terms that a shader mixes in before the tonemap (a
/// flat fog colour, a UI-picked tint), so they print as given instead of being
/// tonemapped a second time.
#[must_use]
pub fn inverse_tonemap_agx(display: [f32; 3]) -> [f32; 3] {
    let d = display.map(|c| if c.is_finite() { c.clamp(0.0, 1.0) } else { 0.0 });
    let encoded = mul(&INSET, d);
    let low = sigmoid(0.0);
    let high = sigmoid(1.0);
    let log_norm = encoded.map(|target| {
        let target = target.clamp(low, high);
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        for _ in 0..24 {
            let mid = 0.5 * (lo + hi);
            if sigmoid(mid) < target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    });
    let inset_space = log_norm.map(|v| (v * (MAX_EV - MIN_EV) + MIN_EV).exp2());
    mul(&OUTSET, inset_space).map(|c| c.max(0.0))
}

/// The default look applied after [`tonemap_agx`] (mirror of `display_look` in
/// `utils.wgsl`): an S-curve around display mid grey and a saturation scale around
/// Rec.709 luma. `contrast = saturation = 1` is the identity.
#[must_use]
pub fn display_look(display: [f32; 3], contrast: f32, saturation: f32) -> [f32; 3] {
    let curved = display.map(|v| {
        let v = v.clamp(0.0, 1.0);
        if v < 0.5 {
            0.5 * (v * 2.0).powf(contrast)
        } else {
            1.0 - 0.5 * ((1.0 - v) * 2.0).powf(contrast)
        }
    });
    let luma = 0.2126 * curved[0] + 0.7152 * curved[1] + 0.0722 * curved[2];
    curved.map(|c| (luma + (c - luma) * saturation).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middle_grey_lands_mid_screen() {
        let g = tonemap_agx([0.18; 3]);
        for c in g {
            assert!((c - 0.5).abs() < 0.03, "middle gray -> {c}");
        }
    }

    #[test]
    fn shadows_are_not_crushed_and_highlights_roll_off() {
        // The old linear-input fit sent 0.01 to 0 and 1.0 to 1.0 (clip).
        let dark = tonemap_agx([0.01; 3])[1];
        assert!(dark > 0.05, "0.01 linear must stay visible, got {dark}");
        let one = tonemap_agx([1.0; 3])[1];
        assert!(one < 0.9, "1.0 linear must not clip, got {one}");
        let four = tonemap_agx([4.0; 3])[1];
        assert!(four > one && four < 1.0, "highlight roll-off {four}");
    }

    #[test]
    fn output_is_total_and_in_range() {
        for &val in &[
            -1.0,
            0.0,
            1.0e-6,
            0.5,
            1.0,
            10.0,
            1.0e4,
            1.0e38,
            f32::INFINITY,
            f32::NAN,
        ] {
            for c in tonemap_agx([val, val * 0.5, val * 0.25]) {
                assert!(c.is_finite() && (0.0..=1.0).contains(&c), "{val} -> {c}");
            }
        }
    }

    #[test]
    fn brighter_input_maps_higher() {
        let mut last = -1.0;
        for &val in &[0.001, 0.01, 0.1, 0.18, 0.5, 1.0, 2.0, 8.0] {
            let y = tonemap_agx([val; 3])[1];
            assert!(y >= last, "{val} -> {y} not monotonic");
            last = y;
        }
    }

    #[test]
    fn matrices_are_inverse() {
        for col in 0..3 {
            let mut e = [0.0; 3];
            e[col] = 1.0;
            let round = mul(&OUTSET, mul(&INSET, e));
            for (row, value) in round.iter().enumerate() {
                let expect = if row == col { 1.0 } else { 0.0 };
                assert!((value - expect).abs() < 1.0e-4, "({row},{col}) {value}");
            }
        }
    }

    #[test]
    fn inverse_round_trips_display_colours() {
        for display in [
            [0.5, 0.5, 0.5],
            [0.79, 0.84, 0.9],
            [0.3, 0.6, 0.2],
            [0.95, 0.9, 0.85],
        ] {
            let scene = inverse_tonemap_agx(display);
            let back = tonemap_agx(scene);
            for c in 0..3 {
                assert!(
                    (back[c] - display[c]).abs() < 2.0e-3,
                    "{display:?} -> {scene:?} -> {back:?}"
                );
            }
        }
    }

    #[test]
    fn look_keeps_mid_grey_and_ends() {
        let c = crate::DEFAULT_LOOK_CONTRAST;
        let s = crate::DEFAULT_LOOK_SATURATION;
        assert!((display_look([0.5; 3], c, s)[0] - 0.5).abs() < 1.0e-5);
        assert_eq!(display_look([0.0; 3], c, s), [0.0; 3]);
        assert!((display_look([1.0; 3], c, s)[0] - 1.0).abs() < 1.0e-5);
        assert!(
            display_look([0.25; 3], c, s)[0] < 0.25,
            "contrast deepens shadows"
        );
        assert_eq!(display_look([0.3, 0.6, 0.9], 1.0, 1.0), [0.3, 0.6, 0.9]);
    }

    #[test]
    fn look_defaults_match_the_shaders() {
        let utils = include_str!("../shaders/utils.wgsl");
        let contrast = format!(
            "const DEFAULT_LOOK_CONTRAST: f32 = {};",
            crate::DEFAULT_LOOK_CONTRAST
        );
        let saturation = format!(
            "const DEFAULT_LOOK_SATURATION: f32 = {};",
            crate::DEFAULT_LOOK_SATURATION
        );
        assert!(utils.contains(&contrast), "utils.wgsl must declare `{contrast}`");
        assert!(
            utils.contains(&saturation),
            "utils.wgsl must declare `{saturation}`"
        );
    }
}
