// SPDX-License-Identifier: MPL-2.0
//! Procedural decal atlas: four 64×64 layers in one `texture_2d_array`.
//!
//! Scorch, pad markings, ruts and grime are generated on the CPU; authored images are
//! appended by [`crate::decal_texture::DecalTextureAtlas`]. The generator is pure math on the deterministic kernels
//! ([`regia_core::math::det_powf_nonnegative`], integer hashes only), so the atlas is byte-stable across
//! platforms and the offscreen shots can rely on it.
//!
//! Layers: 0 scorch (soft blob), 1 pad (hard painted bar), 2 rut (groove along V), 3 grime (noise).
//! RGB is authored sRGB (sampled through an `Rgba8UnormSrgb` view, so the shader sees
//! linear colour). Alpha is the coverage. Near-white RGB lets the prefab albedo through;
//! a darker texel multiplies that albedo down, which is the scorch core and the rut groove.

use regia_core::math::det_powf_nonnegative;

/// Texels on one side of a layer.
pub const DECAL_ATLAS_SIZE: u32 = 64;
/// Array layers.
pub const DECAL_ATLAS_LAYERS: u32 = 4;

/// RGBA8 bytes, layer-major, row-major, top-left origin. One layer is `SIZE * SIZE * 4` bytes.
pub fn generate_decal_atlas() -> Vec<u8> {
    let layer = (DECAL_ATLAS_SIZE * DECAL_ATLAS_SIZE * 4) as usize;
    let mut rgba = vec![0u8; layer * DECAL_ATLAS_LAYERS as usize];
    for layer_i in 0..DECAL_ATLAS_LAYERS {
        for y in 0..DECAL_ATLAS_SIZE {
            for x in 0..DECAL_ATLAS_SIZE {
                let u = (x as f32 + 0.5) / DECAL_ATLAS_SIZE as f32;
                let v = (y as f32 + 0.5) / DECAL_ATLAS_SIZE as f32;
                let (rgb, a) = texel(layer_i, u, v);
                let idx = (layer_i as usize * layer) + ((y * DECAL_ATLAS_SIZE + x) * 4) as usize;
                rgba[idx] = rgb[0];
                rgba[idx + 1] = rgb[1];
                rgba[idx + 2] = rgb[2];
                rgba[idx + 3] = a;
            }
        }
    }
    rgba
}

fn texel(layer: u32, u: f32, v: f32) -> ([u8; 3], u8) {
    let white = [255u8, 255, 255];
    match layer {
        0 => {
            // Soft scorch: noisy radial alpha, a charred core and a lighter ash rim.
            let dx = u * 2.0 - 1.0;
            let dy = v * 2.0 - 1.0;
            let r = (dx * dx + dy * dy).sqrt();
            let n = hash01(u, v);
            // The deterministic kernel: platform `powf` last digits would store different bytes.
            let a = det_powf_nonnegative((1.0 - r).clamp(0.0, 1.0) * (0.75 + 0.25 * n), 1.4);
            let core = (1.0 - r).clamp(0.0, 1.0);
            let byte = (255.0 - core * 150.0) as u8;
            (
                [byte, (byte as u16 * 9 / 10) as u8, (byte as u16 * 8 / 10) as u8],
                (a * 255.0) as u8,
            )
        }
        1 => {
            // Crisp pad bar: a hard rectangle inset from the edges. Alpha is 0 or 255.
            // RGB stays white so the prefab albedo is the paint colour.
            let inside = (0.18..0.82).contains(&u) && (0.28..0.72).contains(&v);
            (white, if inside { 255 } else { 0 })
        }
        2 => {
            // Rut groove: a darker band across U (the tyre) running the full V.
            let across = (u - 0.5).abs();
            let (shade, a) = if across < 0.22 {
                (0.45, 1.0 - det_powf_nonnegative(across / 0.22, 1.6) * 0.15)
            } else if across < 0.38 {
                (0.75, (1.0 - (across - 0.22) / 0.16) * 0.55)
            } else {
                (1.0, 0.0)
            };
            let byte = (shade * 255.0) as u8;
            (
                [byte, (byte as u16 * 9 / 10) as u8, (byte as u16 * 8 / 10) as u8],
                (a * 255.0) as u8,
            )
        }
        _ => {
            let n = hash01(u * 3.1, v * 2.7);
            let n2 = hash01(u * 9.0 + 0.2, v * 8.0);
            let blob = (n * 0.7 + n2 * 0.3 - 0.35).clamp(0.0, 1.0);
            let shade = 0.55 + 0.45 * n2;
            let byte = (shade * 255.0) as u8;
            ([byte, byte, (byte as u16 * 9 / 10) as u8], (blob * 220.0) as u8)
        }
    }
}

/// Pages per layer of the forward-decal atlas ([`generate_decal_atlas_pages`]).
pub const DECAL_ATLAS_PAGES: u32 = 3;

/// The forward-decal atlas (ADR-3255): per layer three RGBA8 pages, page-major, each
/// [`DECAL_ATLAS_SIZE`]² texels, row-major, top-left origin.
///
/// 1. Albedo + coverage: the bytes of [`generate_decal_atlas`] (sRGB-encoded colour).
/// 2. Normal: the tangent-space normal of the layer's luma read as a height (dark is low), the
///    same one-texel slope the projected decal pass takes at run time, scaled 0.95, stored as
///    `xy * 0.5 + 0.5` (B holds `z`, A is 255).
/// 3. Roughness/metallic multipliers: R and G store `scale / 2` (128 is ×1.0). Scorch is rougher
///    in its char core (×1.0 to ×1.05), grime varies ×0.9 to ×1.1 with its noise, pad paint and
///    rut keep ×1.0; metallic stays ×1.0.
#[must_use]
pub fn generate_decal_atlas_pages() -> Vec<u8> {
    let albedo = generate_decal_atlas();
    let size = DECAL_ATLAS_SIZE as usize;
    let page = size * size * 4;
    let mut out = vec![0u8; page * (DECAL_ATLAS_PAGES * DECAL_ATLAS_LAYERS) as usize];
    let luma = |layer: usize, x: usize, y: usize| -> f32 {
        let i = layer * page + (y.min(size - 1) * size + x.min(size - 1)) * 4;
        let lin = |b: u8| {
            let c = f32::from(b) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(albedo[i]) + 0.7152 * lin(albedo[i + 1]) + 0.0722 * lin(albedo[i + 2])
    };
    let unorm = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    for layer in 0..DECAL_ATLAS_LAYERS as usize {
        let base = layer * DECAL_ATLAS_PAGES as usize * page;
        out[base..base + page].copy_from_slice(&albedo[layer * page..(layer + 1) * page]);
        for y in 0..size {
            for x in 0..size {
                let h = luma(layer, x, y);
                // `auv.y` grows downward, so +v is −up: the slope along v flips sign (as the
                // decal pass does).
                let du = (luma(layer, x + 1, y) - h) * size as f32;
                let dv = (luma(layer, x, y + 1) - h) * size as f32;
                let (nx, ny, nz) = (-du * 0.95, dv * 0.95, 1.0f32);
                let len = (nx * nx + ny * ny + nz * nz).sqrt();
                let i = base + page + (y * size + x) * 4;
                out[i] = unorm(nx / len * 0.5 + 0.5);
                out[i + 1] = unorm(ny / len * 0.5 + 0.5);
                out[i + 2] = unorm(nz / len);
                out[i + 3] = 255;
                let u = (x as f32 + 0.5) / size as f32;
                let v = (y as f32 + 0.5) / size as f32;
                let rough = match layer {
                    0 => {
                        let r = ((u * 2.0 - 1.0).powi(2) + (v * 2.0 - 1.0).powi(2)).sqrt();
                        1.0 + 0.05 * (1.0 - r).clamp(0.0, 1.0)
                    }
                    3 => 0.9 + 0.2 * hash01(u * 9.0 + 0.2, v * 8.0),
                    _ => 1.0,
                };
                let m = base + 2 * page + (y * size + x) * 4;
                out[m] = unorm(rough / 2.0);
                out[m + 1] = 128;
                out[m + 2] = 255;
                out[m + 3] = 255;
            }
        }
    }
    out
}

/// Cheap deterministic value in `0..1` (integer hash, no trig).
fn hash01(u: f32, v: f32) -> f32 {
    let x = (u * 4096.0) as u32;
    let y = (v * 4096.0) as u32;
    let mut n = x.wrapping_mul(374761393).wrapping_add(y.wrapping_mul(668265263));
    n = (n ^ (n >> 13)).wrapping_mul(1274126177);
    (n & 0xffff) as f32 / 65535.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decal_atlas_bytes_match_a_pinned_hash() {
        // Every texel curve runs on the deterministic kernel (det_powf_nonnegative) or integer math, so a
        // committed FNV-1a hash pins all layer bytes on every platform. Update the constant
        // only with a reason in the commit message.
        let atlas = generate_decal_atlas();
        assert_eq!(
            atlas.len(),
            (DECAL_ATLAS_SIZE * DECAL_ATLAS_SIZE * 4 * DECAL_ATLAS_LAYERS) as usize
        );
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for b in &atlas {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        assert_eq!(hash, DECAL_ATLAS_FNV1A, "decal atlas bytes changed");
        // Scorch alpha sits near byte boundaries at mid radius: pin a few coverage texels.
        let layer = (DECAL_ATLAS_SIZE * DECAL_ATLAS_SIZE * 4) as usize;
        let alpha = |layer_i: u32, x: u32, y: u32| {
            atlas[layer_i as usize * layer + ((y * DECAL_ATLAS_SIZE + x) * 4 + 3) as usize]
        };
        assert_eq!(
            [alpha(0, 32, 32), alpha(0, 48, 32), alpha(2, 32, 32)],
            SCORCH_RUT_PINNED
        );
    }

    // Pinned bytes of the deterministic generator (see the test above).
    const DECAL_ATLAS_FNV1A: u64 = 0x2aad19111f2001b4; // pinned 2026-11-09 (deterministic-kernel generator)
    const SCORCH_RUT_PINNED: [u8; 3] = [171, 74, 254]; // pinned 2026-11-09

    #[test]
    fn pad_layer_is_binary_and_scorch_is_soft() {
        let atlas = generate_decal_atlas();
        let layer = (DECAL_ATLAS_SIZE * DECAL_ATLAS_SIZE * 4) as usize;
        let alpha = |layer_i: u32, x: u32, y: u32| {
            let idx = (layer_i as usize * layer) + ((y * DECAL_ATLAS_SIZE + x) * 4) as usize;
            atlas[idx + 3]
        };
        // Centre of the pad bar is opaque; a corner is empty. No partial texels.
        assert_eq!(alpha(1, 32, 32), 255);
        assert_eq!(alpha(1, 1, 1), 0);
        let mut partial = 0u32;
        for y in 0..DECAL_ATLAS_SIZE {
            for x in 0..DECAL_ATLAS_SIZE {
                let a = alpha(1, x, y);
                if a != 0 && a != 255 {
                    partial += 1;
                }
            }
        }
        assert_eq!(partial, 0, "pad coverage is binary so the mark stays crisp");
        // Scorch centre is stronger than a mid-radius texel, and the mid texel is not binary.
        let centre = alpha(0, 32, 32);
        let mid = alpha(0, 48, 32);
        assert!(
            centre > mid && mid > 0,
            "scorch falls off: center {centre} mid {mid}"
        );
    }

    #[test]
    fn pages_hold_the_albedo_a_luma_normal_and_neutral_multipliers() {
        let pages = generate_decal_atlas_pages();
        let page = (DECAL_ATLAS_SIZE * DECAL_ATLAS_SIZE * 4) as usize;
        assert_eq!(pages.len(), page * 12);
        let albedo = generate_decal_atlas();
        for layer in 0..4 {
            assert_eq!(
                &pages[layer * 3 * page..][..page],
                &albedo[layer * page..][..page]
            );
        }
        // Pad paint is flat inside the bar: its normal page is +Z there; its multipliers are ×1.
        let pad_normal = &pages[(3 + 1) * page..][..page];
        let centre = (32 * 64 + 32) * 4;
        assert_eq!(&pad_normal[centre..centre + 3], &[128, 128, 255]);
        assert_eq!(pages[(3 + 2) * page + centre], 128);
        // The scorch bowl tilts its rim normals toward the centre-out slope: left of centre the
        // luma rises leftward (darker core), so x is positive there.
        let scorch_normal = &pages[page..][..page];
        let left = (32 * 64 + 16) * 4;
        assert!(scorch_normal[left] > 128, "{}", scorch_normal[left]);
    }
}
