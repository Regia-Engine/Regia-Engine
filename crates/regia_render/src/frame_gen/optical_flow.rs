// SPDX-License-Identifier: MPL-2.0
//! Optical flow inputs and a CPU reference of the block-matching estimator (ADR-514 § 3).
//!
//! Game motion vectors miss motion that has no geometry behind it: shadows sliding over the
//! ground, reflections, particles, scrolling textures. Frame interpolation therefore also uses an
//! optical flow field estimated from the presented images. The FidelityFX SDK v1.1.4 documents its
//! estimator (`docs/techniques/optical-flow.md`, MIT): a luminance pyramid of the presented colour;
//! a scene-change test on histograms of 3 × 3 screen sections; then, from the coarsest level down,
//! a full search of 8 × 8 blocks within ±8 pixels by the sum of absolute differences (SAD), a 3 × 3
//! "middle vector" filter, and a 2× upscale that gives each finer block the best of four parent
//! candidates by the SAD of its 4 × 4 quadrant. The block-matching method itself is classical
//! (Jain & Jain, "Displacement measurement and its application in interframe image coding", IEEE
//! Trans. Communications 1981; hierarchical search after Bierling, "Displacement estimation by
//! hierarchical blockmatching", SPIE VCIP 1988).
//!
//! The code below is Regia Engine's own, written from those descriptions, and runs on the CPU. It is
//! the oracle for the WGSL port: on the test images the GPU field must equal this one vector for
//! vector. Vectors are in full-resolution pixels and point from a block's current position to
//! its previous position, the same direction as the game motion vectors
//! ([`super::MotionVectorConvention::REGIA`]).

use super::TransferFunction;

/// Pixels per side of a flow block (the SDK's only block size).
pub const BLOCK_SIZE: u32 = 8;
/// Search radius per level in that level's pixels.
pub const SEARCH_RADIUS: i32 = 8;
/// Most pyramid levels the estimator uses.
pub const MAX_LEVELS: usize = 7;
/// Screen sections per side for the scene-change histograms.
pub const SCENE_SECTIONS: usize = 3;
/// Histogram bins per section.
pub const HISTOGRAM_BINS: usize = 16;

/// The flow field's size for an image: one vector per started block.
#[must_use]
pub fn flow_field_size(image: (u32, u32), block: u32) -> (u32, u32) {
    let block = block.max(1);
    (image.0.div_ceil(block), image.1.div_ceil(block))
}

/// SMPTE ST 2084 (PQ) inverse EOTF: absolute luminance in cd/m² to a 0–1 signal.
#[must_use]
pub fn pq_encode(nits: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let y = (nits.max(0.0) / 10_000.0).min(1.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2)
}

/// The flow luminance (0–1) of one presented pixel.
///
/// The estimator compares luminance with SAD, so it wants a perceptually even scale: SDR uses
/// Rec.709 luma of the sRGB-encoded values; HDR maps luminance through PQ and stretches the
/// display's `[min, max]` cd/m² to 0–1. `rgb` is the value as stored: encoded for
/// [`TransferFunction::Srgb`] and [`TransferFunction::Pq`], linear (1 = 80 cd/m²) for
/// [`TransferFunction::ScRgb`].
#[must_use]
pub fn flow_luma(rgb: [f32; 3], transfer: TransferFunction, min_max_luminance: [f32; 2]) -> f32 {
    let dot = |w: [f32; 3]| w[0] * rgb[0] + w[1] * rgb[1] + w[2] * rgb[2];
    let stretch = |signal: f32| {
        let lo = pq_encode(min_max_luminance[0]);
        let hi = pq_encode(min_max_luminance[1]);
        if hi > lo {
            (signal - lo) / (hi - lo)
        } else {
            signal
        }
    };
    let luma = match transfer {
        TransferFunction::Srgb => dot([0.2126, 0.7152, 0.0722]),
        // Rec.2020 luma weights on the PQ signal.
        TransferFunction::Pq => stretch(dot([0.2627, 0.6780, 0.0593])),
        TransferFunction::ScRgb => stretch(pq_encode(dot([0.2126, 0.7152, 0.0722]) * 80.0)),
    };
    if luma.is_finite() {
        luma.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// [`flow_luma`] quantized to 8 bits, the estimator's input.
#[must_use]
pub fn flow_luma_u8(rgb: [f32; 3], transfer: TransferFunction, min_max_luminance: [f32; 2]) -> u8 {
    (flow_luma(rgb, transfer, min_max_luminance) * 255.0).round() as u8
}

/// An 8-bit luminance image, row-major.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LumaImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl LumaImage {
    /// `pixels` as a `width × height` image; `None` when the length does not match or a side is 0.
    #[must_use]
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Option<Self> {
        let expected = usize::try_from(u64::from(width) * u64::from(height)).ok()?;
        (width > 0 && height > 0 && pixels.len() == expected).then_some(Self {
            width,
            height,
            pixels,
        })
    }

    /// An image whose pixel `(x, y)` is `f(x, y)`.
    #[must_use]
    pub fn from_fn(width: u32, height: u32, mut f: impl FnMut(u32, u32) -> u8) -> Self {
        let (width, height) = (width.max(1), height.max(1));
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

    /// `(width, height)`.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The pixels, row-major.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The pixel at `(x, y)` with the coordinates clamped to the image.
    #[must_use]
    pub fn get_clamped(&self, x: i32, y: i32) -> u8 {
        let x = x.clamp(0, self.width as i32 - 1) as usize;
        let y = y.clamp(0, self.height as i32 - 1) as usize;
        self.pixels[y * self.width as usize + x]
    }

    /// Half the size (rounded up): the rounded mean of each 2 × 2 footprint, edges clamped.
    #[must_use]
    pub fn downsample(&self) -> Self {
        let (w, h) = (self.width.div_ceil(2), self.height.div_ceil(2));
        Self::from_fn(w, h, |x, y| {
            let (sx, sy) = (2 * x as i32, 2 * y as i32);
            let sum = u32::from(self.get_clamped(sx, sy))
                + u32::from(self.get_clamped(sx + 1, sy))
                + u32::from(self.get_clamped(sx, sy + 1))
                + u32::from(self.get_clamped(sx + 1, sy + 1));
            ((sum + 2) / 4) as u8
        })
    }
}

/// Histograms of the 3 × 3 screen sections, row-major.
pub type SectionHistograms = [[u32; HISTOGRAM_BINS]; SCENE_SECTIONS * SCENE_SECTIONS];

/// The section histograms of `image`.
#[must_use]
pub fn section_histograms(image: &LumaImage) -> SectionHistograms {
    let mut out = [[0u32; HISTOGRAM_BINS]; SCENE_SECTIONS * SCENE_SECTIONS];
    let (w, h) = image.size();
    let sections = SCENE_SECTIONS as u64;
    for (i, &value) in image.pixels().iter().enumerate() {
        let (x, y) = (i as u64 % u64::from(w), i as u64 / u64::from(w));
        let sx = (x * sections / u64::from(w)) as usize;
        let sy = (y * sections / u64::from(h)) as usize;
        let bin = usize::from(value) * HISTOGRAM_BINS / 256;
        out[sy * SCENE_SECTIONS + sx][bin] += 1;
    }
    out
}

/// Per-section histogram difference, 0 (same) to 1 (disjoint): `Σ|a − b| / (2 · pixels)`.
#[must_use]
pub fn section_differences(
    a: &SectionHistograms,
    b: &SectionHistograms,
) -> [f32; SCENE_SECTIONS * SCENE_SECTIONS] {
    let mut out = [0.0; SCENE_SECTIONS * SCENE_SECTIONS];
    for (slot, (ha, hb)) in out.iter_mut().zip(a.iter().zip(b)) {
        let total: u32 = ha.iter().sum::<u32>().max(hb.iter().sum::<u32>()).max(1);
        let diff: u32 = ha.iter().zip(hb).map(|(x, y)| x.abs_diff(*y)).sum();
        *slot = diff as f32 / (2.0 * total as f32);
    }
    out
}

/// Detects a scene change between consecutive presented images (a cut the game did not flag).
/// The estimator returns zero motion after one, as the SDK's does.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneChangeDetector {
    /// A section counts as changed above this histogram difference.
    pub section_threshold: f32,
    /// A scene change needs at least this many changed sections.
    pub min_sections: usize,
    previous: Option<SectionHistograms>,
}

impl Default for SceneChangeDetector {
    fn default() -> Self {
        Self {
            section_threshold: 0.5,
            min_sections: 5,
            previous: None,
        }
    }
}

impl SceneChangeDetector {
    /// Takes the next image; true when it starts a new scene. The first image after a
    /// [`Self::reset`] has no history and returns false: the caller's reset covers it.
    pub fn update(&mut self, image: &LumaImage) -> bool {
        let current = section_histograms(image);
        let changed = self.previous.as_ref().is_some_and(|previous| {
            section_differences(previous, &current)
                .iter()
                .filter(|&&d| d > self.section_threshold)
                .count()
                >= self.min_sections
        });
        self.previous = Some(current);
        changed
    }

    /// Forgets the history.
    pub fn reset(&mut self) {
        self.previous = None;
    }
}

/// A block motion field: one vector per [`BLOCK_SIZE`] block, in full-resolution pixels, pointing
/// to the block's previous position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowField {
    /// Blocks per row and column.
    pub blocks: (u32, u32),
    /// Pixels per block side.
    pub block_size: u32,
    /// Row-major vectors.
    pub vectors: Vec<[i16; 2]>,
}

impl FlowField {
    /// The vector of block `(bx, by)`, clamped to the grid.
    #[must_use]
    pub fn block(&self, bx: u32, by: u32) -> [i16; 2] {
        let bx = bx.min(self.blocks.0.saturating_sub(1)) as usize;
        let by = by.min(self.blocks.1.saturating_sub(1)) as usize;
        self.vectors
            .get(by * self.blocks.0 as usize + bx)
            .copied()
            .unwrap_or([0, 0])
    }

    /// The vector covering pixel `(x, y)`.
    #[must_use]
    pub fn at_pixel(&self, x: u32, y: u32) -> [i16; 2] {
        self.block(x / self.block_size.max(1), y / self.block_size.max(1))
    }
}

// Pixels `sad` has read on this thread. Test instrumentation for the search early-out.
// Release builds do not count. A doc comment cannot sit on `thread_local!`.
#[cfg(test)]
thread_local! {
    static SAD_PIXEL_VISITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_sad_visit() {
    SAD_PIXEL_VISITS.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(test)]
fn sad_pixel_visits() -> u64 {
    SAD_PIXEL_VISITS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn reset_sad_pixel_visits() {
    SAD_PIXEL_VISITS.with(|count| count.set(0));
}

/// SAD of the `size`-pixel square of `current` at `origin` against `previous` at
/// `origin + offset` (current pixels outside the image are skipped, previous ones clamped).
///
/// Returns as soon as the sum passes `limit`. That candidate cannot beat a strictly smaller SAD,
/// and an equal SAD is still counted in full so the shorter correction can win the tie.
fn sad(
    previous: &LumaImage,
    current: &LumaImage,
    origin: (i32, i32),
    offset: [i32; 2],
    size: i32,
    limit: u32,
) -> u32 {
    let (w, h) = current.size();
    let mut sum = 0;
    for y in origin.1..origin.1 + size {
        if y >= h as i32 {
            break;
        }
        for x in origin.0..origin.0 + size {
            if x >= w as i32 {
                break;
            }
            let c = current.get_clamped(x, y);
            let p = previous.get_clamped(x + offset[0], y + offset[1]);
            sum += u32::from(c.abs_diff(p));
            #[cfg(test)]
            note_sad_visit();
            if sum > limit {
                return sum;
            }
        }
    }
    sum
}

/// Full search around each block's guess: the offset with the least SAD, ties to the shorter
/// correction.
///
/// The zero correction is scored first, so a good guess sets a tight limit. Every other offset
/// keeps the previous scan order (the corner-first window, with the zero slot skipped).
fn search(
    previous: &LumaImage,
    current: &LumaImage,
    blocks: (u32, u32),
    guess: &[[i32; 2]],
) -> Vec<[i32; 2]> {
    let b = BLOCK_SIZE as i32;
    let window = SEARCH_RADIUS * 2 + 1;
    let count = window * window;
    let zero = SEARCH_RADIUS * window + SEARCH_RADIUS;
    let mut out = Vec::with_capacity(guess.len());
    for by in 0..blocks.1 as i32 {
        for bx in 0..blocks.0 as i32 {
            let g = guess[(by * blocks.0 as i32 + bx) as usize];
            let mut best = (u32::MAX, i32::MAX, g);
            for step in 0..count {
                let k = if step == 0 {
                    zero
                } else {
                    let earlier = step - 1;
                    if earlier >= zero { earlier + 1 } else { earlier }
                };
                let dy = k / window - SEARCH_RADIUS;
                let dx = k % window - SEARCH_RADIUS;
                let v = [g[0] + dx, g[1] + dy];
                let cost = (
                    sad(previous, current, (bx * b, by * b), v, b, best.0),
                    dx.abs() + dy.abs(),
                );
                if cost < (best.0, best.1) {
                    best = (cost.0, cost.1, v);
                }
            }
            out.push(best.2);
        }
    }
    out
}

/// The 3 × 3 "middle vector": of a block and its neighbours, the vector with the least summed
/// L1 distance to the others (the block's own vector wins ties).
fn middle_filter(field: &[[i32; 2]], blocks: (u32, u32)) -> Vec<[i32; 2]> {
    let (w, h) = (blocks.0 as i32, blocks.1 as i32);
    let mut out = Vec::with_capacity(field.len());
    for by in 0..h {
        for bx in 0..w {
            let mut group = Vec::with_capacity(9);
            group.push(field[(by * w + bx) as usize]);
            for ny in by - 1..=by + 1 {
                for nx in bx - 1..=bx + 1 {
                    if (nx, ny) != (bx, by) && (0..w).contains(&nx) && (0..h).contains(&ny) {
                        group.push(field[(ny * w + nx) as usize]);
                    }
                }
            }
            let cost = |v: [i32; 2]| -> i32 {
                group
                    .iter()
                    .map(|o| (v[0] - o[0]).abs() + (v[1] - o[1]).abs())
                    .sum()
            };
            let mut best = group[0];
            let mut best_cost = cost(best);
            for &v in &group[1..] {
                let c = cost(v);
                if c < best_cost {
                    best = v;
                    best_cost = c;
                }
            }
            out.push(best);
        }
    }
    out
}

/// One level finer: each fine block takes, of its parent and the three parents next to its
/// quadrant, the vector whose SAD over the quadrant (at the coarse level) is least, doubled.
fn upscale(
    previous: &LumaImage,
    current: &LumaImage,
    field: &[[i32; 2]],
    blocks: (u32, u32),
    fine_blocks: (u32, u32),
) -> Vec<[i32; 2]> {
    let (w, h) = (blocks.0 as i32, blocks.1 as i32);
    let at = |x: i32, y: i32| field[(y.clamp(0, h - 1) * w + x.clamp(0, w - 1)) as usize];
    let half = BLOCK_SIZE as i32 / 2;
    let mut out = Vec::with_capacity(fine_blocks.0 as usize * fine_blocks.1 as usize);
    for fy in 0..fine_blocks.1 as i32 {
        for fx in 0..fine_blocks.0 as i32 {
            let (px, py) = ((fx / 2).min(w - 1), (fy / 2).min(h - 1));
            let (qx, qy) = (fx % 2, fy % 2);
            let (sx, sy) = (if qx == 0 { -1 } else { 1 }, if qy == 0 { -1 } else { 1 });
            let origin = (
                px * BLOCK_SIZE as i32 + qx * half,
                py * BLOCK_SIZE as i32 + qy * half,
            );
            let candidates = [at(px, py), at(px + sx, py), at(px, py + sy), at(px + sx, py + sy)];
            let mut best = candidates[0];
            let mut best_sad = sad(previous, current, origin, best, half, u32::MAX);
            for &v in &candidates[1..] {
                let s = sad(previous, current, origin, v, half, best_sad);
                if s < best_sad {
                    best = v;
                    best_sad = s;
                }
            }
            out.push([best[0] * 2, best[1] * 2]);
        }
    }
    out
}

/// Estimates the block motion from `previous` to `current` over up to `levels` pyramid levels
/// (clamped to 1..=[`MAX_LEVELS`], and to the levels whose image still holds a block). `None`
/// when the sizes differ.
#[must_use]
pub fn estimate_flow(previous: &LumaImage, current: &LumaImage, levels: usize) -> Option<FlowField> {
    if previous.size() != current.size() {
        return None;
    }
    let _span = tracing::info_span!("frame_gen_reference_flow").entered();
    let mut prev_pyramid = vec![previous.clone()];
    let mut cur_pyramid = vec![current.clone()];
    while prev_pyramid.len() < levels.clamp(1, MAX_LEVELS) {
        let (Some(p), Some(c)) = (prev_pyramid.last(), cur_pyramid.last()) else {
            break;
        };
        let (w, h) = p.size();
        if w.div_ceil(2) < BLOCK_SIZE || h.div_ceil(2) < BLOCK_SIZE {
            break;
        }
        let (p, c) = (p.downsample(), c.downsample());
        prev_pyramid.push(p);
        cur_pyramid.push(c);
    }
    let top = prev_pyramid.len() - 1;
    let mut blocks = flow_field_size(cur_pyramid[top].size(), BLOCK_SIZE);
    let mut guess = vec![[0i32; 2]; blocks.0 as usize * blocks.1 as usize];
    for level in (0..=top).rev() {
        let (p, c) = (&prev_pyramid[level], &cur_pyramid[level]);
        let raw = search(p, c, blocks, &guess);
        let filtered = middle_filter(&raw, blocks);
        if level == 0 {
            guess = filtered;
            break;
        }
        let fine = flow_field_size(cur_pyramid[level - 1].size(), BLOCK_SIZE);
        guess = upscale(p, c, &filtered, blocks, fine);
        blocks = fine;
    }
    let clamp = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    Some(FlowField {
        blocks,
        block_size: BLOCK_SIZE,
        vectors: guess.into_iter().map(|v| [clamp(v[0]), clamp(v[1])]).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic texture with detail at every pyramid level: hashed noise summed over
    /// 1, 2, 4, 8 and 16 pixel cells. `(x, y)` may be negative (shifted copies).
    fn texture(x: i32, y: i32) -> u8 {
        let hash = |cx: i32, cy: i32, salt: u32| -> u32 {
            let mut h = (cx as u32).wrapping_mul(0x9E37_79B1) ^ (cy as u32).wrapping_mul(0x85EB_CA77) ^ salt;
            h ^= h >> 15;
            h = h.wrapping_mul(0x2C1B_3C6D);
            h ^= h >> 12;
            h & 0xFF
        };
        let mut sum = 0u32;
        for (i, cell) in [1, 2, 4, 8, 16].into_iter().enumerate() {
            sum += hash(x.div_euclid(cell), y.div_euclid(cell), i as u32);
        }
        (sum / 5) as u8
    }

    fn shifted(size: u32, shift: (i32, i32)) -> LumaImage {
        LumaImage::from_fn(size, size, |x, y| texture(x as i32 - shift.0, y as i32 - shift.1))
    }

    #[test]
    fn the_field_has_one_vector_per_started_block() {
        assert_eq!(flow_field_size((1920, 1080), BLOCK_SIZE), (240, 135));
        assert_eq!(flow_field_size((1921, 1081), BLOCK_SIZE), (241, 136));
        assert_eq!(flow_field_size((3840, 2160), BLOCK_SIZE), (480, 270));
    }

    #[test]
    fn luma_spans_zero_to_one_in_every_encoding() {
        let range = [0.0, 1000.0];
        assert_eq!(flow_luma([0.0; 3], TransferFunction::Srgb, range), 0.0);
        assert!((flow_luma([1.0; 3], TransferFunction::Srgb, range) - 1.0).abs() < 1.0e-6);
        // HDR10: the display's peak maps to 1, its black to 0.
        let peak = pq_encode(1000.0);
        assert!((flow_luma([peak; 3], TransferFunction::Pq, range) - 1.0).abs() < 1.0e-4);
        assert_eq!(flow_luma([0.0; 3], TransferFunction::Pq, range), 0.0);
        // scRGB 12.5 = 1000 cd/m².
        assert!((flow_luma([12.5; 3], TransferFunction::ScRgb, range) - 1.0).abs() < 1.0e-4);
        // Brighter is never darker, and nothing escapes 0–1.
        let mut last = 0.0;
        for i in 0..=40 {
            let v = flow_luma([i as f32; 3], TransferFunction::ScRgb, range);
            assert!(v >= last && (0.0..=1.0).contains(&v));
            last = v;
        }
        assert_eq!(flow_luma([f32::NAN; 3], TransferFunction::Srgb, range), 0.0);
        assert_eq!(flow_luma_u8([1.0; 3], TransferFunction::Srgb, range), 255);
        // PQ reference points (ST 2084): 100 cd/m² ≈ 0.508, 10 000 cd/m² = 1.
        assert!((pq_encode(100.0) - 0.5081).abs() < 1.0e-3);
        assert!((pq_encode(10_000.0) - 1.0).abs() < 1.0e-5);
    }

    #[test]
    fn downsampling_rounds_odd_sizes_up_and_averages() {
        let image = LumaImage::from_fn(5, 3, |x, y| (x * 10 + y) as u8);
        let half = image.downsample();
        assert_eq!(half.size(), (3, 2));
        // (0,0): mean of 0, 10, 1, 11 = 5.5 → 6.
        assert_eq!(half.get_clamped(0, 0), 6);
        // (2,1): every tap clamps to (4,2) = 42.
        assert_eq!(half.get_clamped(2, 1), 42);
        assert!(LumaImage::new(2, 2, vec![0; 3]).is_none());
        assert!(LumaImage::new(0, 2, Vec::new()).is_none());
    }

    #[test]
    fn identical_images_have_no_motion() {
        let image = shifted(64, (0, 0));
        let flow = estimate_flow(&image, &image, MAX_LEVELS).unwrap_or_else(|| panic!("sizes match"));
        assert_eq!(flow.blocks, (8, 8));
        assert!(flow.vectors.iter().all(|v| *v == [0, 0]), "{:?}", flow.vectors);
    }

    #[test]
    fn a_translation_beyond_one_search_radius_is_found_through_the_pyramid() {
        // The image moved 13 px right and 6 px up: each block's previous position is 13 px left
        // and 6 px down, so the vector is (−13, +6).
        let previous = shifted(128, (0, 0));
        let current = shifted(128, (13, -6));
        let flow = estimate_flow(&previous, &current, MAX_LEVELS).unwrap_or_else(|| panic!("sizes match"));
        assert_eq!(flow.blocks, (16, 16));
        // Interior blocks (the border ones see clamped edges).
        for by in 2..14 {
            for bx in 2..14 {
                assert_eq!(flow.block(bx, by), [-13, 6], "block ({bx}, {by})");
            }
        }
        // One level only cannot reach 13 px with a ±8 search.
        let flat = estimate_flow(&previous, &current, 1).unwrap_or_else(|| panic!("sizes match"));
        assert_ne!(flat.block(8, 8), [-13, 6]);
        assert!(estimate_flow(&previous, &shifted(64, (0, 0)), MAX_LEVELS).is_none());
    }

    #[test]
    fn a_cut_is_a_scene_change_and_a_pan_is_not() {
        let mut detector = SceneChangeDetector::default();
        let a = shifted(96, (0, 0));
        assert!(!detector.update(&a), "no history yet");
        assert!(
            !detector.update(&shifted(96, (5, 3))),
            "a pan keeps the histograms"
        );
        let inverted = LumaImage::from_fn(96, 96, |x, y| 255 - texture(x as i32, y as i32) / 4);
        assert!(detector.update(&inverted), "a different scene");
        detector.reset();
        assert!(!detector.update(&a));
        let same = section_histograms(&a);
        assert!(section_differences(&same, &same).iter().all(|&d| d == 0.0));
    }

    /// Pixel reads of the fixed search: the zero correction first, then the old window order,
    /// stopping a candidate once its partial SAD passes the best so far.
    fn early_out_visits(image: &LumaImage) -> u64 {
        let blocks = flow_field_size(image.size(), BLOCK_SIZE);
        let window = SEARCH_RADIUS * 2 + 1;
        let count = window * window;
        let zero = SEARCH_RADIUS * window + SEARCH_RADIUS;
        let block = BLOCK_SIZE as i32;
        let (w, h) = image.size();
        let mut visits = 0u64;
        for by in 0..blocks.1 as i32 {
            for bx in 0..blocks.0 as i32 {
                let origin = (bx * block, by * block);
                let mut best_sad = u32::MAX;
                let mut best_len = i32::MAX;
                for step in 0..count {
                    let k = if step == 0 {
                        zero
                    } else {
                        let earlier = step - 1;
                        if earlier >= zero { earlier + 1 } else { earlier }
                    };
                    let dy = k / window - SEARCH_RADIUS;
                    let dx = k % window - SEARCH_RADIUS;
                    let mut sum = 0u32;
                    'pixels: for y in origin.1..origin.1 + block {
                        if y >= h as i32 {
                            break;
                        }
                        for x in origin.0..origin.0 + block {
                            if x >= w as i32 {
                                break;
                            }
                            visits += 1;
                            let diff = u32::from(
                                image
                                    .get_clamped(x, y)
                                    .abs_diff(image.get_clamped(x + dx, y + dy)),
                            );
                            sum += diff;
                            if sum > best_sad {
                                break 'pixels;
                            }
                        }
                    }
                    let len = dx.abs() + dy.abs();
                    if (sum, len) < (best_sad, best_len) {
                        best_sad = sum;
                        best_len = len;
                    }
                }
            }
        }
        visits
    }

    #[test]
    fn audit_g3_perfect_guess_stops_at_the_first_differing_pixel() {
        // Every pixel is distinct, so a non-zero offset inside the image differs at the first
        // sample once the zero correction has set the limit to 0.
        let image = LumaImage::from_fn(24, 16, |x, y| {
            (x.wrapping_mul(17)
                .wrapping_add(y.wrapping_mul(13))
                .wrapping_add(1)
                & 0xff) as u8
        });
        let blocks = flow_field_size(image.size(), BLOCK_SIZE);
        let full = u64::from(blocks.0) * u64::from(blocks.1) * (17 * 17) * u64::from(BLOCK_SIZE * BLOCK_SIZE);
        let expected = early_out_visits(&image);
        assert!(
            expected < full / 4,
            "the early-out model read {expected} pixels of a full scan {full}"
        );
        reset_sad_pixel_visits();
        let guess = vec![[0, 0]; blocks.0 as usize * blocks.1 as usize];
        let found = search(&image, &image, blocks, &guess);
        assert!(found.iter().all(|v| *v == [0, 0]), "{found:?}");
        let visits = sad_pixel_visits();
        assert_eq!(
            visits, expected,
            "pixel visits {visits}, early-out model {expected}, full scan {full}"
        );
    }
}
