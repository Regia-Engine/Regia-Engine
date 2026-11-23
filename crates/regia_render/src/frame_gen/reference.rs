// SPDX-License-Identifier: MPL-2.0
//! CPU reference of motion-vector frame interpolation (ADR-514 § 2).
//!
//! The oracle that fixes the conventions the WGSL interpolator must follow: the motion vector
//! points from a pixel's current position to its previous one, and the frame at time `t`
//! (0 = previous, 1 = current; 0.5 for one generated frame) sees each surface at
//! `current + vector · (1 − t)`. It follows the stages the FidelityFX SDK v1.1.4 documents for its
//! frame interpolation (`docs/techniques/frame-interpolation.md`, MIT), reduced to their meaning:
//!
//! 1. Splat each current pixel's vector to its position in the interpolated frame; the nearest
//!    surface wins (the SDK's game motion vector field with depth priority).
//! 2. Fill positions nothing reached with the farthest vector nearby: a gap opens where a nearer
//!    surface moved away, so what shows there is behind it (the SDK's inpainting of the field).
//! 3. Sample the previous frame at `q + vector · t` and the current frame at
//!    `q − vector · (1 − t)`; a side whose depth does not match the vector's surface is
//!    disoccluded and dropped (the SDK's two disocclusion masks); blend the rest by `t`.
//!
//! On a reset it returns the current frame, as the SDK does. The code is Regia Engine's own and is a
//! test oracle, not a runtime path: it runs on the CPU at test sizes.

/// A linear RGB image, row-major.
#[derive(Debug, Clone, PartialEq)]
pub struct ColorImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row-major pixels.
    pub pixels: Vec<[f32; 3]>,
}

impl ColorImage {
    /// An image whose pixel `(x, y)` is `f(x, y)`.
    #[must_use]
    pub fn from_fn(width: u32, height: u32, mut f: impl FnMut(u32, u32) -> [f32; 3]) -> Self {
        let mut pixels = Vec::with_capacity(width as usize * height as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.push(f(x, y));
            }
        }
        Self {
            width,
            height,
            pixels,
        }
    }

    /// The pixel at `(x, y)`, or `None` outside the image.
    #[must_use]
    pub fn get(&self, x: i32, y: i32) -> Option<[f32; 3]> {
        self.index(x, y).map(|i| self.pixels[i])
    }

    fn index(&self, x: i32, y: i32) -> Option<usize> {
        ((0..self.width as i32).contains(&x) && (0..self.height as i32).contains(&y))
            .then(|| y as usize * self.width as usize + x as usize)
    }
}

/// What [`interpolate`] reads. Every per-pixel slice has `width × height` entries.
#[derive(Debug, Clone, Copy)]
pub struct InterpolationInputs<'a> {
    /// The previous real frame.
    pub previous: &'a ColorImage,
    /// The current real frame.
    pub current: &'a ColorImage,
    /// Per current pixel: motion in pixels toward the previous position.
    pub motion_px: &'a [[f32; 2]],
    /// Per current pixel: view depth in metres.
    pub current_depth: &'a [f32],
    /// Per previous pixel: view depth in metres.
    pub previous_depth: &'a [f32],
    /// Time of the generated frame, 0 (previous) to 1 (current).
    pub t: f32,
    /// A camera cut: return the current frame.
    pub reset: bool,
}

/// The interpolated frame and how it was made.
#[derive(Debug, Clone, PartialEq)]
pub struct Interpolated {
    /// The generated image.
    pub image: ColorImage,
    /// Pixels no vector reached, filled from a neighbour's vector.
    pub filled: usize,
    /// Pixels where one side (or both) was disoccluded.
    pub disoccluded: usize,
}

/// Window radius (pixels) for filling the vector field's gaps.
pub const FILL_RADIUS: i32 = 4;
/// Relative depth difference under which a sample belongs to the vector's surface.
pub const DEPTH_TOLERANCE: f32 = 0.1;

/// Interpolates between `inputs.previous` and `inputs.current` at `inputs.t`. `None` when the
/// sizes of the inputs disagree.
#[must_use]
pub fn interpolate(inputs: &InterpolationInputs<'_>) -> Option<Interpolated> {
    let (w, h) = (inputs.current.width, inputs.current.height);
    let n = w as usize * h as usize;
    let sizes_match = inputs.previous.width == w
        && inputs.previous.height == h
        && inputs.current.pixels.len() == n
        && inputs.previous.pixels.len() == n
        && inputs.motion_px.len() == n
        && inputs.current_depth.len() == n
        && inputs.previous_depth.len() == n;
    if !sizes_match {
        return None;
    }
    if inputs.reset {
        return Some(Interpolated {
            image: inputs.current.clone(),
            filled: 0,
            disoccluded: 0,
        });
    }
    let t = inputs.t.clamp(0.0, 1.0);
    let index = |x: i32, y: i32| inputs.current.index(x, y);

    // 1. Splat: (vector, depth) per interpolated pixel, the nearest surface winning.
    let mut field: Vec<Option<([f32; 2], f32)>> = vec![None; n];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let Some(i) = index(x, y) else { continue };
            let m = inputs.motion_px[i];
            let d = inputs.current_depth[i];
            let qx = (x as f32 + m[0] * (1.0 - t)).round() as i32;
            let qy = (y as f32 + m[1] * (1.0 - t)).round() as i32;
            if let Some(q) = index(qx, qy)
                && field[q].is_none_or(|(_, old)| d < old)
            {
                field[q] = Some((m, d));
            }
        }
    }

    // 2. Fill gaps with the farthest vector in the window (the nearest one on a depth tie).
    let mut filled = 0;
    let mut complete = field.clone();
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let Some(i) = index(x, y) else { continue };
            if field[i].is_some() {
                continue;
            }
            let mut best: Option<([f32; 2], f32, i32)> = None;
            for ny in y - FILL_RADIUS..=y + FILL_RADIUS {
                for nx in x - FILL_RADIUS..=x + FILL_RADIUS {
                    let Some((m, d)) = index(nx, ny).and_then(|j| field[j]) else {
                        continue;
                    };
                    let dist = (nx - x).abs().max((ny - y).abs());
                    if best.is_none_or(|(_, bd, bdist)| d > bd || (d == bd && dist < bdist)) {
                        best = Some((m, d, dist));
                    }
                }
            }
            complete[i] = Some(best.map_or(([0.0, 0.0], inputs.current_depth[i]), |(m, d, _)| (m, d)));
            filled += 1;
        }
    }

    // 3. Sample both frames along the vector; drop a side that shows another surface.
    let matches =
        |depth: f32, surface: f32| (depth - surface).abs() <= DEPTH_TOLERANCE * surface.abs().max(1.0e-6);
    let mut disoccluded = 0;
    let mut pixels = Vec::with_capacity(n);
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let Some((m, d)) = index(x, y).and_then(|i| complete[i]) else {
                continue;
            };
            let px = (x as f32 + m[0] * t).round() as i32;
            let py = (y as f32 + m[1] * t).round() as i32;
            let cx = (x as f32 - m[0] * (1.0 - t)).round() as i32;
            let cy = (y as f32 - m[1] * (1.0 - t)).round() as i32;
            let prev = index(px, py)
                .filter(|&j| matches(inputs.previous_depth[j], d))
                .map(|j| inputs.previous.pixels[j]);
            let cur = index(cx, cy)
                .filter(|&j| matches(inputs.current_depth[j], d))
                .map(|j| inputs.current.pixels[j]);
            let color = match (prev, cur) {
                (Some(a), Some(b)) => [
                    a[0] * (1.0 - t) + b[0] * t,
                    a[1] * (1.0 - t) + b[1] * t,
                    a[2] * (1.0 - t) + b[2] * t,
                ],
                (Some(a), None) => {
                    disoccluded += 1;
                    a
                }
                (None, Some(b)) => {
                    disoccluded += 1;
                    b
                }
                (None, None) => {
                    disoccluded += 1;
                    let cx = cx.clamp(0, w as i32 - 1);
                    let cy = cy.clamp(0, h as i32 - 1);
                    inputs.current.get(cx, cy).unwrap_or([0.0; 3])
                }
            };
            pixels.push(color);
        }
    }
    Some(Interpolated {
        image: ColorImage {
            width: w,
            height: h,
            pixels,
        },
        filled,
        disoccluded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 32;
    const H: u32 = 16;
    const NEAR: f32 = 5.0;
    const FAR: f32 = 100.0;
    const WHITE: [f32; 3] = [1.0; 3];
    const BLACK: [f32; 3] = [0.0; 3];

    fn in_square(x: u32, y: u32, left: u32) -> bool {
        (left..left + 4).contains(&x) && (6..10).contains(&y)
    }

    /// A 4 × 4 white square on black, at `left` in the previous frame and `left + 8` now.
    struct Scene {
        previous: ColorImage,
        current: ColorImage,
        motion: Vec<[f32; 2]>,
        current_depth: Vec<f32>,
        previous_depth: Vec<f32>,
    }

    fn scene(left: u32) -> Scene {
        let colour = |sq: bool| if sq { WHITE } else { BLACK };
        let depth = |sq: bool| if sq { NEAR } else { FAR };
        let now = left + 8;
        let all = |f: &dyn Fn(u32, u32) -> f32| -> Vec<f32> {
            (0..H)
                .flat_map(|y| (0..W).map(move |x| (x, y)))
                .map(|(x, y)| f(x, y))
                .collect()
        };
        Scene {
            previous: ColorImage::from_fn(W, H, |x, y| colour(in_square(x, y, left))),
            current: ColorImage::from_fn(W, H, |x, y| colour(in_square(x, y, now))),
            // The square came from 8 px to the left: its vector points there.
            motion: (0..H)
                .flat_map(|y| (0..W).map(move |x| (x, y)))
                .map(|(x, y)| {
                    if in_square(x, y, now) {
                        [-8.0, 0.0]
                    } else {
                        [0.0, 0.0]
                    }
                })
                .collect(),
            current_depth: all(&|x, y| depth(in_square(x, y, now))),
            previous_depth: all(&|x, y| depth(in_square(x, y, left))),
        }
    }

    fn run(s: &Scene, t: f32, reset: bool) -> Interpolated {
        interpolate(&InterpolationInputs {
            previous: &s.previous,
            current: &s.current,
            motion_px: &s.motion,
            current_depth: &s.current_depth,
            previous_depth: &s.previous_depth,
            t,
            reset,
        })
        .unwrap_or_else(|| panic!("sizes match"))
    }

    fn assert_square_at(image: &ColorImage, left: u32) {
        for y in 0..H {
            for x in 0..W {
                let want = if in_square(x, y, left) { WHITE } else { BLACK };
                assert_eq!(image.get(x as i32, y as i32), Some(want), "pixel ({x}, {y})");
            }
        }
    }

    #[test]
    fn the_midpoint_frame_shows_the_square_halfway_with_clean_background() {
        let s = scene(4);
        let out = run(&s, 0.5, false);
        // Previous at 4, current at 12: halfway is 8.
        assert_square_at(&out.image, 8);
        // The background the square uncovered (12..16 in rows 6..10) had no vector: filled.
        assert_eq!(out.filled, 16);
        // Where the square was (4..8) and where it goes (12..16), one side is another surface.
        assert_eq!(out.disoccluded, 32);
    }

    #[test]
    fn other_times_follow_the_vector_linearly() {
        let s = scene(4);
        assert_square_at(&run(&s, 0.25, false).image, 6);
        assert_square_at(&run(&s, 0.75, false).image, 10);
        assert_square_at(&run(&s, 1.0, false).image, 12);
        assert_square_at(&run(&s, 0.0, false).image, 4);
    }

    #[test]
    fn a_reset_returns_the_current_frame() {
        let s = scene(4);
        let out = run(&s, 0.5, true);
        assert_eq!(out.image, s.current);
        assert_eq!((out.filled, out.disoccluded), (0, 0));
    }

    #[test]
    fn mismatched_sizes_are_refused() {
        let s = scene(4);
        let short = vec![[0.0; 2]; 3];
        let inputs = InterpolationInputs {
            previous: &s.previous,
            current: &s.current,
            motion_px: &short,
            current_depth: &s.current_depth,
            previous_depth: &s.previous_depth,
            t: 0.5,
            reset: false,
        };
        assert!(interpolate(&inputs).is_none());
    }
}
