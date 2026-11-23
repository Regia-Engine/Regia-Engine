// SPDX-License-Identifier: MPL-2.0
//! CPU image arithmetic of the review kit: indirect isolation, the sky fill before scoring, the
//! interior mask and the leak metric. Pure functions on row-major linear RGB planes.
//!
//! Callers must supply aligned, nonempty images with one element per pixel in every plane.
//! The command checks dimensions and lengths at its input boundary before calling these
//! source-compatible helpers. These functions do not implement cropping or resizing.

use super::views::WorldBox;

/// How far outside the interior box a first hit may lie and still count as interior (its walls,
/// floor and ceiling sit on the box faces), metres.
pub const INTERIOR_MARGIN: f32 = 0.05;

/// The leak metric's floor on the reference's interior level, as a fraction of the reference's
/// mean luminance over every geometry pixel of the frame: a sealed room's reference is ~0, and
/// the leak is then measured against 1 % of the frame's light instead of dividing by zero.
pub const LEAK_FLOOR_FRACTION: f32 = 0.01;

/// Rec. 709 luminance.
#[must_use]
pub fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// `max(0, a − b)` per channel.
#[must_use]
pub fn sub_clamped(a: &[[f32; 3]], b: &[[f32; 3]]) -> Vec<[f32; 3]> {
    a.iter()
        .zip(b)
        .map(|(x, y)| [0, 1, 2].map(|k| (x[k] - y[k]).max(0.0)))
        .collect()
}

/// A frame's linear RGB pixels, row-major.
pub type Pixels = [[f32; 3]];
/// `|a − b| · gain` per channel: the amplified difference tile (ADR-1326).
#[must_use]
pub fn abs_diff_gain(a: &[[f32; 3]], b: &[[f32; 3]], gain: f32) -> Vec<[f32; 3]> {
    a.iter()
        .zip(b)
        .map(|(x, y)| [0, 1, 2].map(|k| (x[k] - y[k]).abs() * gain))
        .collect()
}

/// `a + b` per channel.
#[must_use]
pub fn add(a: &[[f32; 3]], b: &[[f32; 3]]) -> Vec<[f32; 3]> {
    a.iter()
        .zip(b)
        .map(|(x, y)| [0, 1, 2].map(|k| x[k] + y[k]))
        .collect()
}

/// The reference's indirect light, in the raster's sense (everything the frame gets from its GI
/// rather than from its analytic lights): the tracer's diffuse and specular indirect AOVs, plus,
/// when the raster lights nothing with its emitters, the first bounce of the emitters' light
/// (`direct − direct of the same scene with emission off`): then only the bake carries that light
/// (ADR-206), so it is indirect for this review. A frame with emissive area lights (Desktop High)
/// lights with its emitters directly, and the caller passes `None` (ADR-1326).
#[must_use]
pub fn reference_indirect(
    indirect_diffuse: &[[f32; 3]],
    indirect_specular: &[[f32; 3]],
    emitter_bounce: Option<(&Pixels, &Pixels)>,
) -> Vec<[f32; 3]> {
    let gi = add(indirect_diffuse, indirect_specular);
    match emitter_bounce {
        Some((direct, direct_without_emitters)) => add(&gi, &sub_clamped(direct, direct_without_emitters)),
        None => gi,
    }
}

/// A copy of `test` with every pixel where the reference hit nothing (`depth == 0`: sky) taken
/// from `reference`. The scores then judge surfaces only; the raster's and the tracer's
/// backgrounds are different things (a clear colour and a uniform sky) and not what this kit
/// reviews. The tiles show the frames unchanged.
#[must_use]
pub fn fill_sky(test: &[[f32; 3]], reference: &[[f32; 3]], depth: &[f32]) -> Vec<[f32; 3]> {
    test.iter()
        .zip(reference)
        .zip(depth)
        .map(|((t, r), d)| if *d > 0.0 { *t } else { *r })
        .collect()
}

/// The pinhole camera of a view, for turning a pixel and its view depth into a world point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelCamera {
    /// Eye.
    pub eye: [f32; 3],
    /// Look-at point.
    pub target: [f32; 3],
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Image width.
    pub width: u32,
    /// Image height.
    pub height: u32,
}

impl PixelCamera {
    /// The world point at view depth `depth` (distance along the forward axis, the tracer's
    /// `depth` AOV) through the centre of pixel `(x, y)`; `None` for depth 0 (no hit).
    #[must_use]
    pub fn world_point(&self, x: u32, y: u32, depth: f32) -> Option<[f32; 3]> {
        if !(depth > 0.0 && depth.is_finite()) {
            return None;
        }
        let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        let norm = |v: [f32; 3]| {
            let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-12);
            [v[0] / l, v[1] / l, v[2] / l]
        };
        let cross = |a: [f32; 3], b: [f32; 3]| {
            [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ]
        };
        let fwd = norm(sub(self.target, self.eye));
        let right = norm(cross(fwd, [0.0, 1.0, 0.0]));
        let up = cross(right, fwd);
        let (w, h) = (self.width.max(1) as f32, self.height.max(1) as f32);
        let t = (self.fov_y * 0.5).tan();
        let sx = ((x as f32 + 0.5) / w * 2.0 - 1.0) * t * w / h;
        let sy = (1.0 - (y as f32 + 0.5) / h * 2.0) * t;
        // The direction's forward component is 1, so the view depth is the ray parameter.
        Some([0, 1, 2].map(|k| self.eye[k] + (fwd[k] + right[k] * sx + up[k] * sy) * depth))
    }
}

/// Pixels whose reference first hit lies inside `interior` grown by [`INTERIOR_MARGIN`].
#[must_use]
pub fn interior_mask(camera: &PixelCamera, depth: &[f32], interior: &WorldBox) -> Vec<bool> {
    let w = camera.width.max(1);
    depth
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let (x, y) = ((i as u32) % w, (i as u32) / w);
            camera
                .world_point(x, y, *d)
                .is_some_and(|p| interior.contains(p, INTERIOR_MARGIN))
        })
        .collect()
}

/// Mean luminance over the pixels where `mask` is true, `None` when it selects none.
#[must_use]
pub fn mean_luma(img: &[[f32; 3]], mask: impl Fn(usize) -> bool) -> Option<f64> {
    let (mut sum, mut n) = (0.0f64, 0u64);
    for (i, c) in img.iter().enumerate() {
        if mask(i) {
            sum += f64::from(luma(*c));
            n += 1;
        }
    }
    if n > 0 { Some(sum / n as f64) } else { None }
}

/// The leak metric of one view (ADR-314).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LeakStats {
    /// Pixels in the interior mask.
    pub pixels: u64,
    /// Mean luminance of the baked frame's indirect light in the mask, cd/m².
    pub baked_mean: f32,
    /// Mean luminance of the reference's indirect light in the mask, cd/m².
    pub reference_mean: f32,
    /// The floor under the reference level ([`LEAK_FLOOR_FRACTION`] of the frame), cd/m².
    pub floor: f32,
    /// `max(0, baked − reference) / max(reference, floor)`; `None` when both the reference
    /// level and the floor are 0 (a black frame).
    pub leak: Option<f32>,
}

/// The leak metric: how much more indirect light the bake puts inside the interior mask than the
/// reference, relative to the reference's own interior level. Means over the mask, not per-pixel
/// excess, so the reference's noise does not read as a leak. `None` when the mask is empty (the
/// interior is not in view).
#[must_use]
pub fn leak(
    baked_indirect: &[[f32; 3]],
    reference_indirect: &[[f32; 3]],
    reference_full: &[[f32; 3]],
    depth: &[f32],
    mask: &[bool],
) -> Option<LeakStats> {
    let inside = |i: usize| mask.get(i).copied().unwrap_or(false);
    let pixels = mask.iter().filter(|m| **m).count() as u64;
    let baked = mean_luma(baked_indirect, inside)?;
    let reference = mean_luma(reference_indirect, inside)?;
    let frame = mean_luma(reference_full, |i| depth.get(i).is_some_and(|d| *d > 0.0)).unwrap_or(0.0);
    let floor = frame * f64::from(LEAK_FLOOR_FRACTION);
    let denom = reference.max(floor);
    let excess = (baked - reference).max(0.0);
    Some(LeakStats {
        pixels,
        baked_mean: baked as f32,
        reference_mean: reference as f32,
        floor: floor as f32,
        leak: if denom > 0.0 {
            Some((excess / denom) as f32)
        } else {
            None
        },
    })
}

/// FLIP error mapped to the review's false colour (black → magenta → orange → white), RGBA8.
#[must_use]
pub fn error_tile(flip_map: &[f32]) -> Vec<u8> {
    regia_render::compare::flip_heatmap_rgba8(flip_map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indirect_isolation_adds_the_emitter_bounce_only_when_asked() {
        let d = vec![[1.0, 2.0, 3.0]; 2];
        let s = vec![[0.5, 0.0, 0.0]; 2];
        assert_eq!(reference_indirect(&d, &s, None), vec![[1.5, 2.0, 3.0]; 2]);
        let direct = vec![[4.0, 4.0, 4.0]; 2];
        let analytic = vec![[3.0, 5.0, 4.0]; 2];
        assert_eq!(
            reference_indirect(&d, &s, Some((&direct, &analytic))),
            vec![[2.5, 2.0, 3.0]; 2],
            "the emitters' first bounce, clamped at 0 per channel"
        );
    }

    #[test]
    fn the_difference_tile_is_absolute_and_amplified() {
        let a = vec![[1.0, 2.0, 3.0], [0.0; 3]];
        let b = vec![[1.5, 2.0, 1.0], [0.0; 3]];
        assert_eq!(
            abs_diff_gain(&a, &b, 4.0),
            vec![[2.0, 0.0, 8.0], [0.0; 3]],
            "a darker and a brighter channel both show, four times over"
        );
    }

    #[test]
    fn sky_pixels_come_from_the_reference() {
        let t = vec![[9.0; 3], [1.0; 3]];
        let r = vec![[2.0; 3], [3.0; 3]];
        assert_eq!(fill_sky(&t, &r, &[0.0, 4.0]), vec![[2.0; 3], [1.0; 3]]);
    }

    #[test]
    fn the_centre_pixel_lands_on_the_forward_axis() {
        let cam = PixelCamera {
            eye: [0.0, 1.0, 5.0],
            target: [0.0, 1.0, 0.0],
            fov_y: 1.0,
            width: 3,
            height: 3,
        };
        let p = cam.world_point(1, 1, 5.0).expect("hit");
        assert!(
            p.iter().zip([0.0, 1.0, 0.0]).all(|(a, b)| (a - b).abs() < 1e-5),
            "{p:?}"
        );
        assert_eq!(cam.world_point(1, 1, 0.0), None);
        // The top-left pixel is up and to the left (−X from a camera looking down −Z).
        let q = cam.world_point(0, 0, 5.0).expect("hit");
        assert!(q[0] < 0.0 && q[1] > 1.0, "{q:?}");
    }

    #[test]
    fn a_leak_above_the_reference_is_measured_against_its_level_or_the_floor() {
        // Four pixels: two inside the mask, one outside, one sky.
        let mask = [true, true, false, false];
        let depth = [1.0, 1.0, 1.0, 0.0];
        let full = vec![[100.0; 3], [100.0; 3], [100.0; 3], [5000.0; 3]];
        let reference = vec![[10.0; 3], [10.0; 3], [50.0; 3], [0.0; 3]];
        let baked = vec![[14.0; 3], [16.0; 3], [50.0; 3], [0.0; 3]];
        let l = leak(&baked, &reference, &full, &depth, &mask).expect("mask in view");
        assert_eq!(l.pixels, 2);
        assert!((l.leak.expect("defined") - 0.5).abs() < 1e-5, "{l:?}");
        assert!(
            (l.floor - 1.0).abs() < 1e-4,
            "1 % of the geometry pixels' mean, sky excluded"
        );
        // A sealed room: the reference is black inside, so the floor is the denominator.
        let dark = vec![[0.0; 3]; 4];
        let l = leak(&baked, &dark, &full, &depth, &mask).expect("mask in view");
        assert!((l.leak.expect("defined") - 15.0).abs() < 1e-3, "{l:?}");
        // A darker bake is not a leak.
        let l = leak(&dark, &reference, &full, &depth, &mask).expect("mask in view");
        assert_eq!(l.leak, Some(0.0));
        // No interior pixel in view.
        assert_eq!(leak(&baked, &reference, &full, &depth, &[false; 4]), None);
    }

    #[test]
    fn the_mask_selects_hits_inside_the_box() {
        let cam = PixelCamera {
            eye: [0.0, 1.0, 5.0],
            target: [0.0, 1.0, 0.0],
            fov_y: 1.0,
            width: 3,
            height: 1,
        };
        let interior = WorldBox {
            min: [-1.0, 0.0, -1.0],
            max: [1.0, 2.0, 1.0],
        };
        // Centre pixel hits at z = 0 (inside); the others miss or hit far behind the box.
        let mask = interior_mask(&cam, &[0.0, 5.0, 20.0], &interior);
        assert_eq!(mask, vec![false, true, false]);
    }
}
