// SPDX-License-Identifier: MPL-2.0
//! Weighted blended order-independent transparency for translucent meshes (ADR-2065,
//! `book/agents/oit.md`).
//!
//! Sorting whole meshes back to front fails on what no mesh order can draw: two surfaces that cross
//! each other, or one mesh wrapped around another. Weighted blended transparency (McGuire and
//! Bavoil, "Weighted Blended Order-Independent Transparency", JCGT 2(2), 2013) needs no order. Each
//! fragment adds its premultiplied light and its coverage to an accumulation target, both scaled by
//! a weight that falls with distance, and multiplies a revealage target by what it lets through.
//! A full-screen composite then lays the weighted average of the light over the image, in
//! proportion to how much of the background the fragments covered.
//!
//! Coverage is exact: the revealage is the true product of what each layer lets through, in any
//! order. A single layer is exact too. The color of several layers is an approximation that favors
//! nearer ones.
//!
//! This module holds the weight, the reactive mask's value and a CPU model of the composite, which
//! the tests and `shaders/oit_composite.wgsl` agree with. The GPU side is `frame/oit_gpu.rs`.

/// Format of the accumulation target: premultiplied light and coverage, both weighted.
pub const ACCUM_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// Format of the revealage target: the share of the background that still shows.
pub const REVEALAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R16Float;
/// Format of the reactive mask the composite writes for the temporal upscalers (red, 0..=1).
pub const REACTIVE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

/// The reactive mask's value on a pixel the translucent layers cover completely. Below 1 so the
/// history still removes jitter on a nearly solid layer (our own choice, ADR-2065).
pub const REACTIVE_SCALE: f32 = 0.9;

/// Largest weight the depth term gives (McGuire and Bavoil 2013, equation 7).
pub const WEIGHT_MAX: f32 = 3.0e3;
/// Smallest weight the depth term gives (McGuire and Bavoil 2013, equation 7).
pub const WEIGHT_MIN: f32 = 1.0e-2;
/// Largest weighted light one fragment adds. The accumulation target is a half float (at most
/// 65504): a bright fragment's weight is held down so ten of them still add up without reaching it.
pub const WEIGHTED_LIGHT_MAX: f32 = 6.0e3;
/// The coverage floor that keeps a fragment's share of the average when its alpha is close to 0.
pub const WEIGHT_ALPHA_FLOOR: f32 = 1.0e-2;
/// Smallest weighted coverage of a covering fragment: above a half float's smallest normal value
/// (6.1e-5). A far, faint fragment would otherwise add a denormal, and hardware that flushes those
/// while blending would keep its revealage with no color to show for it: a black veil.
pub const WEIGHTED_COVERAGE_MIN: f32 = 1.0e-4;
/// Smallest coverage of a premultiplied fragment that has light: a texel with alpha 0 only adds
/// light, and without coverage the composite would have nothing to lay it over the image with.
pub const PREMULTIPLIED_MIN_COVERAGE: f32 = 1.0 / 255.0;

/// The depth term of the weight at `distance` world units from the eye (McGuire and Bavoil 2013,
/// equation 7). The paper uses the view-space depth; the radial distance needs no projection, so a
/// perspective and an orthographic view share it, and it does not change as the camera turns.
#[must_use]
pub fn depth_weight(distance: f32) -> f32 {
    let near = distance / 5.0;
    let far = distance / 200.0;
    let term = 10.0 / (1.0e-5 + near * near + far * far * far * far * far * far);
    term.clamp(WEIGHT_MIN, WEIGHT_MAX)
}

/// One translucent fragment as the forward shader outputs it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OitFragment {
    /// Linear color: straight for an alpha-blended material, premultiplied for a premultiplied one.
    pub color: [f32; 3],
    /// Alpha.
    pub alpha: f32,
    /// The material's alpha mode is premultiplied.
    pub premultiplied: bool,
    /// Distance from the eye in world units.
    pub distance: f32,
}

impl OitFragment {
    /// What the fragment adds to the accumulation target and its coverage: `([r, g, b, a], coverage)`.
    #[must_use]
    pub fn accumulate(self) -> ([f32; 4], f32) {
        let alpha = self.alpha.clamp(0.0, 1.0);
        let scale = if self.premultiplied { 1.0 } else { alpha };
        let light = self.color.map(|channel| channel.max(0.0) * scale);
        let peak = light[0].max(light[1]).max(light[2]);
        let coverage = if self.premultiplied {
            alpha.max(peak.min(PREMULTIPLIED_MIN_COVERAGE))
        } else {
            alpha
        };
        let weight = (coverage.max(WEIGHT_ALPHA_FLOOR) * depth_weight(self.distance))
            .max(WEIGHTED_COVERAGE_MIN / coverage.max(1.0e-6))
            .min(WEIGHTED_LIGHT_MAX / peak.max(1.0e-5));
        (
            [
                light[0] * weight,
                light[1] * weight,
                light[2] * weight,
                coverage * weight,
            ],
            coverage,
        )
    }
}

/// The composite of `layers` over `background`, in any order: the weighted average of their light
/// over the image, in proportion to what they covered.
#[must_use]
pub fn composite(background: [f32; 3], layers: &[OitFragment]) -> [f32; 3] {
    let mut accum = [0.0f32; 4];
    let mut revealage = 1.0f32;
    for layer in layers {
        let (added, coverage) = layer.accumulate();
        for (sum, value) in accum.iter_mut().zip(added) {
            *sum += value;
        }
        revealage *= 1.0 - coverage;
    }
    if accum[3] <= 0.0 && revealage >= 1.0 {
        return background;
    }
    let divisor = accum[3].max(1.0e-7);
    let covered = 1.0 - revealage;
    [
        accum[0] / divisor * covered + background[0] * revealage,
        accum[1] / divisor * covered + background[1] * revealage,
        accum[2] / divisor * covered + background[2] * revealage,
    ]
}

/// The reactive mask's value on a pixel with this revealage.
#[must_use]
pub fn reactive_from_revealage(revealage: f32) -> f32 {
    (1.0 - revealage.clamp(0.0, 1.0)) * REACTIVE_SCALE
}

/// The two targets of the accumulation pass: the sum, and the product of what is let through.
pub(crate) fn accum_targets() -> [Option<wgpu::ColorTargetState>; 2] {
    let add = wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    };
    let let_through = wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::OneMinusSrc,
        operation: wgpu::BlendOperation::Add,
    };
    [
        Some(wgpu::ColorTargetState {
            format: ACCUM_FORMAT,
            blend: Some(wgpu::BlendState {
                color: add,
                alpha: add,
            }),
            write_mask: wgpu::ColorWrites::ALL,
        }),
        Some(wgpu::ColorTargetState {
            format: REVEALAGE_FORMAT,
            blend: Some(wgpu::BlendState {
                color: let_through,
                alpha: let_through,
            }),
            write_mask: wgpu::ColorWrites::RED,
        }),
    ]
}

/// What the last main-view frame did with order-independent transparency (`render.oit`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OitStats {
    /// The frame's profile asks ([`crate::RenderProfile::order_independent_transparency`]).
    pub requested: bool,
    /// Translucent draws that went through the accumulation pass this frame.
    pub drawn: usize,
    /// Visible draws that could have gone through it but drew sorted this frame: their pipelines
    /// are still building, or the frame cannot run it (`reason`).
    pub fallback: usize,
    /// Visible blended draws that keep the sorted run on a frame that ran the pass: glass and other
    /// transmissive draws, additive materials, x-ray overrides, masked instances, thin films and
    /// hair.
    pub sorted: usize,
    /// Frames this renderer has composited.
    pub passes: u64,
    /// The share of the image the composite covered, in permille of its pixels.
    pub covered_permille: u32,
    /// The frame wrote a reactive mask for the upscaler.
    pub reactive_mask: bool,
    /// Why the frame did not run the pass: `profile_off`, `no_material`, `no_hdr_target`, `msaa`,
    /// `forward_lite`, `no_independent_blend`, `pipelines_building` or `no_visible_draw`. `None`
    /// when it ran.
    pub reason: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(color: [f32; 3], alpha: f32, distance: f32) -> OitFragment {
        OitFragment {
            color,
            alpha,
            premultiplied: false,
            distance,
        }
    }

    fn close(a: [f32; 3], b: [f32; 3], tolerance: f32) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tolerance)
    }

    fn over(background: [f32; 3], front: OitFragment) -> [f32; 3] {
        let scale = if front.premultiplied { 1.0 } else { front.alpha };
        [
            front.color[0] * scale + background[0] * (1.0 - front.alpha),
            front.color[1] * scale + background[1] * (1.0 - front.alpha),
            front.color[2] * scale + background[2] * (1.0 - front.alpha),
        ]
    }

    /// A single layer is the plain "over" operator, straight or premultiplied, near or far.
    #[test]
    fn a_single_layer_is_exact() {
        let background = [0.2, 0.4, 0.1];
        for distance in [0.05, 1.0, 30.0, 5_000.0] {
            for alpha in [0.02, 0.3, 0.75, 1.0] {
                let straight = layer([0.9, 0.3, 0.6], alpha, distance);
                assert!(
                    close(
                        composite(background, &[straight]),
                        over(background, straight),
                        1e-5
                    ),
                    "straight, {alpha} at {distance}"
                );
                let premultiplied = OitFragment {
                    color: [0.9 * alpha, 0.3 * alpha, 0.6 * alpha],
                    premultiplied: true,
                    ..straight
                };
                assert!(
                    close(
                        composite(background, &[premultiplied]),
                        over(background, premultiplied),
                        1e-5
                    ),
                    "premultiplied, {alpha} at {distance}"
                );
            }
        }
    }

    /// The order of the layers changes nothing, whatever the depths.
    #[test]
    fn the_order_does_not_matter() {
        let background = [0.1, 0.1, 0.3];
        let a = layer([1.0, 0.0, 0.0], 0.5, 2.0);
        let b = layer([0.0, 1.0, 0.0], 0.25, 3.0);
        let c = layer([0.0, 0.0, 1.0], 0.6, 8.0);
        let first = composite(background, &[a, b, c]);
        for order in [[c, b, a], [b, a, c], [a, c, b], [b, c, a], [c, a, b]] {
            assert!(close(composite(background, &order), first, 1e-5));
        }
    }

    /// How much of the background shows is the exact product, whatever the colors and depths.
    #[test]
    fn the_coverage_is_the_exact_product() {
        let layers = [
            layer([0.0; 3], 0.5, 1.0),
            layer([0.0; 3], 0.25, 40.0),
            layer([0.0; 3], 0.8, 400.0),
        ];
        let shown = composite([1.0; 3], &layers);
        let exact = 0.5 * 0.75 * 0.2;
        assert!(close(shown, [exact; 3], 1e-6), "{shown:?}");
    }

    /// Layers of one color composite to exactly what sorted blending gives.
    #[test]
    fn layers_of_one_color_match_sorted_blending() {
        let background = [0.05, 0.3, 0.6];
        let color = [0.8, 0.7, 0.2];
        let near = layer(color, 0.4, 3.0);
        let far = layer(color, 0.7, 9.0);
        let sorted = over(over(background, far), near);
        assert!(close(composite(background, &[far, near]), sorted, 1e-5));
    }

    /// The nearer of two layers counts for more, as sorted blending's front layer does.
    #[test]
    fn the_nearer_layer_counts_for_more() {
        let red = [1.0, 0.0, 0.0];
        let blue = [0.0, 0.0, 1.0];
        let shown = composite([0.0; 3], &[layer(red, 0.5, 2.0), layer(blue, 0.5, 8.0)]);
        assert!(shown[0] > 2.0 * shown[2], "{shown:?}");
        let far_red = composite([0.0; 3], &[layer(red, 0.5, 8.0), layer(blue, 0.5, 2.0)]);
        assert!(far_red[2] > 2.0 * far_red[0], "{far_red:?}");
    }

    /// The depth term stays between its bounds and falls with distance.
    #[test]
    fn the_depth_term_falls_with_distance_inside_its_bounds() {
        let mut last = f32::MAX;
        for step in 0..200 {
            let distance = 0.01 * 1.1f32.powi(step);
            let weight = depth_weight(distance);
            assert!(
                (WEIGHT_MIN..=WEIGHT_MAX).contains(&weight),
                "{distance}: {weight}"
            );
            assert!(weight <= last, "{distance}: {weight} after {last}");
            last = weight;
        }
        assert_eq!(depth_weight(0.0), WEIGHT_MAX);
        assert_eq!(depth_weight(1.0e6), WEIGHT_MIN);
    }

    /// A bright near fragment stays inside the half float, so ten of them still add up.
    #[test]
    fn bright_fragments_stay_inside_the_half_float() {
        let (added, _) = layer([6.0e4, 1.0, 1.0], 1.0, 0.01).accumulate();
        assert!(added[0] <= WEIGHTED_LIGHT_MAX * 1.001, "{added:?}");
        assert!(10.0 * added[0] < 65504.0);
        // Its share of the average keeps its color: the ratio of light to coverage is unchanged.
        assert!((added[0] / added[3] - 6.0e4).abs() < 1.0, "{added:?}");
    }

    /// A far, faint fragment stays above the half float's denormals, and stays exact.
    #[test]
    fn a_far_faint_fragment_is_not_a_denormal() {
        for distance in [160.0, 1_000.0, 50_000.0] {
            for alpha in [0.004, 0.02, 0.05, 0.078] {
                let fragment = layer([0.5, 0.3, 0.2], alpha, distance);
                let (added, _) = fragment.accumulate();
                assert!(added[3] >= 6.1e-5, "{alpha} at {distance}: {added:?}");
                let background = [0.2, 0.4, 0.1];
                assert!(close(
                    composite(background, &[fragment]),
                    over(background, fragment),
                    1e-5
                ));
            }
        }
    }

    /// A premultiplied texel with alpha 0 adds its light; a black one covers nothing.
    #[test]
    fn a_premultiplied_texel_with_no_alpha_still_adds_light() {
        let glow = OitFragment {
            color: [0.5, 0.25, 0.0],
            alpha: 0.0,
            premultiplied: true,
            distance: 3.0,
        };
        let shown = composite([0.1; 3], &[glow]);
        assert!(close(shown, [0.6, 0.35, 0.1], 2e-3), "{shown:?}");
        let black = OitFragment {
            color: [0.0; 3],
            ..glow
        };
        assert_eq!(composite([0.1; 3], &[black]), [0.1; 3]);
    }

    /// The mask is the covered share, scaled, and 0 where nothing is covered.
    #[test]
    fn the_reactive_mask_follows_the_coverage() {
        assert_eq!(reactive_from_revealage(1.0), 0.0);
        assert!((reactive_from_revealage(0.0) - REACTIVE_SCALE).abs() < 1e-6);
        assert!((reactive_from_revealage(0.5) - 0.45).abs() < 1e-6);
        assert_eq!(reactive_from_revealage(1.5), 0.0);
    }
}
