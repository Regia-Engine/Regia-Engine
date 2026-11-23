// SPDX-License-Identifier: MPL-2.0
//! The per-instance crack mask page and CPU stamping (`docs/formats/glass-crack.md`).
//!
//! A page is `size`² RGBA8 (R cracks, G crazing, B holes, A normal-bend) plus an RG8 tangent
//! normal page. Stamps combine with a per-channel max, so overlapping impacts commute; the
//! normal keeps the texel whose stamp has the larger A. Stamping is a CPU blit (an impact is a
//! rare event, a 512² page is under a millisecond), deterministic for a given impact list,
//! which is what saves rely on (the page is rebuilt from the impacts on load).

use regia_asset::glasscrack::StampPixels;

use crate::soft_math;

/// One damaged instance's crack mask (CPU copy; the renderer uploads it when
/// [`Self::version`] changes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlassMask {
    /// Side length in texels.
    pub size: u32,
    /// `size² × 4` bytes, linear.
    pub rgba: Vec<u8>,
    /// `size² × 2` bytes, tangent normal X/Y (128 = flat).
    pub normal: Vec<u8>,
    /// Bumped on every change.
    pub version: u64,
}

/// Where and how one stamp lands on a page (all in page texels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StampPlacement {
    /// Impact point on the page, texels (x right, y down).
    pub center: [f32; 2],
    /// Stamp footprint on the page, texels (width, height of the whole stamp texture).
    pub footprint: [f32; 2],
    /// Impact point inside the stamp texture, 0..1.
    pub pivot: [f32; 2],
    /// Rotation around the pivot, radians.
    pub rotation: f32,
    /// Holes (B) are cleared beyond this many texels from the centre (`f32::INFINITY` = no clamp).
    pub hole_radius: f32,
    /// Multiplier on B (0 = no holes from this stamp).
    pub hole_scale: f32,
    /// Milky halo (G) radius in texels around the centre, 0 = none.
    pub halo_radius: f32,
}

impl GlassMask {
    /// An intact page.
    #[must_use]
    pub fn new(size: u32) -> Self {
        let texels = (size * size) as usize;
        Self {
            size,
            rgba: vec![0; texels * 4],
            normal: vec![128; texels * 2],
            version: 1,
        }
    }

    /// Texel `(x, y)` RGBA.
    #[must_use]
    pub fn texel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y.min(self.size - 1) * self.size + x.min(self.size - 1)) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
    }

    /// Stamps `stamp` at `at`. Returns the number of texels touched.
    pub fn stamp(&mut self, stamp: &StampPixels, at: &StampPlacement) -> usize {
        let _span = tracing::info_span!("glass_stamp").entered();
        let n = self.size as i32;
        let src_n = stamp.size as f32;
        // soft_math: the rotated coverage is quantized into u8 texels, so the platform
        // libm is not allowed here (audit G, render5 determinism).
        let (sin, cos) = (soft_math::sin(at.rotation), soft_math::cos(at.rotation));
        // Stamp-texture texels per page texel along each axis.
        let scale = [
            src_n / at.footprint[0].max(1e-3),
            src_n / at.footprint[1].max(1e-3),
        ];
        // Bounding box of the rotated footprint around the centre.
        let corners = [
            [-at.pivot[0], -at.pivot[1]],
            [1.0 - at.pivot[0], -at.pivot[1]],
            [-at.pivot[0], 1.0 - at.pivot[1]],
            [1.0 - at.pivot[0], 1.0 - at.pivot[1]],
        ];
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for c in corners {
            let (dx, dy) = (c[0] * at.footprint[0], c[1] * at.footprint[1]);
            let p = [
                at.center[0] + dx * cos - dy * sin,
                at.center[1] + dx * sin + dy * cos,
            ];
            for k in 0..2 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        let halo = at.halo_radius.max(0.0);
        for k in 0..2 {
            lo[k] = lo[k].min(at.center[k] - halo);
            hi[k] = hi[k].max(at.center[k] + halo);
        }
        let x0 = (lo[0].floor() as i32).max(0);
        let y0 = (lo[1].floor() as i32).max(0);
        let x1 = (hi[0].ceil() as i32).min(n - 1);
        let y1 = (hi[1].ceil() as i32).min(n - 1);
        // Minifying stamps sample a k×k grid over each page texel's footprint (k ≤ 6), so thin
        // crack lines stay one full-strength texel wide instead of aliasing away.
        let taps = [
            (scale[0].ceil() as usize).clamp(1, 6),
            (scale[1].ceil() as usize).clamp(1, 6),
        ];
        let mut touched = 0usize;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let (px, py) = (x as f32 + 0.5 - at.center[0], y as f32 + 0.5 - at.center[1]);
                // Into the stamp's frame (inverse rotation), then stamp texels.
                let (lx, ly) = (px * cos + py * sin, -px * sin + py * cos);
                let sx = lx * scale[0] + at.pivot[0] * src_n;
                let sy = ly * scale[1] + at.pivot[1] * src_n;
                let mut value = sample_footprint(stamp, [sx, sy], scale, taps);
                let r = (px * px + py * py).sqrt();
                value[2] *= at.hole_scale;
                if r > at.hole_radius {
                    value[2] = 0.0;
                }
                if halo > 0.0 && r < halo {
                    let t = 1.0 - r / halo;
                    value[1] = value[1].max(0.55 * t * t);
                }
                if value.iter().all(|v| *v <= 0.0) {
                    continue;
                }
                let i = (y * n + x) as usize;
                let dst = &mut self.rgba[i * 4..i * 4 + 4];
                let new_a = to_u8(value[3]);
                if new_a > dst[3] {
                    let (nx, ny) = sample_normal(stamp, sx, sy);
                    // Rotate the tangent-space XY with the stamp. The page rotation is in
                    // image axes (Y down); tangent Y points up, so it turns the other way.
                    let (rx, ry) = (nx * cos + ny * sin, -nx * sin + ny * cos);
                    self.normal[i * 2] = to_u8(rx * 0.5 + 0.5);
                    self.normal[i * 2 + 1] = to_u8(ry * 0.5 + 0.5);
                }
                for (c, v) in value.iter().enumerate() {
                    dst[c] = dst[c].max(to_u8(*v));
                }
                touched += 1;
            }
        }
        self.version += 1;
        touched
    }
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// The stamp over one page texel centred at stamp texel `centre`: R, B and A take the max of a
/// `taps` grid across the texel (conservative, so lines survive minification), G (crazing
/// density) the mean.
fn sample_footprint(stamp: &StampPixels, centre: [f32; 2], scale: [f32; 2], taps: [usize; 2]) -> [f32; 4] {
    let src_n = stamp.size as f32;
    let inside = |sx: f32, sy: f32| sx >= 0.0 && sy >= 0.0 && sx < src_n && sy < src_n;
    if taps == [1, 1] {
        return if inside(centre[0], centre[1]) {
            sample_rgba(stamp, centre[0], centre[1])
        } else {
            [0.0; 4]
        };
    }
    let mut out = [0.0f32; 4];
    let mut g = 0.0;
    for j in 0..taps[1] {
        for i in 0..taps[0] {
            // Offsets across the page texel, in stamp texels (the grid follows the stamp axes;
            // a rotated footprint differs from it by under a texel).
            let ox = ((i as f32 + 0.5) / taps[0] as f32 - 0.5) * scale[0];
            let oy = ((j as f32 + 0.5) / taps[1] as f32 - 0.5) * scale[1];
            let (sx, sy) = (centre[0] + ox, centre[1] + oy);
            if !inside(sx, sy) {
                continue;
            }
            let v = sample_rgba(stamp, sx, sy);
            out[0] = out[0].max(v[0]);
            out[2] = out[2].max(v[2]);
            out[3] = out[3].max(v[3]);
            g += v[1];
        }
    }
    out[1] = g / (taps[0] * taps[1]) as f32;
    out
}

/// Bilinear sample of the stamp mask at stamp-texel coordinates, 0..1 per channel.
fn sample_rgba(stamp: &StampPixels, sx: f32, sy: f32) -> [f32; 4] {
    let n = stamp.size as i32;
    let (fx, fy) = (sx - 0.5, sy - 0.5);
    let (ix, iy) = (fx.floor() as i32, fy.floor() as i32);
    let (tx, ty) = (fx - ix as f32, fy - iy as f32);
    let mut out = [0.0f32; 4];
    for (dy, wy) in [(0, 1.0 - ty), (1, ty)] {
        for (dx, wx) in [(0, 1.0 - tx), (1, tx)] {
            let (x, y) = (ix + dx, iy + dy);
            if x < 0 || y < 0 || x >= n || y >= n {
                continue;
            }
            let i = ((y * n + x) * 4) as usize;
            for (o, byte) in out.iter_mut().zip(&stamp.mask[i..i + 4]) {
                *o += f32::from(*byte) / 255.0 * wx * wy;
            }
        }
    }
    out
}

/// Nearest tangent normal XY in −1..1 (flat outside the stamp).
fn sample_normal(stamp: &StampPixels, sx: f32, sy: f32) -> (f32, f32) {
    let n = stamp.size as i32;
    let (x, y) = (sx.floor() as i32, sy.floor() as i32);
    if x < 0 || y < 0 || x >= n || y >= n {
        return (0.0, 0.0);
    }
    let i = ((y * n + x) * 2) as usize;
    (
        f32::from(stamp.normal[i]) / 127.5 - 1.0,
        f32::from(stamp.normal[i + 1]) / 127.5 - 1.0,
    )
}

/// RG8 mip chain (box filter) of a square normal page; level 0 excluded.
#[must_use]
pub fn mip_chain_rg8(size: u32, rg: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let mut out: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut n = size;
    let mut prev = rg.to_vec();
    while n > 1 {
        let m = n / 2;
        let mut next = vec![0u8; (m * m * 2) as usize];
        for y in 0..m {
            for x in 0..m {
                // The last output texel of an odd edge also averages the orphan texel (`n - 1`).
                let xe = if x + 1 == m { n - 1 } else { 2 * x + 1 };
                let ye = if y + 1 == m { n - 1 } else { 2 * y + 1 };
                let count = (xe - 2 * x + 1) * (ye - 2 * y + 1);
                for c in 0..2 {
                    let mut sum = 0u32;
                    for yy in 2 * y..=ye {
                        for xx in 2 * x..=xe {
                            sum += u32::from(prev[((yy * n + xx) * 2 + c) as usize]);
                        }
                    }
                    next[((y * m + x) * 2 + c) as usize] = ((sum + count / 2) / count) as u8;
                }
            }
        }
        out.push((m, next.clone()));
        prev = next;
        n = m;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stamp with a bright 2×2 block around its centre (all channels), so a bilinear sample at
    /// the pivot reads full intensity.
    fn dot_stamp(size: u32) -> StampPixels {
        let n = (size * size) as usize;
        let mut mask = vec![0; n * 4];
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let c = ((size / 2 - 1 + dy) * size + size / 2 - 1 + dx) as usize * 4;
            mask[c..c + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
        StampPixels {
            size,
            mask,
            normal: vec![128; n * 2],
        }
    }

    fn place(center: [f32; 2], footprint: f32) -> StampPlacement {
        StampPlacement {
            center,
            footprint: [footprint; 2],
            pivot: [0.5, 0.5],
            rotation: 0.0,
            hole_radius: f32::INFINITY,
            hole_scale: 1.0,
            halo_radius: 0.0,
        }
    }

    #[test]
    fn a_thin_crack_survives_minification_at_full_strength() {
        // A 1-texel vertical line in a 256-texel stamp drawn over 64 page texels (4:1).
        let size = 256u32;
        let mut mask = vec![0u8; (size * size * 4) as usize];
        for y in 0..size {
            let i = ((y * size + 101) * 4) as usize;
            mask[i] = 255;
            mask[i + 3] = 255;
        }
        let stamp = StampPixels {
            size,
            mask,
            normal: vec![128; (size * size * 2) as usize],
        };
        let mut page = GlassMask::new(128);
        page.stamp(&stamp, &place([64.0, 64.0], 64.0));
        // Every row the line crosses keeps one full-strength texel.
        for y in 36..92 {
            let peak = (32..96).map(|x| page.texel(x, y)[0]).max().unwrap_or(0);
            assert!(peak >= 250, "row {y}: the crack faded to {peak}");
        }
    }

    #[test]
    fn a_stamp_lands_at_its_centre() {
        let mut page = GlassMask::new(128);
        // 16-texel stamp drawn over 16 page texels: 1:1.
        page.stamp(&dot_stamp(16), &place([40.0, 70.0], 16.0));
        let mut best = (0u8, 0u32, 0u32);
        for y in 0..128 {
            for x in 0..128 {
                let r = page.texel(x, y)[0];
                if r > best.0 {
                    best = (r, x, y);
                }
            }
        }
        assert!(best.0 > 200, "stamp missing");
        assert!(
            (best.1 as i32 - 40).abs() <= 1 && (best.2 as i32 - 70).abs() <= 1,
            "{best:?}"
        );
    }

    #[test]
    fn stamps_combine_with_max_and_commute() {
        let s = dot_stamp(8);
        let mut a = GlassMask::new(64);
        let mut b = GlassMask::new(64);
        a.stamp(&s, &place([10.0, 10.0], 16.0));
        a.stamp(&s, &place([12.0, 11.0], 16.0));
        b.stamp(&s, &place([12.0, 11.0], 16.0));
        b.stamp(&s, &place([10.0, 10.0], 16.0));
        assert_eq!(a.rgba, b.rgba);
    }

    #[test]
    fn hole_radius_and_scale_clamp_b() {
        let mut full = dot_stamp(16);
        for t in full.mask.as_chunks_mut::<4>().0.iter_mut() {
            t[2] = 255;
        }
        let mut page = GlassMask::new(64);
        let mut at = place([32.0, 32.0], 32.0);
        at.hole_radius = 4.0;
        page.stamp(&full, &at);
        for y in 0..64u32 {
            for x in 0..64u32 {
                let d = ((x as f32 + 0.5 - 32.0).powi(2) + (y as f32 + 0.5 - 32.0).powi(2)).sqrt();
                if d > 4.0 {
                    assert_eq!(page.texel(x, y)[2], 0, "hole outside radius at {x},{y}");
                }
            }
        }
        let mut none = GlassMask::new(64);
        let mut at = place([32.0, 32.0], 32.0);
        at.hole_scale = 0.0;
        none.stamp(&full, &at);
        assert!(none.rgba.as_chunks::<4>().0.iter().all(|t| t[2] == 0));
    }

    #[test]
    fn rg_mips_halve_to_one() {
        let chain = mip_chain_rg8(8, &[200; 128]);
        assert_eq!(chain.iter().map(|(n, _)| *n).collect::<Vec<_>>(), vec![4, 2, 1]);
        assert!(chain.iter().all(|(_, px)| px.iter().all(|v| *v == 200)));
    }

    #[test]
    fn a_normal_sample_just_outside_the_stamp_is_flat() {
        let mut stamp = dot_stamp(4);
        // Distinct border normal, so reading it would show.
        stamp.normal[0] = 255;
        stamp.normal[1] = 255;
        assert_eq!(sample_normal(&stamp, -0.5, -0.5), (0.0, 0.0));
        assert_eq!(sample_normal(&stamp, -0.5, 0.5), (0.0, 0.0));
        let inside = sample_normal(&stamp, 0.5, 0.5);
        assert!(inside.0 > 0.9 && inside.1 > 0.9, "{inside:?}");
    }

    #[test]
    fn an_odd_normal_page_keeps_its_orphan_edge() {
        // 3×3 page, only the last column/row carry a value in channel 0.
        let mut rg = vec![0u8; 3 * 3 * 2];
        for i in 0..3 {
            rg[(i * 3 + 2) * 2] = 255;
            rg[(2 * 3 + i) * 2] = 255;
        }
        let chain = mip_chain_rg8(3, &rg);
        assert_eq!(chain.len(), 1);
        // The 1×1 level averages all nine texels (5 are 255): not 0.
        assert_eq!(chain[0].1[0], ((5 * 255 + 4) / 9) as u8);
    }
}
