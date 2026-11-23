// SPDX-License-Identifier: MPL-2.0
//! Cook-side damage decal atlas (ADR-8038).
//!
//! A 1024×1024 RGBA8 image, sixteen 256×256 cells, for multiply-over-paint decals: dark holes and
//! soot, bright metal on scratches and torn edges, alpha as coverage. This is not the runtime
//! 64×64 projected atlas ([`crate::generate_decal_atlas`]).
//!
//! Cells, row-major, top-left origin:
//! 0–3 bullet holes, 4–5 shell craters, 6–9 scratch fields, 10–11 scorch and soot, 12 weld patch,
//! 13–14 crack webs, 15 rivet line.
//!
//! Trig, powers and the floor of the value noise go through [`regia_core::math`] so two calls
//! return the same bytes on every target. The hash is the well-known 32-bit mix
//! `x = a * 2654435761 xor b`, then `x * 2246822519 >> 16`.

use regia_core::math::{det_atan2, det_powf_nonnegative, det_sin, det_sin_cos};

/// Texels on one side of the atlas.
pub const DAMAGE_DECAL_ATLAS_SIZE: u32 = 1024;
/// Texels on one side of a cell.
pub const DAMAGE_DECAL_CELL: u32 = 256;
/// Cells in the atlas (4 by 4).
pub const DAMAGE_DECAL_CELLS: u32 = 16;

const CELL_CENTER: f32 = 128.0;
const PI: f32 = std::f32::consts::PI;
const TWO_PI: f32 = PI * 2.0;

/// RGBA8 bytes, row-major, top-left origin. Length is `SIZE * SIZE * 4`.
#[must_use]
pub fn generate_damage_decal_atlas() -> Vec<u8> {
    let side = DAMAGE_DECAL_ATLAS_SIZE as usize;
    let mut scratches = Vec::with_capacity(DAMAGE_DECAL_CELLS as usize);
    let mut spokes = Vec::with_capacity(DAMAGE_DECAL_CELLS as usize);
    for k in 0..DAMAGE_DECAL_CELLS {
        scratches.push(if (6..10).contains(&k) {
            scratch_lines(k)
        } else {
            Vec::new()
        });
        spokes.push(if k == 13 || k == 14 {
            crack_spokes(k)
        } else {
            Vec::new()
        });
    }
    let mut rgba = vec![0u8; side * side * 4];
    for y in 0..DAMAGE_DECAL_ATLAS_SIZE {
        let (cy, ly) = (y / DAMAGE_DECAL_CELL, y % DAMAGE_DECAL_CELL);
        for x in 0..DAMAGE_DECAL_ATLAS_SIZE {
            let (cx, lx) = (x / DAMAGE_DECAL_CELL, x % DAMAGE_DECAL_CELL);
            let k = cy * 4 + cx;
            let sample = sample_cell(
                k,
                lx as f32 + 0.5,
                ly as f32 + 0.5,
                &scratches[k as usize],
                &spokes[k as usize],
            );
            let i = ((y as usize * side) + x as usize) * 4;
            rgba[i] = channel(sample[0]);
            rgba[i + 1] = channel(sample[1]);
            rgba[i + 2] = channel(sample[2]);
            rgba[i + 3] = channel(sample[3]);
        }
    }
    rgba
}

/// FNV-1a 64 of `bytes`. The agent reports this when it returns a generated atlas.
#[must_use]
pub fn damage_decal_atlas_fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// One 256×256 cell of an atlas from [`generate_damage_decal_atlas`] as a decal v3 custom image
/// (ADR-8829): register it with [`crate::decal_format::DecalLibrary::add_damage_cell`] or
/// [`crate::decal_format::DecalLibrary::add_textured`]. `None` when `cell` is not below
/// [`DAMAGE_DECAL_CELLS`] or `atlas` is not `SIZE * SIZE * 4` bytes.
#[must_use]
pub fn damage_decal_cell_image(atlas: &[u8], cell: u32) -> Option<regia_asset::CookedImage> {
    let side = DAMAGE_DECAL_ATLAS_SIZE as usize;
    if cell >= DAMAGE_DECAL_CELLS || atlas.len() != side * side * 4 {
        return None;
    }
    let per_row = DAMAGE_DECAL_ATLAS_SIZE / DAMAGE_DECAL_CELL;
    let (x0, y0) = (
        ((cell % per_row) * DAMAGE_DECAL_CELL) as usize,
        ((cell / per_row) * DAMAGE_DECAL_CELL) as usize,
    );
    let cell_side = DAMAGE_DECAL_CELL as usize;
    let mut rgba8 = Vec::with_capacity(cell_side * cell_side * 4);
    for y in y0..y0 + cell_side {
        let start = (y * side + x0) * 4;
        rgba8.extend_from_slice(&atlas[start..start + cell_side * 4]);
    }
    Some(regia_asset::CookedImage {
        width: DAMAGE_DECAL_CELL,
        height: DAMAGE_DECAL_CELL,
        rgba8,
    })
}

/// The `texture` key a damage-atlas cell registers under (`damage-atlas/cell-07`): a valid v3
/// relative path, so equal cells share one decal atlas layer.
#[must_use]
pub fn damage_decal_cell_key(cell: u32) -> String {
    format!("damage-atlas/cell-{cell:02}")
}

fn channel(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0) as u8
}

fn sample_cell(k: u32, x: f32, y: f32, lines: &[ScratchLine], spokes: &[f32]) -> [f32; 4] {
    match k {
        0..=3 => bullet(k, x, y),
        4 | 5 => crater(k, x, y),
        6..=9 => scratch(x, y, lines),
        10 | 11 => scorch(k, x, y),
        12 => weld(x, y),
        13 | 14 => crack(k, x, y, spokes),
        _ => rivet(x, y),
    }
}

fn bullet(k: u32, x: f32, y: f32) -> [f32; 4] {
    let radius = [18.0, 26.0, 36.0, 50.0][k as usize];
    let (d, a) = polar(x, y);
    let jag = 0.75
        + 0.35
            * vnoise(
                det_sin_cos(a).1 * 1.6 + 10.0,
                det_sin(a) * 1.6 + 10.0,
                k as i32 + 10,
            );
    if d < radius * 0.42 * jag {
        return [0.02, 0.02, 0.02, 1.0];
    }
    if d < radius * 0.78 * jag {
        return [0.62, 0.60, 0.57, 1.0];
    }
    let t = (d - radius * 0.78 * jag) / (radius * 1.1);
    if t < 1.0 {
        let fade = det_powf_nonnegative((1.0 - t).max(0.0), 1.6)
            * 0.85
            * (0.6 + 0.4 * fbm(x / 20.0, y / 20.0, k as i32));
        return [0.06, 0.05, 0.05, fade];
    }
    [0.0, 0.0, 0.0, 0.0]
}

fn crater(k: u32, x: f32, y: f32) -> [f32; 4] {
    let radius = if k == 4 { 78.0 } else { 104.0 };
    let (d, a) = polar(x, y);
    let (sin_a, cos_a) = det_sin_cos(a);
    let rim = radius * (0.55 + 0.25 * vnoise(cos_a * 2.2 + 3.0, sin_a * 2.2 + 3.0, k as i32));
    if d < rim * 0.55 {
        return [0.03, 0.03, 0.03, 1.0];
    }
    if d < rim {
        let shade = 0.35 + 0.2 * fbm(x / 8.0, y / 8.0, k as i32);
        return [shade, 0.32, 0.30, 1.0];
    }
    let t = (d - rim) / (radius * 0.8);
    if t < 1.0 {
        let streak =
            0.5 + 0.5 * det_sin(a * 23.0 + vnoise(cos_a * 3.0 + 1.0, sin_a * 3.0 + 1.0, k as i32) * 6.0);
        let alpha = (1.0 - t) * (0.5 + 0.5 * streak) * 0.9;
        return [0.05, 0.04, 0.04, alpha];
    }
    [0.0, 0.0, 0.0, 0.0]
}

fn scratch(x: f32, y: f32, lines: &[ScratchLine]) -> [f32; 4] {
    let mut best = 0.0f32;
    for line in lines {
        let px = x - line.x0;
        let py = y - line.y0;
        let u = px * line.cx + py * line.sy;
        let half = line.len * 0.5;
        if (-half..=half).contains(&u) {
            let across = (-px * line.sy + py * line.cx).abs();
            let along = (1.0 - u.abs() / half).max(0.0);
            let cover = (1.0 - across / line.w).max(0.0) * det_powf_nonnegative(along, 0.3);
            best = best.max(cover);
        }
    }
    if best > 0.0 {
        [0.85, 0.84, 0.80, best * 0.9]
    } else {
        [0.0, 0.0, 0.0, 0.0]
    }
}

fn scorch(k: u32, x: f32, y: f32) -> [f32; 4] {
    let mut dx = (x - CELL_CENTER) / CELL_CENTER;
    let mut dy = (y - CELL_CENTER) / CELL_CENTER;
    if k == 11 {
        dy = dy * 0.45 + 0.35;
        dx *= 1.6;
    }
    let d = hypot(dx, dy);
    let n = fbm(x / 28.0, y / 28.0, k as i32);
    let alpha = (1.0 - d * (1.1 + 0.6 * (1.0 - n))).max(0.0) * (0.5 + 0.5 * n);
    [0.05, 0.045, 0.04, (alpha * 1.4).min(1.0)]
}

fn weld(x: f32, y: f32) -> [f32; 4] {
    if (48.0..=208.0).contains(&x) && (72.0..=184.0).contains(&y) {
        let edge = (x - 48.0).min(208.0 - x).min(y - 72.0).min(184.0 - y);
        if edge < 6.0 {
            let bead = 0.5 + 0.5 * det_sin((x + y) * 0.9);
            return [0.45 + 0.25 * bead, 0.40 + 0.2 * bead, 0.33, 1.0];
        }
        return [0.55, 0.53, 0.50, 0.95];
    }
    [0.0, 0.0, 0.0, 0.0]
}

fn crack(k: u32, x: f32, y: f32, spokes: &[f32]) -> [f32; 4] {
    let (d, a) = polar(x, y);
    let mut best = 0.0f32;
    for spoke in spokes {
        let da = pos_mod(a - spoke + PI, TWO_PI) - PI;
        let wob = 0.02 * det_sin(d * 0.15 + spoke * 3.0);
        if d < 118.0 && (da - wob).abs() * d < 1.2 {
            best = best.max(1.0 - d / 130.0);
        }
    }
    for ring in [38.0, 74.0] {
        let (sin_a, cos_a) = det_sin_cos(a);
        let radius = ring * (0.9 + 0.2 * vnoise(cos_a + ring, sin_a + ring, k as i32));
        if (d - radius).abs() < 1.0 && vnoise(cos_a * 3.0 + ring, sin_a * 3.0, k as i32 + 5) > 0.35 {
            best = best.max(0.8);
        }
    }
    if best > 0.0 {
        [0.90, 0.92, 0.94, best * 0.85]
    } else {
        [0.0, 0.0, 0.0, 0.0]
    }
}

fn rivet(x: f32, y: f32) -> [f32; 4] {
    for i in 0..8 {
        let cx = 22.0 + i as f32 * 30.0;
        let dx = x - cx;
        let dy = y - CELL_CENTER;
        if hypot(dx, dy) < 7.0 {
            let highlight = if dx + dy < -2.0 { 1.0 } else { 0.0 };
            let metal = 0.35 + 0.35 * highlight;
            return [metal, metal, 0.37 + 0.35 * highlight, 1.0];
        }
    }
    [0.0, 0.0, 0.0, 0.0]
}

struct ScratchLine {
    x0: f32,
    y0: f32,
    cx: f32,
    sy: f32,
    len: f32,
    w: f32,
}

fn scratch_lines(k: u32) -> Vec<ScratchLine> {
    let count = 10 + 4 * (k - 6);
    let mut lines = Vec::with_capacity(count as usize);
    let scale = if k.is_multiple_of(2) { 1.0 } else { 0.3 };
    for i in 0..count {
        let angle = (h32(k * 1000 + i, 77) % 360) as f32 * PI / 180.0 * scale;
        let (sy, cx) = det_sin_cos(angle);
        lines.push(ScratchLine {
            x0: 20.0 + (h32(k, 100 + i) % 216) as f32,
            y0: 20.0 + (h32(k, 200 + i) % 216) as f32,
            cx,
            sy,
            len: 50.0 + (h32(k * 1000 + i, 78) % 170) as f32,
            w: 0.6 + (h32(k, 300 + i) % 10) as f32 / 10.0,
        });
    }
    lines
}

fn crack_spokes(k: u32) -> Vec<f32> {
    let count = 7 + 3 * (k - 13);
    (0..count)
        .map(|i| (h32(k, i) % 360) as f32 * PI / 180.0)
        .collect()
}

fn polar(x: f32, y: f32) -> (f32, f32) {
    let dx = x - CELL_CENTER;
    let dy = y - CELL_CENTER;
    (hypot(dx, dy), det_atan2(dy, dx))
}

fn hypot(dx: f32, dy: f32) -> f32 {
    det_powf_nonnegative(dx * dx + dy * dy, 0.5)
}

fn fbm(x: f32, y: f32, seed: i32) -> f32 {
    0.55 * vnoise(x, y, seed)
        + 0.3 * vnoise(x * 2.1, y * 2.1, seed + 1)
        + 0.15 * vnoise(x * 4.3, y * 4.3, seed + 2)
}

fn vnoise(x: f32, y: f32, seed: i32) -> f32 {
    let ix = det_floor(x);
    let iy = det_floor(y);
    let mut fx = x - ix as f32;
    let mut fy = y - iy as f32;
    fx = fx * fx * (3.0 - 2.0 * fx);
    fy = fy * fy * (3.0 - 2.0 * fy);
    let g = |a: i32, b: i32| noise_hash(a, b, seed);
    g(ix, iy) * (1.0 - fx) * (1.0 - fy)
        + g(ix + 1, iy) * fx * (1.0 - fy)
        + g(ix, iy + 1) * (1.0 - fx) * fy
        + g(ix + 1, iy + 1) * fx * fy
}

/// Floor toward −∞ for a finite value. A non-finite value is 0.
fn det_floor(v: f32) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    let truncated = v.trunc();
    let floored = if v.is_sign_negative() && v != truncated {
        truncated - 1.0
    } else {
        truncated
    };
    floored as i32
}

fn pos_mod(value: f32, modulus: f32) -> f32 {
    let remainder = value % modulus;
    if remainder < 0.0 {
        remainder + modulus
    } else {
        remainder
    }
}

/// `x = (a * 2654435761) xor b`, then `(x * 2246822519) >> 16`, all wrapping.
fn h32(a: u32, b: u32) -> u32 {
    let mixed = a.wrapping_mul(2_654_435_761) ^ b;
    mixed.wrapping_mul(2_246_822_519) >> 16
}

fn noise_hash(a: i32, b: i32, seed: i32) -> f32 {
    let mixed = i64::from(a).wrapping_mul(73_856_093) ^ i64::from(b).wrapping_mul(19_349_663);
    (h32(mixed as u32, seed as u32) as f32) / 65535.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texel(atlas: &[u8], x: u32, y: u32) -> [u8; 4] {
        let side = DAMAGE_DECAL_ATLAS_SIZE as usize;
        let i = (y as usize * side + x as usize) * 4;
        [atlas[i], atlas[i + 1], atlas[i + 2], atlas[i + 3]]
    }

    #[test]
    fn damage_atlas_cells_cover_holes_metal_and_rivets() {
        let atlas = generate_damage_decal_atlas();
        assert_eq!(
            atlas.len(),
            (DAMAGE_DECAL_ATLAS_SIZE * DAMAGE_DECAL_ATLAS_SIZE * 4) as usize
        );
        let hole = texel(&atlas, 128, 128);
        assert!(hole[0] < 20 && hole[1] < 20 && hole[2] < 20, "{hole:?}");
        assert_eq!(hole[3], 255);
        assert_eq!(texel(&atlas, 0, 0)[3], 0, "the corner of a bullet cell is empty");

        // Weld cell 12 starts at (0, 768). Local (100, 100) is the patch interior.
        let weld = texel(&atlas, 100, 868);
        assert!(weld[3] > 200, "weld interior coverage {weld:?}");

        // Rivet cell 15 starts at (768, 768). The first rivet is local (22, 128).
        let rivet = texel(&atlas, 768 + 22, 768 + 128);
        assert_eq!(rivet[3], 255, "rivet {rivet:?}");
        let gap = texel(&atlas, 768 + 37, 768 + 128);
        assert_eq!(gap[3], 0, "between rivets {gap:?}");

        let mut covered = 0u32;
        let mut clear = 0u32;
        let mut bright = 0u32;
        for y in 0..DAMAGE_DECAL_CELL {
            for x in 0..DAMAGE_DECAL_CELL {
                let px = texel(&atlas, 512 + x, 256 + y);
                if px[3] == 0 {
                    clear += 1;
                } else {
                    covered += 1;
                    if px[0] > 180 {
                        bright += 1;
                    }
                }
            }
        }
        assert!(covered > 0, "a scratch cell has coverage");
        assert!(clear > 0, "a scratch cell is not a solid block");
        assert!(bright > 0, "scratch metal is bright");
    }

    #[test]
    fn damage_atlas_is_the_same_bytes_twice() {
        let first = generate_damage_decal_atlas();
        let second = generate_damage_decal_atlas();
        assert_eq!(first, second);
        assert_eq!(
            damage_decal_atlas_fnv1a64(&first),
            damage_decal_atlas_fnv1a64(&second)
        );
    }
}
