// SPDX-License-Identifier: MIT
// Portions ported from Wicked Engine (https://github.com/turanszkij/WickedEngine, commit 7f776d0),
// Copyright (c) 2026 Turánszki János, MIT licence.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
// IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
// OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE
// OR OTHER DEALINGS IN THE SOFTWARE.

//! Octahedral impostors for far scatter (ADR-116): each scatter prototype is captured from a
//! hemisphere of view directions into an atlas (albedo + coverage, object-space normal +
//! depth), and instances past their class's mesh range draw as one camera-facing card that
//! blends the three captures nearest the view direction.
//!
//! - The capture layout (one colour, one normal, one surface image per capture, sampled
//!   together at draw time) follows Wicked Engine's `captureImpostorPS.hlsl` /
//!   `impostorPS.hlsl`; Wicked captures a ring of yaw angles, Regia Engine a hemi-octahedral grid
//!   of view directions (Cigolle et al., "A Survey of Efficient Representations for
//!   Independent Unit Vectors", JCGT 3(2), 2014, § 3.2 hemi-octahedral map), so a rock seen
//!   from a ridge above uses a capture from above.
//! - The capture is a deterministic CPU rasteriser (2×2 supersampled, orthographic), so the
//!   atlas is bit-identical on every GPU and platform; transparent texels are dilated from
//!   their covered neighbours and the mips keep the mip-0 alpha-test coverage (Castaño,
//!   "Computing Alpha Mipmaps", 2010), so far cards neither grow dark fringes nor thin out.
//! - [`ImpostorInstance`] is what the GPU culls and draws (see `gpu/impostor_cull.wgsl`, a port
//!   of Wicked's `impostor_prepareCS.hlsl`, and `gpu/impostor.wgsl`).

use regia_core::glam::{Quat, Vec3};
use regia_render::{LabMaterial, LabTexture, LabVertex};

/// Settings of the impostor far field ([`super::field::ScatterField::set_impostors`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImpostorSettings {
    /// Captures per side of the hemi-octahedral grid (`grid²` view directions).
    pub grid: u32,
    /// Pixels per side of one capture (a power of two).
    pub cell: u32,
    /// Farthest any class draws impostors, metres.
    pub far_end_m: f32,
    /// Projected radius (pixels) where an impostor has shrunk out; it shrinks from twice
    /// this size down to it (sinking about its buried pivot, like the mesh distance fade).
    pub min_pixels: f32,
    /// Screen height (pixels) the per-class far range is planned for (placement range only;
    /// the draw uses the frame's real height).
    pub reference_height_px: f32,
    /// Vertical field of view (radians) the per-class far range is planned for.
    pub reference_fov_y: f32,
}

impl Default for ImpostorSettings {
    fn default() -> Self {
        Self {
            grid: 8,
            cell: 32,
            far_end_m: 2000.0,
            min_pixels: 0.75,
            reference_height_px: 1440.0,
            reference_fov_y: 50f32.to_radians(),
        }
    }
}

impl ImpostorSettings {
    /// The settings with `grid` and `cell` clamped to what the bake supports (grid 2..=16,
    /// cell a power of two in 8..=128).
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.grid = self.grid.clamp(2, 16);
        self.cell = self.cell.clamp(8, 128).next_power_of_two().min(128);
        self.far_end_m = if self.far_end_m.is_finite() {
            self.far_end_m.max(1.0)
        } else {
            2000.0
        };
        self.min_pixels = if self.min_pixels.is_finite() {
            self.min_pixels.max(0.05)
        } else {
            0.75
        };
        self.reference_height_px = self.reference_height_px.max(1.0);
        self.reference_fov_y = self.reference_fov_y.clamp(0.05, 3.0);
        self
    }

    /// Mip levels of one capture (down to 1×1).
    #[must_use]
    pub fn mip_count(&self) -> u32 {
        self.cell.max(1).ilog2() + 1
    }

    /// Distance (metres) where a sphere of `radius` metres projects to `min_pixels` at the
    /// reference resolution and field of view.
    #[must_use]
    pub fn size_limit_distance(&self, radius: f32) -> f32 {
        let inv_tan = 1.0 / (self.reference_fov_y * 0.5).tan().max(1e-4);
        radius * inv_tan * self.reference_height_px * 0.5 / self.min_pixels
    }
}

/// Hemi-octahedral encoding of a unit direction with `y ≥ 0` (Y up) into `[-1, 1]²`
/// (Cigolle et al. 2014). The horizon maps to the square's border, +Y to its centre.
#[must_use]
pub fn hemi_oct_encode(d: [f32; 3]) -> [f32; 2] {
    let s = d[0].abs() + d[1].max(0.0) + d[2].abs();
    let s = if s > 0.0 { s } else { 1.0 };
    let (px, pz) = (d[0] / s, d[2] / s);
    [px + pz, px - pz]
}

/// Inverse of [`hemi_oct_encode`]: the unit direction of `e ∈ [-1, 1]²`.
#[must_use]
pub fn hemi_oct_decode(e: [f32; 2]) -> [f32; 3] {
    let px = (e[0] + e[1]) * 0.5;
    let pz = (e[0] - e[1]) * 0.5;
    let y = 1.0 - px.abs() - pz.abs();
    let v = Vec3::new(px, y.max(0.0), pz);
    v.normalize_or(Vec3::Y).to_array()
}

/// View direction (from the object toward the camera, object space) of capture `(i, j)` of a
/// `grid × grid` atlas: the capture's cell centre decoded.
#[must_use]
pub fn capture_direction(i: u32, j: u32, grid: u32) -> [f32; 3] {
    let g = grid.max(1) as f32;
    hemi_oct_decode([(i as f32 + 0.5) / g * 2.0 - 1.0, (j as f32 + 0.5) / g * 2.0 - 1.0])
}

/// The capture's orthonormal image basis `(right, up, forward)`; `forward` points from the
/// object toward the camera. Shared with `impostor.wgsl` (`capture_basis`).
#[must_use]
pub fn capture_basis(forward: [f32; 3]) -> ([f32; 3], [f32; 3], [f32; 3]) {
    let f = Vec3::from_array(forward).normalize_or(Vec3::Y);
    let mut r = Vec3::Y.cross(f);
    if r.length_squared() < 1e-8 {
        r = Vec3::X;
    }
    let r = r.normalize();
    let u = f.cross(r);
    (r.to_array(), u.to_array(), f.to_array())
}

/// One baked prototype: `mips` levels of the albedo and normal-depth images, each
/// `grid · cell >> level` pixels square, RGBA8.
#[derive(Clone, Debug, PartialEq)]
pub struct ImpostorCard {
    /// Bounding-sphere centre height about the pivot (object space, metres).
    pub centre_y: f32,
    /// Bounding-sphere radius (object space, metres).
    pub radius: f32,
    /// Albedo per mip: sRGB-encoded `min(colour, 2) / 2` (vertex colour × albedo texture; the
    /// material base colour multiplies it at draw time), alpha = coverage.
    pub albedo: Vec<Vec<u8>>,
    /// Normal-depth per mip: RGB = object-space normal `n · 0.5 + 0.5`, A = depth toward the
    /// camera `h · 0.5 + 0.5` with `h` in bounding radii from the centre plane.
    pub normal_depth: Vec<Vec<u8>>,
}

/// Per-prototype draw parameters (`ImpProto` in `impostor_common.wgsl`; 80 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ImpostorProto {
    /// RGB = material base colour × class tint (linear), A = class `tint_jitter`.
    pub tint: [f32; 4],
    /// x = card (atlas layer), y = bounding-sphere centre height, z = bounding radius (object
    /// space), w = perceptual roughness.
    pub card: [f32; 4],
    /// x = mesh fade start (impostor cross-fade begins), y = mesh fade end (impostor whole),
    /// z = impostor far end, w = metallic.
    pub range: [f32; 4],
    /// x = tint variants, y = the card a leafless instance draws (atlas layer; negative: it draws
    /// none, ADR-3146), zw unused.
    pub extra: [f32; 4],
    /// RGB = the class season's tint on the leaf card, A = its density ([`crate::ScatterSeason`]; `1` for
    /// all four without a season). Rewritten on a season change; instances stay as they are.
    pub season: [f32; 4],
}

/// One instance the GPU culls and draws as an impostor (`ImpInstance` in `impostor.wgsl`;
/// 48 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ImpostorInstance {
    /// Bounding-sphere centre (world, unseated) — the value the mesh path measures its
    /// cross-fade distance from, so both sides agree.
    pub centre: [f32; 3],
    /// Bounding radius, metres.
    pub radius: f32,
    /// Model origin (the buried pivot), world metres.
    pub pivot: [f32; 3],
    /// Uniform scale.
    pub scale: f32,
    /// Rotation quaternion `xyzw`, each component as snorm16, two per word.
    pub rotation: [u32; 2],
    /// Prototype index (low 16 bits), tint variant (bits 16..24).
    pub packed: u32,
    /// The instance's place in the leaf order, 24 bits ([`crate::scatter::season::leaf_rank`]): the GPU
    /// compares it with the prototype's season density.
    pub leaf_rank: u32,
}

impl ImpostorInstance {
    /// Packs one instance from its model matrix (uniform scale × rotation, translation).
    #[must_use]
    pub fn from_model(model: &[[f32; 4]; 4], centre: [f32; 3], radius: f32, proto: u32, variant: u8) -> Self {
        let m = regia_core::glam::Mat4::from_cols_array_2d(model);
        let (scale, rotation, translation) = m.to_scale_rotation_translation();
        let q = rotation.normalize();
        let s16 = |v: f32| ((v.clamp(-1.0, 1.0) * 32767.0).round() as i16) as u16 as u32;
        Self {
            centre,
            radius,
            pivot: translation.to_array(),
            scale: scale.x.max(1e-6),
            rotation: [s16(q.x) | s16(q.y) << 16, s16(q.z) | s16(q.w) << 16],
            packed: (proto & 0xffff) | u32::from(variant) << 16,
            leaf_rank: 0,
        }
    }

    /// This instance with its place in the leaf order ([`crate::scatter::season::leaf_rank`]).
    #[must_use]
    pub fn with_leaf_rank(mut self, rank: u32) -> Self {
        self.leaf_rank = rank;
        self
    }

    /// The packed rotation, unpacked and normalised.
    #[must_use]
    pub fn quat(&self) -> Quat {
        let u = |w: u32, hi: bool| {
            let bits = if hi { w >> 16 } else { w & 0xffff } as u16 as i16;
            (f32::from(bits) / 32767.0).max(-1.0)
        };
        Quat::from_xyzw(
            u(self.rotation[0], false),
            u(self.rotation[0], true),
            u(self.rotation[1], false),
            u(self.rotation[1], true),
        )
        .normalize()
    }
}

/// Everything the GPU layer uploads: the cards and the per-prototype table.
#[derive(Clone, Debug, PartialEq)]
pub struct ImpostorAtlas {
    /// The settings the cards were baked with ([`ImpostorSettings::sanitized`]).
    pub settings: ImpostorSettings,
    /// Baked cards (atlas layers).
    pub cards: Vec<ImpostorCard>,
    /// Per field prototype, in field order.
    pub protos: Vec<ImpostorProto>,
}

impl ImpostorAtlas {
    /// Side of one atlas layer at mip 0, pixels.
    #[must_use]
    pub fn size(&self) -> u32 {
        self.settings.grid * self.settings.cell
    }

    /// GPU bytes of the two texture arrays (all mips).
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.cards
            .iter()
            .map(|c| {
                c.albedo
                    .iter()
                    .chain(&c.normal_depth)
                    .map(Vec::len)
                    .sum::<usize>()
            })
            .sum()
    }
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn unorm(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Bilinear sample of an RGBA8 texture with wrap addressing, bytes / 255.
fn sample_texture(t: &LabTexture, uv: [f32; 2]) -> [f32; 4] {
    let (w, h) = (t.width.max(1) as i64, t.height.max(1) as i64);
    // u128 so 2^31 x 2^31 x 4 cannot wrap to a small number and let the reads run past `rgba`.
    if (t.rgba.len() as u128) < (w as u128) * (h as u128) * 4 {
        return [1.0; 4];
    }
    let x = uv[0] * w as f32 - 0.5;
    let y = uv[1] * h as f32 - 0.5;
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let texel = |xi: i64, yi: i64| -> [f32; 4] {
        let xi = xi.rem_euclid(w);
        let yi = yi.rem_euclid(h);
        let o = ((yi * w + xi) * 4) as usize;
        [0, 1, 2, 3].map(|k| f32::from(t.rgba[o + k]) / 255.0)
    };
    let (x0, y0) = (x0 as i64, y0 as i64);
    let a = texel(x0, y0);
    let b = texel(x0 + 1, y0);
    let c = texel(x0, y0 + 1);
    let d = texel(x0 + 1, y0 + 1);
    [0, 1, 2, 3].map(|k| {
        let top = a[k] + (b[k] - a[k]) * fx;
        let bottom = c[k] + (d[k] - c[k]) * fx;
        top + (bottom - top) * fy
    })
}

/// One rasterised sample of a capture before encoding.
#[derive(Clone, Copy, Default)]
struct Texel {
    /// Linear albedo (vertex colour × texture).
    albedo: [f32; 3],
    normal: [f32; 3],
    /// Depth toward the camera, bounding radii.
    depth: f32,
    covered: bool,
}

/// Rasterises a mesh's `parts` seen along `forward` into a `size²` orthographic image of the
/// sphere `(centre, radius)`; keeps the sample nearest the camera. An alpha-masked material's holes
/// stay holes, as the alpha test leaves them in the frame.
fn rasterise(
    parts: &[ImpostorPart<'_>],
    centre: Vec3,
    radius: f32,
    forward: [f32; 3],
    size: u32,
) -> Vec<Texel> {
    let mut out = vec![Texel::default(); (size as usize) * (size as usize)];
    let mut zbuf = vec![f32::NEG_INFINITY; out.len()];
    for part in parts {
        rasterise_part(part, centre, radius, forward, size, &mut out, &mut zbuf);
    }
    out
}

fn rasterise_part(
    part: &ImpostorPart<'_>,
    centre: Vec3,
    radius: f32,
    forward: [f32; 3],
    size: u32,
    out: &mut [Texel],
    zbuf: &mut [f32],
) {
    let ImpostorPart {
        vertices,
        indices,
        material,
        colour: part_colour,
    } = *part;
    let mask = (material.alpha_mode == regia_render::ALPHA_MASK).then_some(material.alpha_cutoff);
    let (r, u, f) = capture_basis(forward);
    let (r, u, f) = (Vec3::from_array(r), Vec3::from_array(u), Vec3::from_array(f));
    let n = size as usize;
    let s = size as f32;
    // Projected position (pixels, y down) and depth (radii toward the camera).
    let project = |p: [f32; 3]| {
        let l = Vec3::from_array(p) - centre;
        let x = l.dot(r) / radius;
        let y = l.dot(u) / radius;
        let z = l.dot(f) / radius;
        [(x * 0.5 + 0.5) * s, (0.5 - y * 0.5) * s, z]
    };
    let normal_map = material
        .normal_map
        .as_ref()
        .filter(|_| material.normal_scale != 0.0);
    for tri in indices.as_chunks::<3>().0 {
        let (Some(a), Some(b), Some(c)) = (
            vertices.get(tri[0] as usize),
            vertices.get(tri[1] as usize),
            vertices.get(tri[2] as usize),
        ) else {
            continue;
        };
        let (pa, pb, pc) = (project(a.position), project(b.position), project(c.position));
        let area = (pb[0] - pa[0]) * (pc[1] - pa[1]) - (pb[1] - pa[1]) * (pc[0] - pa[0]);
        if area.abs() < 1e-12 {
            continue;
        }
        let min_x = pa[0].min(pb[0]).min(pc[0]).floor().max(0.0) as usize;
        let max_x = (pa[0].max(pb[0]).max(pc[0]).ceil() as usize).min(n);
        let min_y = pa[1].min(pb[1]).min(pc[1]).floor().max(0.0) as usize;
        let max_y = (pa[1].max(pb[1]).max(pc[1]).ceil() as usize).min(n);
        for py in min_y..max_y {
            for px in min_x..max_x {
                let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
                let w0 = ((pb[0] - x) * (pc[1] - y) - (pb[1] - y) * (pc[0] - x)) / area;
                let w1 = ((pc[0] - x) * (pa[1] - y) - (pc[1] - y) * (pa[0] - x)) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let z = w0 * pa[2] + w1 * pb[2] + w2 * pc[2];
                let o = py * n + px;
                if z <= zbuf[o] {
                    continue;
                }
                let lerp3 = |ka: [f32; 3], kb: [f32; 3], kc: [f32; 3]| {
                    [0, 1, 2].map(|k| w0 * ka[k] + w1 * kb[k] + w2 * kc[k])
                };
                let uv = [
                    w0 * a.uv[0] + w1 * b.uv[0] + w2 * c.uv[0],
                    w0 * a.uv[1] + w1 * b.uv[1] + w2 * c.uv[1],
                ];
                let colour = lerp3(
                    [a.color[0], a.color[1], a.color[2]],
                    [b.color[0], b.color[1], b.color[2]],
                    [c.color[0], c.color[1], c.color[2]],
                );
                let sampled = material.albedo.as_ref().map(|t| sample_texture(t, uv));
                if let (Some(cutoff), Some(s)) = (mask, sampled)
                    && s[3] * material.base_color[3] < cutoff
                {
                    continue;
                }
                zbuf[o] = z;
                let tex = sampled.map_or([1.0; 3], |s| {
                    [srgb_to_linear(s[0]), srgb_to_linear(s[1]), srgb_to_linear(s[2])]
                });
                let mut nrm = Vec3::from_array(lerp3(a.normal, b.normal, c.normal)).normalize_or(f);
                if let Some(map) = normal_map {
                    let t4 = [0, 1, 2, 3].map(|k| w0 * a.tangent[k] + w1 * b.tangent[k] + w2 * c.tangent[k]);
                    let t = Vec3::new(t4[0], t4[1], t4[2]);
                    if t.length_squared() > 1e-8 {
                        let t = (t - nrm * nrm.dot(t)).normalize_or(Vec3::X);
                        let bsign = if t4[3] < 0.0 { -1.0 } else { 1.0 };
                        let bt = nrm.cross(t) * bsign;
                        let s = sample_texture(map, uv);
                        let tn = Vec3::new(
                            (s[0] * 2.0 - 1.0) * material.normal_scale,
                            (s[1] * 2.0 - 1.0) * material.normal_scale,
                            s[2] * 2.0 - 1.0,
                        );
                        nrm = (t * tn.x + bt * tn.y + nrm * tn.z).normalize_or(nrm);
                    }
                }
                out[o] = Texel {
                    albedo: [0, 1, 2].map(|k| colour[k] * tex[k] * part_colour[k]),
                    normal: nrm.to_array(),
                    depth: z,
                    covered: true,
                };
            }
        }
    }
}

/// Encoded mip-0 capture (`cell²` RGBA8 albedo and normal-depth) from a supersampled raster.
fn resolve(samples: &[Texel], ss: usize, cell: usize) -> (Vec<[f32; 4]>, Vec<[f32; 4]>) {
    let big = cell * ss;
    let mut albedo = vec![[0.0f32; 4]; cell * cell];
    let mut nd = vec![[0.0f32; 4]; cell * cell];
    for y in 0..cell {
        for x in 0..cell {
            let (mut a, mut n, mut d, mut k) = ([0.0f32; 3], Vec3::ZERO, 0.0f32, 0usize);
            for sy in 0..ss {
                for sx in 0..ss {
                    let t = &samples[(y * ss + sy) * big + x * ss + sx];
                    if t.covered {
                        mad(&mut a, &t.albedo, 1.0);
                        n += Vec3::from_array(t.normal);
                        d += t.depth;
                        k += 1;
                    }
                }
            }
            let o = y * cell + x;
            if k > 0 {
                let kf = k as f32;
                let n = n.normalize_or(Vec3::Y);
                albedo[o] = [a[0] / kf, a[1] / kf, a[2] / kf, kf / (ss * ss) as f32];
                nd[o] = [n.x, n.y, n.z, d / kf];
            }
        }
    }
    (albedo, nd)
}

/// Fills transparent texels' colour, normal and depth from their covered neighbours (up to
/// `passes` texels out; the rest take the image mean), so bilinear and mip filtering never
/// pull in a meaningless background. Alpha is unchanged.
fn dilate(albedo: &mut [[f32; 4]], nd: &mut [[f32; 4]], side: usize, passes: usize) {
    let mut known: Vec<bool> = albedo.iter().map(|a| a[3] > 0.0).collect();
    for _ in 0..passes {
        let mut next = known.clone();
        let mut changed = false;
        for y in 0..side {
            for x in 0..side {
                let o = y * side + x;
                if known[o] {
                    continue;
                }
                let (mut a, mut n, mut k) = ([0.0f32; 3], [0.0f32; 4], 0.0f32);
                for (dx, dy) in [
                    (-1i64, 0i64),
                    (1, 0),
                    (0, -1),
                    (0, 1),
                    (-1, -1),
                    (1, -1),
                    (-1, 1),
                    (1, 1),
                ] {
                    let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                    if nx < 0 || ny < 0 || nx >= side as i64 || ny >= side as i64 {
                        continue;
                    }
                    let q = ny as usize * side + nx as usize;
                    if known[q] {
                        mad(&mut a, &albedo[q][..3], 1.0);
                        mad(&mut n, &nd[q], 1.0);
                        k += 1.0;
                    }
                }
                if k > 0.0 {
                    set_scaled(&mut albedo[o][..3], &a, 1.0 / k);
                    set_scaled(&mut nd[o], &n, 1.0 / k);
                    next[o] = true;
                    changed = true;
                }
            }
        }
        known = next;
        if !changed {
            break;
        }
    }
    // Whatever is still unknown takes the mean of the known texels.
    let count = known.iter().filter(|k| **k).count();
    if count == 0 || count == known.len() {
        return;
    }
    let (mut a, mut n) = ([0.0f32; 3], [0.0f32; 4]);
    for (o, _) in known.iter().enumerate().filter(|(_, k)| **k) {
        mad(&mut a, &albedo[o][..3], 1.0);
        mad(&mut n, &nd[o], 1.0);
    }
    let k = count as f32;
    for (o, _) in known.iter().enumerate().filter(|(_, k)| !**k) {
        set_scaled(&mut albedo[o][..3], &a, 1.0 / k);
        set_scaled(&mut nd[o], &n, 1.0 / k);
    }
}

/// `acc += v · w`, channel by channel.
fn mad(acc: &mut [f32], v: &[f32], w: f32) {
    for (a, x) in acc.iter_mut().zip(v) {
        *a += x * w;
    }
}

/// `out = v · k`, channel by channel.
fn set_scaled(out: &mut [f32], v: &[f32], k: f32) {
    for (o, x) in out.iter_mut().zip(v) {
        *o = x * k;
    }
}

/// Fraction of `alpha` values at or above 0.5 after scaling by `k`.
fn coverage(alpha: &[f32], k: f32) -> f32 {
    alpha.iter().filter(|a| **a * k >= 0.5).count() as f32 / alpha.len().max(1) as f32
}

/// The scale that makes `alpha`'s alpha-test coverage closest to `target` (bisection on the
/// monotone coverage curve; Castaño 2010).
fn coverage_scale(alpha: &[f32], target: f32) -> f32 {
    let (mut lo, mut hi) = (0.0f32, 8.0f32);
    for _ in 0..24 {
        let mid = 0.5 * (lo + hi);
        if coverage(alpha, mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi
}

/// Bakes one capture image: `grid²` captures of `cell²`, the mip chain per capture with
/// alpha-test coverage preserved, encoded to RGBA8.
fn bake_card(
    parts: &[ImpostorPart<'_>],
    centre_y: f32,
    radius: f32,
    settings: &ImpostorSettings,
) -> ImpostorCard {
    let grid = settings.grid as usize;
    let cell = settings.cell as usize;
    let side = grid * cell;
    let mips = settings.mip_count() as usize;
    const SS: usize = 2;
    let centre = Vec3::new(0.0, centre_y, 0.0);
    // Per capture, per mip: linear float images (albedo, normal-depth).
    let mut levels_albedo: Vec<Vec<[f32; 4]>> = (0..mips)
        .map(|m| vec![[0.0; 4]; (side >> m) * (side >> m)])
        .collect();
    let mut levels_nd: Vec<Vec<[f32; 4]>> = levels_albedo.clone();
    for j in 0..grid {
        for i in 0..grid {
            let forward = capture_direction(i as u32, j as u32, grid as u32);
            let raster = rasterise(parts, centre, radius, forward, (cell * SS) as u32);
            let (mut albedo, mut nd) = resolve(&raster, SS, cell);
            let target = coverage(&albedo.iter().map(|a| a[3]).collect::<Vec<_>>(), 1.0);
            dilate(&mut albedo, &mut nd, cell, cell);
            let mut level_a = albedo;
            let mut level_n = nd;
            let mut c = cell;
            for m in 0..mips {
                // Write this level into the atlas at capture (i, j).
                let atlas_side = side >> m;
                for y in 0..c {
                    for x in 0..c {
                        let o = (j * c + y) * atlas_side + i * c + x;
                        levels_albedo[m][o] = level_a[y * c + x];
                        levels_nd[m][o] = level_n[y * c + x];
                    }
                }
                if m + 1 == mips {
                    break;
                }
                // Box-filter down: colour weighted by alpha (dilated texels carry weight
                // through a floor so a fully transparent quad keeps its dilated colour).
                let h = c / 2;
                let mut next_a = vec![[0.0f32; 4]; h * h];
                let mut next_n = vec![[0.0f32; 4]; h * h];
                for y in 0..h {
                    for x in 0..h {
                        let (mut a, mut wsum, mut alpha) = ([0.0f32; 3], 0.0f32, 0.0f32);
                        let mut n = [0.0f32; 4];
                        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                            let s = (2 * y + dy) * c + 2 * x + dx;
                            let w = level_a[s][3].max(1e-3);
                            mad(&mut a, &level_a[s][..3], w);
                            mad(&mut n, &level_n[s], w);
                            wsum += w;
                            alpha += level_a[s][3];
                        }
                        let nn = Vec3::new(n[0], n[1], n[2]).normalize_or(Vec3::Y);
                        next_a[y * h + x] = [a[0] / wsum, a[1] / wsum, a[2] / wsum, alpha * 0.25];
                        next_n[y * h + x] = [nn.x, nn.y, nn.z, n[3] / wsum];
                    }
                }
                // Keep the alpha-test coverage of mip 0.
                let alphas: Vec<f32> = next_a.iter().map(|a| a[3]).collect();
                if target > 0.0 {
                    let k = coverage_scale(&alphas, target);
                    for a in &mut next_a {
                        a[3] = (a[3] * k).min(1.0);
                    }
                }
                level_a = next_a;
                level_n = next_n;
                c = h;
            }
        }
    }
    let encode_albedo = |img: &[[f32; 4]]| -> Vec<u8> {
        img.iter()
            .flat_map(|p| {
                [
                    unorm(linear_to_srgb((p[0] * 0.5).min(1.0))),
                    unorm(linear_to_srgb((p[1] * 0.5).min(1.0))),
                    unorm(linear_to_srgb((p[2] * 0.5).min(1.0))),
                    unorm(p[3]),
                ]
            })
            .collect()
    };
    let encode_nd = |img: &[[f32; 4]]| -> Vec<u8> {
        img.iter()
            .flat_map(|p| {
                [
                    unorm(p[0] * 0.5 + 0.5),
                    unorm(p[1] * 0.5 + 0.5),
                    unorm(p[2] * 0.5 + 0.5),
                    unorm(p[3] * 0.5 + 0.5),
                ]
            })
            .collect()
    };
    ImpostorCard {
        centre_y,
        radius,
        albedo: levels_albedo.iter().map(|l| encode_albedo(l)).collect(),
        normal_depth: levels_nd.iter().map(|l| encode_nd(l)).collect(),
    }
}

/// One material of a mesh to bake: LOD 0 geometry and its material.
#[derive(Clone, Copy)]
pub struct ImpostorPart<'a> {
    /// LOD 0 vertices (object space, pivot at the base centre).
    pub vertices: &'a [LabVertex],
    /// Triangle list.
    pub indices: &'a [u32],
    /// Material (albedo texture and normal map are baked, an alpha-masked one cuts its holes; factors
    /// apply at draw time).
    pub material: &'a LabMaterial,
    /// This part's colour against the factors the draw table applies: baked in, since one card holds
    /// every part. `[1; 3]` for the part the table's factors come from.
    pub colour: [f32; 3],
}

/// One mesh to bake: its parts and its bounding sphere about the pivot.
pub struct ImpostorSource<'a> {
    /// The mesh's material parts, nearest the camera winning where they overlap.
    pub parts: Vec<ImpostorPart<'a>>,
    /// Bounding-sphere centre height.
    pub centre_y: f32,
    /// Bounding-sphere radius.
    pub radius: f32,
}

/// Bakes one card per source (in parallel on the compute pool when there are several).
#[must_use]
pub fn bake_cards(sources: &[ImpostorSource<'_>], settings: &ImpostorSettings) -> Vec<ImpostorCard> {
    let _span = tracing::info_span!("impostor_bake", cards = sources.len()).entered();
    let settings = settings.sanitized();
    let bake = |s: &ImpostorSource<'_>| bake_card(&s.parts, s.centre_y, s.radius.max(1e-4), &settings);
    if sources.len() < 2 {
        return sources.iter().map(bake).collect();
    }
    let bake = &bake;
    regia_core::tasks::scope(|scope| {
        for source in sources {
            scope.spawn(async move { bake(source) });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scatter::rock::{RockStyle, procedural_rock};

    #[test]
    fn sample_texture_with_wrapping_dimensions_is_neutral() {
        let t = LabTexture {
            width: 1 << 31,
            height: 1 << 31,
            rgba: Vec::new(),
        };
        assert_eq!(sample_texture(&t, [0.5, 0.5]), [1.0; 4]);
    }

    #[test]
    fn hemi_octahedral_round_trip() {
        for i in 0..=16 {
            for j in 0..=16 {
                let e = [i as f32 / 8.0 - 1.0, j as f32 / 8.0 - 1.0];
                let d = hemi_oct_decode(e);
                let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
                assert!((len - 1.0).abs() < 1e-5);
                assert!(d[1] >= 0.0);
                let back = hemi_oct_encode(d);
                assert!(
                    (back[0] - e[0]).abs() < 1e-4 && (back[1] - e[1]).abs() < 1e-4,
                    "{e:?} -> {back:?}"
                );
            }
        }
        assert_eq!(hemi_oct_encode([0.0, 1.0, 0.0]), [0.0, 0.0]);
        // The border is the horizon.
        assert!(hemi_oct_decode([1.0, 0.0])[1].abs() < 1e-6);
        assert!(hemi_oct_decode([-1.0, 0.3])[1].abs() < 1e-6);
    }

    #[test]
    fn capture_basis_is_orthonormal_and_faces_the_camera() {
        for (i, j) in [(0, 0), (3, 4), (7, 7), (2, 6)] {
            let f = capture_direction(i, j, 8);
            let (r, u, f2) = capture_basis(f);
            let (r, u, f2) = (Vec3::from_array(r), Vec3::from_array(u), Vec3::from_array(f2));
            assert!((r.length() - 1.0).abs() < 1e-5 && (u.length() - 1.0).abs() < 1e-5);
            assert!(r.dot(u).abs() < 1e-5 && r.dot(f2).abs() < 1e-5 && u.dot(f2).abs() < 1e-5);
            // Right-handed: right × up = forward (toward the camera).
            assert!((r.cross(u) - f2).length() < 1e-5);
            // Up leans toward +Y.
            assert!(u.y >= -1e-6);
        }
    }

    #[test]
    fn instance_rotation_packs() {
        let q = Quat::from_rotation_arc(Vec3::Y, Vec3::new(0.3, 0.9, 0.1).normalize())
            * Quat::from_rotation_y(1.3);
        let m = regia_core::glam::Mat4::from_scale_rotation_translation(
            Vec3::splat(2.5),
            q,
            Vec3::new(1.0, 2.0, 3.0),
        );
        let inst = ImpostorInstance::from_model(&m.to_cols_array_2d(), [0.0; 3], 1.0, 7, 3);
        assert!((inst.scale - 2.5).abs() < 1e-5);
        assert_eq!(inst.packed & 0xffff, 7);
        assert_eq!((inst.packed >> 16) & 0xff, 3);
        let back = inst.quat();
        assert!(back.dot(q).abs() > 0.99999, "{back:?} vs {q:?}");
        assert_eq!(inst.pivot, [1.0, 2.0, 3.0]);
    }

    fn rock_card(settings: &ImpostorSettings) -> ImpostorCard {
        let asset = procedural_rock(5, RockStyle::angular([0.4, 0.3, 0.25]));
        let lod = &asset.lods[0];
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for v in &lod.vertices {
            lo = lo.min(v.position[1]);
            hi = hi.max(v.position[1]);
        }
        let cy = 0.5 * (lo + hi);
        let r = lod
            .vertices
            .iter()
            .map(|v| (v.position[0].powi(2) + (v.position[1] - cy).powi(2) + v.position[2].powi(2)).sqrt())
            .fold(0.0f32, f32::max);
        bake_cards(
            &[ImpostorSource {
                parts: vec![ImpostorPart {
                    vertices: &lod.vertices,
                    indices: &lod.indices,
                    material: &asset.material,
                    colour: [1.0; 3],
                }],
                centre_y: cy,
                radius: r,
            }],
            settings,
        )
        .remove(0)
    }

    #[test]
    fn bake_covers_the_silhouette_and_is_deterministic() {
        let settings = ImpostorSettings {
            grid: 4,
            cell: 16,
            ..ImpostorSettings::default()
        };
        let a = rock_card(&settings);
        let b = rock_card(&settings);
        assert_eq!(a, b, "the bake is deterministic");
        assert_eq!(a.albedo.len(), settings.mip_count() as usize);
        let side = 64usize;
        assert_eq!(a.albedo[0].len(), side * side * 4);
        // Every capture shows the rock: covered texels in the middle, none at the corners
        // (the rock fills less than its bounding sphere's square).
        for j in 0..4 {
            for i in 0..4 {
                let px = |x: usize, y: usize| a.albedo[0][((j * 16 + y) * side + i * 16 + x) * 4 + 3];
                assert!(
                    px(8, 8) > 128 || px(7, 9) > 128,
                    "capture ({i}, {j}) has its center covered"
                );
                assert_eq!(px(0, 0), 0, "capture ({i}, {j}) corner is empty");
                let covered = (0..16)
                    .flat_map(|y| (0..16).map(move |x| (x, y)))
                    .filter(|&(x, y)| px(x, y) >= 128)
                    .count();
                assert!(
                    covered > 40 && covered < 230,
                    "capture ({i}, {j}) coverage {covered}/256"
                );
            }
        }
        // Normals of the top-down-most capture face the camera on average.
        let mut nz = 0.0;
        let mut n = 0;
        let (_, _, f) = capture_basis(capture_direction(1, 1, 4));
        for y in 0..16 {
            for x in 0..16 {
                let o = ((16 + y) * side + 16 + x) * 4;
                if a.albedo[0][o + 3] >= 128 {
                    let v = [0, 1, 2].map(|k| f32::from(a.normal_depth[0][o + k]) / 255.0 * 2.0 - 1.0);
                    nz += v[0] * f[0] + v[1] * f[1] + v[2] * f[2];
                    n += 1;
                }
            }
        }
        assert!(nz / n as f32 > 0.4, "mean N·view {}", nz / n as f32);
    }

    #[test]
    fn mips_keep_alpha_test_coverage() {
        let settings = ImpostorSettings {
            grid: 2,
            cell: 32,
            ..ImpostorSettings::default()
        };
        let card = rock_card(&settings);
        let frac = |level: &Vec<u8>| {
            let n = level.len() / 4;
            level.as_chunks::<4>().0.iter().filter(|p| p[3] >= 128).count() as f32 / n as f32
        };
        let base = frac(&card.albedo[0]);
        for (m, level) in card.albedo.iter().enumerate().take(4).skip(1) {
            let f = frac(level);
            assert!((f - base).abs() < 0.12, "mip {m} coverage {f} vs {base}");
        }
        // Transparent texels carry a dilated (rock-like) colour, not black.
        let transparent: Vec<&[u8]> = card.albedo[0]
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] == 0)
            .map(|p| &p[..])
            .collect();
        assert!(!transparent.is_empty());
        let mean = transparent.iter().map(|p| f32::from(p[0])).sum::<f32>() / transparent.len() as f32;
        assert!(mean > 20.0, "dilated color mean {mean}");
    }
}
