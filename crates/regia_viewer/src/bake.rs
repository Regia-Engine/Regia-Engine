// SPDX-License-Identifier: MPL-2.0
//! The offline environment baker: an HDR image or the procedural default studio becomes a
//! [`BakedEnv`]. It runs on a desktop, never in the player. (A player that never calls it links none of
//! it.)
//!
//! Specular: the split-sum prefilter (Karis 2013) of the GGX lobe (Walter et al. 2007), importance
//! sampled with a Hammersley set and read from a solid-angle pyramid of the source (filtered importance
//! sampling, Křivánek & Colbert 2008), so a small bright lamp neither speckles nor loses energy.
//! Diffuse: nine spherical harmonics (Ramamoorthi & Hanrahan 2001).

use std::f32::consts::PI;

use glam::Vec3;

use crate::env::{BakedEnv, EnvError, encode_rgb9e5};

/// What a bake reads: linear radiance per direction.
pub trait EnvSource: Sync {
    /// The radiance arriving from the unit direction `dir`.
    fn radiance(&self, dir: Vec3) -> Vec3;
}

/// Bake settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BakeOptions {
    /// Texels per face at mip 0, a power of two.
    pub face_size: u32,
    /// Mip levels. The last one holds roughness 1 (`lod = sqrt(roughness) * (mips - 1)`).
    pub mip_count: u32,
    /// GGX samples per texel.
    pub samples: u32,
}

impl Default for BakeOptions {
    /// The size of the shipped studio: 131 KB in the file before compression.
    fn default() -> Self {
        Self {
            face_size: 64,
            mip_count: 5,
            samples: 256,
        }
    }
}

/// The largest side the source cube is sampled at: 4x the face is quality head-room the box
/// filter does not need beyond it, and the f32 cube costs `6 * side^2 * 12` bytes — an 8192
/// side for a 2048 face is about 4.8 GB, which the bake tool cannot assume it has.
pub(crate) const SOURCE_CUBE_MAX_SIDE: usize = 2048;

/// The side [`bake`] samples its source at for a `face_size` output: 4x the face (never below
/// 256), capped at [`SOURCE_CUBE_MAX_SIDE`].
pub(crate) fn source_cube_side(face_size: u32) -> usize {
    (face_size as usize * 4).clamp(256, SOURCE_CUBE_MAX_SIDE)
}

/// The unit direction of a cube texel, with the face layout of the Vulkan and WebGPU cube lookup.
fn face_dir(face: usize, u: f32, v: f32) -> Vec3 {
    match face {
        0 => Vec3::new(1.0, -v, -u),
        1 => Vec3::new(-1.0, -v, u),
        2 => Vec3::new(u, 1.0, v),
        3 => Vec3::new(u, -1.0, -v),
        4 => Vec3::new(u, -v, 1.0),
        _ => Vec3::new(-u, -v, -1.0),
    }
    .normalize()
}

/// The face and the u, v in -1..=1 a direction lands on (the inverse of [`face_dir`]).
fn dir_face(d: Vec3) -> (usize, f32, f32) {
    let a = d.abs();
    if a.x >= a.y && a.x >= a.z {
        if d.x > 0.0 {
            (0, -d.z / a.x, -d.y / a.x)
        } else {
            (1, d.z / a.x, -d.y / a.x)
        }
    } else if a.y >= a.z {
        if d.y > 0.0 {
            (2, d.x / a.y, d.z / a.y)
        } else {
            (3, d.x / a.y, -d.z / a.y)
        }
    } else if d.z > 0.0 {
        (4, d.x / a.z, -d.y / a.z)
    } else {
        (5, -d.x / a.z, -d.y / a.z)
    }
}

/// How much solid angle a cube texel at face coordinates u, v covers, up to the texel's size.
fn texel_solid_angle_weight(u: f32, v: f32) -> f32 {
    (1.0 + u * u + v * v).powf(-1.5)
}

/// A float cube pyramid of the source, read at a fractional level.
struct SourceCube {
    /// Per level: size and six faces of texels.
    levels: Vec<(usize, Vec<Vec3>)>,
}

impl SourceCube {
    /// Samples `source` at `face_size` per face, with 4 sub-samples per texel, and box-filters the
    /// levels down to one texel per face.
    fn build(source: &dyn EnvSource, face_size: usize) -> Self {
        let mut base = vec![Vec3::ZERO; 6 * face_size * face_size];
        let threads = std::thread::available_parallelism().map_or(1, usize::from).max(1);
        let rows_per_chunk = (face_size * 6).div_ceil(threads).max(1) * face_size;
        std::thread::scope(|scope| {
            for (chunk_index, chunk) in base.chunks_mut(rows_per_chunk).enumerate() {
                scope.spawn(move || {
                    for (i, texel) in chunk.iter_mut().enumerate() {
                        let index = chunk_index * rows_per_chunk + i;
                        let face = index / (face_size * face_size);
                        let in_face = index % (face_size * face_size);
                        let (x, y) = (in_face % face_size, in_face / face_size);
                        let mut sum = Vec3::ZERO;
                        for (ox, oy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                            let u = 2.0 * (x as f32 + ox) / face_size as f32 - 1.0;
                            let v = 2.0 * (y as f32 + oy) / face_size as f32 - 1.0;
                            sum += source.radiance(face_dir(face, u, v));
                        }
                        *texel = sum * 0.25;
                    }
                });
            }
        });
        let mut levels = vec![(face_size, base)];
        while let Some((size, texels)) = levels.last().filter(|(size, _)| *size > 1) {
            let half = size / 2;
            let mut next = Vec::with_capacity(6 * half * half);
            for face in 0..6 {
                let start = face * size * size;
                for y in 0..half {
                    for x in 0..half {
                        // Weighted by each child's solid angle, so a coarse texel holds the mean radiance
                        // over its directions and a lamp keeps its energy at every level.
                        let mut sum = Vec3::ZERO;
                        let mut weight = 0.0;
                        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                            let (cx, cy) = (x * 2 + dx, y * 2 + dy);
                            let u = 2.0 * (cx as f32 + 0.5) / *size as f32 - 1.0;
                            let v = 2.0 * (cy as f32 + 0.5) / *size as f32 - 1.0;
                            let w = texel_solid_angle_weight(u, v);
                            sum += texels[start + cy * size + cx] * w;
                            weight += w;
                        }
                        next.push(sum / weight);
                    }
                }
            }
            levels.push((half, next));
        }
        Self { levels }
    }

    fn base_size(&self) -> usize {
        self.levels[0].0
    }

    fn sample_level(&self, level: usize, face: usize, u: f32, v: f32) -> Vec3 {
        let (size, texels) = &self.levels[level.min(self.levels.len() - 1)];
        let size = *size;
        let fx = ((u + 1.0) * 0.5 * size as f32 - 0.5).clamp(0.0, size as f32 - 1.0);
        let fy = ((v + 1.0) * 0.5 * size as f32 - 0.5).clamp(0.0, size as f32 - 1.0);
        let (x0, y0) = (fx as usize, fy as usize);
        let (x1, y1) = ((x0 + 1).min(size - 1), (y0 + 1).min(size - 1));
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let start = face * size * size;
        let at = |x: usize, y: usize| texels[start + y * size + x];
        at(x0, y0)
            .lerp(at(x1, y0), tx)
            .lerp(at(x0, y1).lerp(at(x1, y1), tx), ty)
    }

    fn sample(&self, dir: Vec3, lod: f32) -> Vec3 {
        let (face, u, v) = dir_face(dir);
        let lod = lod.clamp(0.0, (self.levels.len() - 1) as f32);
        let low = lod as usize;
        let t = lod - low as f32;
        let a = self.sample_level(low, face, u, v);
        if t <= 0.0 {
            return a;
        }
        a.lerp(self.sample_level(low + 1, face, u, v), t)
    }
}

fn hammersley(i: u32, n: u32) -> (f32, f32) {
    (i as f32 / n as f32, i.reverse_bits() as f32 * 2.328_306_4e-10)
}

/// A half vector of the GGX lobe of `alpha` around +Z.
fn ggx_half(xi: (f32, f32), alpha: f32) -> Vec3 {
    let phi = 2.0 * PI * xi.0;
    let cos_theta = ((1.0 - xi.1) / (1.0 + (alpha * alpha - 1.0) * xi.1)).sqrt();
    let sin_theta = (1.0 - cos_theta * cos_theta).max(0.0).sqrt();
    Vec3::new(sin_theta * phi.cos(), sin_theta * phi.sin(), cos_theta)
}

/// A rotation frame with `n` as its +Z.
fn frame(n: Vec3) -> (Vec3, Vec3) {
    let up = if n.z.abs() < 0.999 { Vec3::Z } else { Vec3::X };
    let t = up.cross(n).normalize();
    (t, n.cross(t))
}

/// The prefiltered radiance along `dir`. `min_lod` is the source level whose texels match the output
/// texel: a narrow lobe read any finer would fall between output texels and lose a small lamp.
fn prefilter(cube: &SourceCube, dir: Vec3, roughness: f32, samples: u32, min_lod: f32) -> Vec3 {
    let alpha = (roughness * roughness).max(1e-4);
    let (t, b) = frame(dir);
    let omega_texel = 4.0 * PI / (6.0 * (cube.base_size() * cube.base_size()) as f32);
    let mut sum = Vec3::ZERO;
    let mut weight = 0.0;
    for i in 0..samples {
        let h_local = ggx_half(hammersley(i, samples), alpha);
        let h = t * h_local.x + b * h_local.y + dir * h_local.z;
        let v_dot_h = dir.dot(h);
        let l = h * 2.0 * v_dot_h - dir;
        let n_dot_l = dir.dot(l);
        if n_dot_l <= 0.0 {
            continue;
        }
        // N = V: the lobe's pdf over the light direction is D / 4.
        let n_dot_h = h_local.z;
        let a2 = alpha * alpha;
        let denom = n_dot_h * n_dot_h * (a2 - 1.0) + 1.0;
        let d = a2 / (PI * denom * denom);
        let pdf = (d * 0.25).max(1e-4);
        let omega_sample = 1.0 / (samples as f32 * pdf);
        let lod = (0.5 * (omega_sample / omega_texel).max(1.0).log2()).max(min_lod);
        sum += cube.sample(l, lod) * n_dot_l;
        weight += n_dot_l;
    }
    if weight > 0.0 {
        sum / weight
    } else {
        cube.sample(dir, min_lod)
    }
}

/// The real spherical harmonics basis in the order of `env_diffuse` in the prelude.
fn sh_basis(d: Vec3) -> [f32; 9] {
    [
        0.282_095,
        0.488_603 * d.y,
        0.488_603 * d.z,
        0.488_603 * d.x,
        1.092_548 * d.x * d.y,
        1.092_548 * d.y * d.z,
        0.315_392 * (3.0 * d.z * d.z - 1.0),
        1.092_548 * d.x * d.z,
        0.546_274 * (d.x * d.x - d.y * d.y),
    ]
}

/// Projects the source on nine harmonics and applies the cosine lobe divided by pi (1, 2/3, 1/4 per
/// band), so the shader's sum is the light a white diffuse surface reflects.
fn project_diffuse(cube: &SourceCube) -> [[f32; 3]; 9] {
    // A smooth level: nine harmonics see nothing finer.
    let level = cube.levels.iter().position(|(size, _)| *size <= 64).unwrap_or(0);
    let (size, texels) = &cube.levels[level];
    let size = *size;
    let mut sum = [Vec3::ZERO; 9];
    let mut solid = 0.0f32;
    for face in 0..6 {
        for y in 0..size {
            for x in 0..size {
                let u = 2.0 * (x as f32 + 0.5) / size as f32 - 1.0;
                let v = 2.0 * (y as f32 + 0.5) / size as f32 - 1.0;
                let d = face_dir(face, u, v);
                // The solid angle of a cube texel.
                let w = 4.0 / (size * size) as f32 * texel_solid_angle_weight(u, v);
                let radiance = texels[face * size * size + y * size + x];
                for (coefficient, basis) in sum.iter_mut().zip(sh_basis(d)) {
                    *coefficient += radiance * basis * w;
                }
                solid += w;
            }
        }
    }
    let normalize = 4.0 * PI / solid;
    const BAND: [f32; 9] = [1.0, 2.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0, 0.25, 0.25, 0.25, 0.25, 0.25];
    let mut out = [[0.0; 3]; 9];
    for i in 0..9 {
        out[i] = (sum[i] * normalize * BAND[i]).to_array();
    }
    out
}

/// Bakes `source`.
///
/// # Errors
/// [`EnvError::BadSize`] when the options describe an impossible size.
pub fn bake(source: &dyn EnvSource, options: BakeOptions) -> Result<BakedEnv, EnvError> {
    if !options.face_size.is_power_of_two() || options.face_size < 4 || options.mip_count == 0 {
        return Err(EnvError::BadSize(format!(
            "{} texels per face, {} mips",
            options.face_size, options.mip_count
        )));
    }
    // `from_parts` checks this too, but only after the source cube is allocated. A face past
    // `MAX_FACE` (or a mip count the face cannot hold) used to overflow that allocation in debug.
    crate::env::check_size(options.face_size, options.mip_count)?;
    let source_size = source_cube_side(options.face_size);
    let cube = SourceCube::build(source, source_size);
    let sh = project_diffuse(&cube);
    let mut texels = Vec::new();
    let last = (options.mip_count - 1).max(1) as f32;
    for mip in 0..options.mip_count {
        let size = (options.face_size >> mip).max(1) as usize;
        let lod_norm = mip as f32 / last;
        // lod = sqrt(roughness) * last.
        let roughness = lod_norm * lod_norm;
        let mut level = vec![0u32; 6 * size * size];
        let threads = std::thread::available_parallelism().map_or(1, usize::from).max(1);
        let chunk_len = level.len().div_ceil(threads).max(1);
        let cube = &cube;
        std::thread::scope(|scope| {
            for (chunk_index, chunk) in level.chunks_mut(chunk_len).enumerate() {
                scope.spawn(move || {
                    for (i, texel) in chunk.iter_mut().enumerate() {
                        let index = chunk_index * chunk_len + i;
                        let face = index / (size * size);
                        let in_face = index % (size * size);
                        let (x, y) = (in_face % size, in_face / size);
                        let u = 2.0 * (x as f32 + 0.5) / size as f32 - 1.0;
                        let v = 2.0 * (y as f32 + 0.5) / size as f32 - 1.0;
                        let dir = face_dir(face, u, v);
                        // The source filtered to this texel's footprint is the mirror level, and the
                        // finest any lobe of this level may read.
                        let footprint = (source_size as f32 / size as f32).log2();
                        let radiance = if mip == 0 {
                            cube.sample(dir, footprint)
                        } else {
                            prefilter(cube, dir, roughness, options.samples, footprint)
                        };
                        *texel = encode_rgb9e5(radiance.to_array());
                    }
                });
            }
        });
        texels.extend_from_slice(&level);
    }
    BakedEnv::from_parts(options.face_size, options.mip_count, sh, texels)
}

/// The split-sum table: `size` x `size` RG8, x = n dot v, y = perceptual roughness, red the scale and
/// green the bias of F0. GGX with height-correlated Smith visibility (Heitz 2014).
#[must_use]
pub fn bake_dfg(size: u32, samples: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((size * size * 2) as usize);
    for y in 0..size {
        let roughness = (y as f32 + 0.5) / size as f32;
        for x in 0..size {
            let n_dot_v = ((x as f32 + 0.5) / size as f32).max(1e-3);
            let (a, b) = integrate_dfg(n_dot_v, roughness, samples);
            out.push((a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            out.push((b.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
        }
    }
    out
}

fn integrate_dfg(n_dot_v: f32, roughness: f32, samples: u32) -> (f32, f32) {
    let alpha = roughness * roughness;
    let a2 = alpha * alpha;
    let v = Vec3::new((1.0 - n_dot_v * n_dot_v).max(0.0).sqrt(), 0.0, n_dot_v);
    let (mut sum_a, mut sum_b) = (0.0, 0.0);
    for i in 0..samples {
        let h = ggx_half(hammersley(i, samples), alpha.max(1e-4));
        let v_dot_h = v.dot(h);
        let l = h * 2.0 * v_dot_h - v;
        let n_dot_l = l.z;
        if n_dot_l <= 0.0 || v_dot_h <= 0.0 {
            continue;
        }
        let g_v = n_dot_l * (n_dot_v * n_dot_v * (1.0 - a2) + a2).sqrt();
        let g_l = n_dot_v * (n_dot_l * n_dot_l * (1.0 - a2) + a2).sqrt();
        let visibility = 0.5 / (g_v + g_l).max(1e-6);
        let g = 4.0 * visibility * n_dot_l * v_dot_h / h.z.max(1e-6);
        let fc = (1.0 - v_dot_h).powi(5);
        sum_a += (1.0 - fc) * g;
        sum_b += fc * g;
    }
    (sum_a / samples as f32, sum_b / samples as f32)
}

/// An equirectangular HDR image, row 0 at the top (+Y).
#[derive(Debug, Clone, PartialEq)]
pub struct EquirectImage {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Linear radiance, `width * height` texels.
    pub texels: Vec<[f32; 3]>,
}

impl EnvSource for EquirectImage {
    fn radiance(&self, dir: Vec3) -> Vec3 {
        let (w, h) = (self.width as usize, self.height as usize);
        // A zero side panics in `%` or in `clamp` (the upper bound goes negative). A short
        // buffer or a side product that does not fit in `usize` would panic on the index.
        let Some(need) = w.checked_mul(h).filter(|need| *need > 0) else {
            return Vec3::ZERO;
        };
        if self.texels.len() < need {
            return Vec3::ZERO;
        }
        let u = dir.x.atan2(-dir.z) / (2.0 * PI) + 0.5;
        let v = dir.y.clamp(-1.0, 1.0).acos() / PI;
        let fx = (u * w as f32 - 0.5).rem_euclid(w as f32);
        let fy = (v * h as f32 - 0.5).clamp(0.0, h as f32 - 1.0);
        let (x0, y0) = (fx as usize % w, fy as usize);
        let (x1, y1) = ((x0 + 1) % w, (y0 + 1).min(h - 1));
        let (tx, ty) = (fx.fract(), fy.fract());
        let at = |x: usize, y: usize| Vec3::from(self.texels[y * w + x]);
        at(x0, y0)
            .lerp(at(x1, y0), tx)
            .lerp(at(x0, y1).lerp(at(x1, y1), tx), ty)
    }
}

/// Why a Radiance HDR file was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HdrError {
    /// The header does not describe an RGBE image in the usual orientation.
    #[error("bad Radiance header: {0}")]
    Header(&'static str),
    /// The pixel data ended early or a run does not fit its scanline.
    #[error("bad Radiance pixel data at scanline {0}")]
    Pixels(u32),
}

/// Reads a Radiance `.hdr` (RGBE, flat or new-style run-length encoded, `-Y h +X w`), from the format's
/// public description (Ward, Graphics Gems II, 1991).
///
/// # Errors
/// [`HdrError`] with the reason.
pub fn parse_radiance_hdr(bytes: &[u8]) -> Result<EquirectImage, HdrError> {
    let mut at = 0;
    let line = |at: &mut usize| -> Option<&[u8]> {
        let rest = bytes.get(*at..)?;
        let end = rest.iter().position(|b| *b == b'\n')?;
        *at += end + 1;
        let mut row = &rest[..end];
        // Writers on Windows end header lines with CR LF. The resolution line splits on
        // whitespace, but `FORMAT=32-bit_rle_rgbe\r` would miss the format check.
        if row.last() == Some(&b'\r') {
            row = &row[..row.len() - 1];
        }
        Some(row)
    };
    let first = line(&mut at).ok_or(HdrError::Header("no signature"))?;
    if !first.starts_with(b"#?") {
        return Err(HdrError::Header("no signature"));
    }
    let mut format_ok = false;
    loop {
        let row = line(&mut at).ok_or(HdrError::Header("no resolution line"))?;
        if row.is_empty() {
            break;
        }
        if row == b"FORMAT=32-bit_rle_rgbe" {
            format_ok = true;
        }
    }
    if !format_ok {
        return Err(HdrError::Header("not 32-bit_rle_rgbe"));
    }
    let resolution = line(&mut at).ok_or(HdrError::Header("no resolution line"))?;
    let text = std::str::from_utf8(resolution).map_err(|_| HdrError::Header("resolution line"))?;
    let mut words = text.split_ascii_whitespace();
    let (Some("-Y"), Some(h), Some("+X"), Some(w), None) = (
        words.next(),
        words.next(),
        words.next(),
        words.next(),
        words.next(),
    ) else {
        return Err(HdrError::Header("only -Y h +X w images are read"));
    };
    let width: u32 = w.parse().map_err(|_| HdrError::Header("width"))?;
    let height: u32 = h.parse().map_err(|_| HdrError::Header("height"))?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 8192 * 4096 {
        return Err(HdrError::Header("size"));
    }
    let mut texels = Vec::with_capacity((width * height) as usize);
    let mut scan = vec![[0u8; 4]; width as usize];
    for y in 0..height {
        read_scanline(bytes, &mut at, &mut scan).ok_or(HdrError::Pixels(y))?;
        for rgbe in &scan {
            texels.push(decode_rgbe(*rgbe));
        }
    }
    Ok(EquirectImage {
        width,
        height,
        texels,
    })
}

fn read_scanline(bytes: &[u8], at: &mut usize, scan: &mut [[u8; 4]]) -> Option<()> {
    let width = scan.len();
    let head = bytes.get(*at..*at + 4)?;
    let run_length = (8..=0x7fff).contains(&width)
        && head[0] == 2
        && head[1] == 2
        && (usize::from(head[2]) << 8 | usize::from(head[3])) == width;
    if !run_length {
        for texel in scan.iter_mut() {
            let b = bytes.get(*at..*at + 4)?;
            texel.copy_from_slice(b);
            *at += 4;
        }
        return Some(());
    }
    *at += 4;
    for channel in 0..4 {
        let mut x = 0;
        while x < width {
            let code = *bytes.get(*at)?;
            *at += 1;
            if code > 128 {
                let count = usize::from(code - 128);
                let value = *bytes.get(*at)?;
                *at += 1;
                for texel in scan.get_mut(x..x + count)? {
                    texel[channel] = value;
                }
                x += count;
            } else {
                let count = usize::from(code);
                if count == 0 {
                    return None;
                }
                let values = bytes.get(*at..*at + count)?;
                for (texel, value) in scan.get_mut(x..x + count)?.iter_mut().zip(values) {
                    texel[channel] = *value;
                }
                *at += count;
                x += count;
            }
        }
    }
    Some(())
}

fn decode_rgbe(rgbe: [u8; 4]) -> [f32; 3] {
    if rgbe[3] == 0 {
        return [0.0; 3];
    }
    let scale = 2f32.powi(i32::from(rgbe[3]) - 128 - 8);
    [
        (f32::from(rgbe[0]) + 0.5) * scale,
        (f32::from(rgbe[1]) + 0.5) * scale,
        (f32::from(rgbe[2]) + 0.5) * scale,
    ]
}

/// One rectangular light of the procedural studio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SoftBox {
    /// Direction to the light's center.
    pub dir: [f32; 3],
    /// Half width and half height, as tangents of the angle from the center.
    pub half_size: [f32; 2],
    /// Soft edge width, in the same tangent units.
    pub edge: f32,
    /// Linear radiance.
    pub radiance: [f32; 3],
}

/// The procedural default studio, generic and original: a dim tent with large soft boxes on the viewer's
/// side, so polished metal shows clean bright and dark bands facing the camera.
#[derive(Debug, Clone, PartialEq)]
pub struct StudioLights {
    /// The tent's color at the zenith.
    pub top: [f32; 3],
    /// The tent's color at the horizon.
    pub horizon: [f32; 3],
    /// The tent's color at the nadir.
    pub bottom: [f32; 3],
    /// The lights.
    pub lights: Vec<SoftBox>,
}

impl Default for StudioLights {
    fn default() -> Self {
        Self {
            top: [0.62, 0.63, 0.65],
            horizon: [0.32, 0.32, 0.33],
            bottom: [0.09, 0.09, 0.09],
            lights: vec![
                // The key, up and to the viewer's left.
                SoftBox {
                    dir: [-0.55, 0.55, 0.65],
                    half_size: [0.50, 0.42],
                    edge: 0.10,
                    radiance: [6.5, 6.4, 6.2],
                },
                // A tall fill on the right.
                SoftBox {
                    dir: [0.82, 0.10, 0.55],
                    half_size: [0.30, 0.80],
                    edge: 0.08,
                    radiance: [2.6, 2.6, 2.7],
                },
                // A strip overhead, for the top edges.
                SoftBox {
                    dir: [0.0, 0.93, 0.36],
                    half_size: [1.10, 0.14],
                    edge: 0.05,
                    radiance: [8.0, 8.0, 8.0],
                },
                // A low strip, for the lower edges.
                SoftBox {
                    dir: [0.1, -0.62, 0.78],
                    half_size: [0.90, 0.08],
                    edge: 0.04,
                    radiance: [1.6, 1.6, 1.6],
                },
                // A small hard lamp near the camera: the sharp point stones catch.
                SoftBox {
                    dir: [0.18, 0.30, 0.94],
                    half_size: [0.035, 0.035],
                    edge: 0.01,
                    radiance: [40.0, 40.0, 40.0],
                },
            ],
        }
    }
}

/// A non-finite channel becomes 0. A finite negative stays, so a tent can still go dark.
fn finite_color(rgb: [f32; 3]) -> Vec3 {
    Vec3::new(
        if rgb[0].is_finite() { rgb[0] } else { 0.0 },
        if rgb[1].is_finite() { rgb[1] } else { 0.0 },
        if rgb[2].is_finite() { rgb[2] } else { 0.0 },
    )
}

impl EnvSource for StudioLights {
    fn radiance(&self, dir: Vec3) -> Vec3 {
        let y = dir.y;
        let horizon = finite_color(self.horizon);
        let base = if y >= 0.0 {
            horizon.lerp(finite_color(self.top), y.sqrt())
        } else {
            horizon.lerp(finite_color(self.bottom), (-y).sqrt())
        };
        let mut out = base;
        for light in &self.lights {
            let center_v = Vec3::from(light.dir);
            // A zero or non-finite direction normalizes to NaN, and `NaN <= 0` is false, so the
            // sample mixes NaN into every texel and the bake fails the finite-coefficient check.
            if !center_v.is_finite() || center_v.length_squared() < 1e-12 {
                continue;
            }
            let center = center_v.normalize();
            let cos = dir.dot(center);
            if cos <= 0.0 {
                continue;
            }
            let (t, b) = frame(center);
            let p = dir / cos;
            let (x, y) = (p.dot(t).abs(), p.dot(b).abs());
            let edge = if light.edge.is_finite() {
                light.edge.max(1e-4)
            } else {
                1e-4
            };
            let half_x = if light.half_size[0].is_finite() {
                light.half_size[0]
            } else {
                0.0
            };
            let half_y = if light.half_size[1].is_finite() {
                light.half_size[1]
            } else {
                0.0
            };
            let inside = ((half_x - x) / edge).clamp(0.0, 1.0) * ((half_y - y) / edge).clamp(0.0, 1.0);
            out = out.lerp(finite_color(light.radiance), inside);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Constant(Vec3);
    impl EnvSource for Constant {
        fn radiance(&self, _: Vec3) -> Vec3 {
            self.0
        }
    }

    #[test]
    fn face_dir_and_dir_face_agree() {
        for face in 0..6 {
            for (u, v) in [(0.3, -0.4), (-0.7, 0.2), (0.0, 0.0)] {
                let (f, a, b) = dir_face(face_dir(face, u, v));
                assert_eq!(f, face);
                assert!(
                    (a - u).abs() < 1e-5 && (b - v).abs() < 1e-5,
                    "{face} {u} {v}: {a} {b}"
                );
            }
        }
    }

    #[test]
    fn the_source_cube_side_is_capped_for_large_faces() {
        // corr-web-viewer listed: `--face 2048` sampled an 8192-per-face source cube
        // (~4.8 GB of f32) and was killed for lack of memory.
        assert_eq!(source_cube_side(64), 256, "the small-source floor");
        assert_eq!(source_cube_side(512), 2048, "4x at the cap");
        assert_eq!(
            source_cube_side(2048),
            SOURCE_CUBE_MAX_SIDE,
            "a 2048 face no longer asks for an 8192 side"
        );
    }

    #[test]
    fn a_constant_world_bakes_to_itself_at_every_roughness_and_in_the_harmonics() {
        let env = bake(
            &Constant(Vec3::new(0.5, 1.0, 2.0)),
            BakeOptions {
                face_size: 8,
                mip_count: 3,
                samples: 32,
            },
        )
        .expect("bake");
        for mip in 0..3 {
            for texel in env.mip_texels(mip) {
                let c = crate::env::decode_rgb9e5(*texel);
                assert!((c[0] - 0.5).abs() < 0.01 && (c[1] - 1.0).abs() < 0.01 && (c[2] - 2.0).abs() < 0.02);
            }
        }
        // White diffuse under a constant world reflects that world.
        let dc = env.sh()[0][1] * 0.282_095;
        assert!((dc - 1.0).abs() < 0.01, "{dc}");
        for coefficient in &env.sh()[1..] {
            assert!(coefficient[1].abs() < 1e-3, "{coefficient:?}");
        }
    }

    #[test]
    fn the_prefilter_keeps_energy_on_a_small_bright_lamp() {
        let studio = StudioLights {
            top: [0.0; 3],
            horizon: [0.0; 3],
            bottom: [0.0; 3],
            lights: vec![SoftBox {
                dir: [0.0, 0.0, 1.0],
                half_size: [0.1, 0.1],
                edge: 0.01,
                radiance: [100.0; 3],
            }],
        };
        let env = bake(
            &studio,
            BakeOptions {
                face_size: 16,
                mip_count: 5,
                samples: 256,
            },
        )
        .expect("bake");
        // The mean over the sphere is about the same at each roughness (cube texels weighted by solid
        // angle), so a rough lobe spreads the lamp without losing it.
        let mean = |mip: u32| {
            let size = 16u32 >> mip;
            let mut sum = 0.0;
            let mut weights = 0.0;
            for (i, texel) in env.mip_texels(mip).iter().enumerate() {
                let in_face = i % (size * size) as usize;
                let (x, y) = (in_face % size as usize, in_face / size as usize);
                let u = 2.0 * (x as f32 + 0.5) / size as f32 - 1.0;
                let v = 2.0 * (y as f32 + 0.5) / size as f32 - 1.0;
                let w = (1.0 + u * u + v * v).powf(-1.5);
                sum += crate::env::decode_rgb9e5(*texel)[0] * w;
                weights += w;
            }
            sum / weights
        };
        let base = mean(0);
        assert!(base > 0.0);
        for mip in 1..5 {
            let ratio = mean(mip) / base;
            assert!((0.8..=1.25).contains(&ratio), "mip {mip}: ratio {ratio}");
        }
    }

    #[test]
    fn the_dfg_table_matches_the_known_shape() {
        let table = bake_dfg(32, 256);
        assert_eq!(table.len(), 32 * 32 * 2);
        let at = |x: usize, y: usize| (table[(y * 32 + x) * 2], table[(y * 32 + x) * 2 + 1]);
        // A smooth lobe seen head on: full scale, almost no bias.
        let (a, b) = at(31, 0);
        assert!(a > 245 && b < 8, "{a} {b}");
        // A rough lobe loses energy to single scattering.
        let (a, b) = at(31, 31);
        assert!((u32::from(a) + u32::from(b)) < 215, "{a} {b}");
    }

    #[test]
    fn the_shipped_table_is_what_the_bake_produces() {
        let shipped = crate::DFG_TABLE;
        let baked = bake_dfg(crate::DFG_SIZE, 1024);
        assert_eq!(shipped.len(), baked.len());
        for (s, b) in shipped.iter().zip(&baked) {
            assert!(s.abs_diff(*b) <= 1, "{s} vs {b}");
        }
    }

    #[test]
    fn the_shipped_studio_is_what_the_bake_produces() {
        let baked = bake(&StudioLights::default(), BakeOptions::default()).expect("bake");
        let shipped = BakedEnv::studio_default();
        assert_eq!(baked.face_size(), shipped.face_size());
        assert_eq!(baked.mip_count(), shipped.mip_count());
        for (a, b) in baked.sh().iter().flatten().zip(shipped.sh().iter().flatten()) {
            assert!((a - b).abs() < 0.01, "{a} vs {b}");
        }
        for (a, b) in baked.texels.iter().zip(&shipped.texels) {
            let (a, b) = (crate::env::decode_rgb9e5(*a), crate::env::decode_rgb9e5(*b));
            for i in 0..3 {
                assert!(
                    (a[i] - b[i]).abs() <= 0.02 * a[i].max(b[i]).max(0.5),
                    "{a:?} vs {b:?}"
                );
            }
        }
    }

    fn rle_hdr() -> Vec<u8> {
        let mut bytes = b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 2 +X 8\n".to_vec();
        for row in 0..2u8 {
            bytes.extend_from_slice(&[2, 2, 0, 8]);
            // Red: a run of 8; green: 8 literals; blue: a run; exponent: a run.
            bytes.extend_from_slice(&[128 + 8, 128 + row]);
            bytes.push(8);
            bytes.extend_from_slice(&[10, 20, 30, 40, 50, 60, 70, 80]);
            bytes.extend_from_slice(&[128 + 8, 0]);
            bytes.extend_from_slice(&[128 + 8, 129]);
        }
        bytes
    }

    #[test]
    fn an_impossible_bake_size_is_refused() {
        let result = bake(
            &Constant(Vec3::ONE),
            BakeOptions {
                face_size: 1 << 31,
                mip_count: 1,
                samples: 1,
            },
        );
        assert!(matches!(result, Err(EnvError::BadSize(_))), "{result:?}");
        // Past the file's largest face, and more mips than a 4-texel face can hold. Both return
        // before any cube is allocated.
        let huge_face = bake(
            &Constant(Vec3::ONE),
            BakeOptions {
                face_size: 4096,
                mip_count: 1,
                samples: 1,
            },
        );
        assert!(matches!(huge_face, Err(EnvError::BadSize(_))), "{huge_face:?}");
        let huge_mips = bake(
            &Constant(Vec3::ONE),
            BakeOptions {
                face_size: 4,
                mip_count: 100,
                samples: 1,
            },
        );
        assert!(matches!(huge_mips, Err(EnvError::BadSize(_))), "{huge_mips:?}");
    }

    #[test]
    fn a_degenerate_studio_light_does_not_poison_the_bake() {
        let studio = StudioLights {
            top: [f32::NAN, 0.2, 0.2],
            horizon: [0.2, 0.2, 0.2],
            bottom: [0.2, 0.2, 0.2],
            lights: vec![SoftBox {
                dir: [0.0, 0.0, 0.0],
                half_size: [0.2, 0.2],
                edge: 0.05,
                radiance: [5.0, 5.0, 5.0],
            }],
        };
        let env = bake(
            &studio,
            BakeOptions {
                face_size: 8,
                mip_count: 2,
                samples: 4,
            },
        )
        .expect("a degenerate light is skipped");
        assert!(env.sh().iter().flatten().all(|c| c.is_finite()), "{:?}", env.sh());
        assert!(
            env.mip_texels(0)
                .iter()
                .all(|t| { crate::env::decode_rgb9e5(*t).iter().all(|c| c.is_finite()) })
        );
    }

    #[test]
    fn a_bad_equirect_image_does_not_panic() {
        let empty = EquirectImage {
            width: 0,
            height: 4,
            texels: vec![],
        };
        assert_eq!(empty.radiance(Vec3::Y), Vec3::ZERO);
        let flat = EquirectImage {
            width: 4,
            height: 0,
            texels: vec![],
        };
        assert_eq!(flat.radiance(Vec3::Y), Vec3::ZERO);
        let short = EquirectImage {
            width: 4,
            height: 4,
            texels: vec![],
        };
        assert_eq!(short.radiance(Vec3::Z), Vec3::ZERO);
        let huge = EquirectImage {
            width: u32::MAX,
            height: u32::MAX,
            texels: vec![[1.0, 0.0, 0.0]],
        };
        assert_eq!(huge.radiance(Vec3::X), Vec3::ZERO);
    }

    #[test]
    fn the_hdr_reader_accepts_crlf_headers() {
        let mut bytes = b"#?RADIANCE\r\nFORMAT=32-bit_rle_rgbe\r\n\r\n-Y 1 +X 1\r\n".to_vec();
        bytes.extend_from_slice(&[128, 0, 0, 129]);
        let image = parse_radiance_hdr(&bytes).expect("crlf radiance");
        assert_eq!((image.width, image.height), (1, 1));
        assert!((image.texels[0][0] - 128.5 / 128.0).abs() < 1e-5);
    }

    #[test]
    fn the_hdr_reader_reads_run_length_and_flat_files() {
        let image = parse_radiance_hdr(&rle_hdr()).expect("rle");
        assert_eq!((image.width, image.height), (8, 2));
        // Exponent 129: scale 2^(129 - 136) = 1/128.
        assert!((image.texels[0][0] - 128.5 / 128.0).abs() < 1e-5);
        assert!((image.texels[3][1] - 40.5 / 128.0).abs() < 1e-5);
        assert!((image.texels[8][0] - 129.5 / 128.0).abs() < 1e-5);

        let mut flat = b"#?RGBE\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 2\n".to_vec();
        flat.extend_from_slice(&[128, 0, 0, 129, 0, 0, 0, 0]);
        let image = parse_radiance_hdr(&flat).expect("flat");
        assert_eq!(image.texels[1], [0.0; 3]);
    }

    #[test]
    fn bad_hdr_files_are_refused() {
        assert!(parse_radiance_hdr(b"junk").is_err());
        assert!(parse_radiance_hdr(b"#?RADIANCE\n\n-Y 1 +X 1\n").is_err());
        assert!(parse_radiance_hdr(b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n+X 1 -Y 1\n").is_err());
        assert!(parse_radiance_hdr(b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 99999 +X 99999\n").is_err());
        let rle = rle_hdr();
        for cut in [rle.len() - 1, rle.len() - 20, 50] {
            assert!(
                matches!(parse_radiance_hdr(&rle[..cut]), Err(HdrError::Pixels(_))),
                "cut {cut}"
            );
        }
    }

    #[test]
    fn an_equirect_image_maps_directions_to_the_right_rows_and_columns() {
        // Row 0 is up. The horizontal center of the image looks along -Z.
        let (w, h) = (8u32, 4u32);
        let mut texels = vec![[0.0; 3]; (w * h) as usize];
        for (i, t) in texels.iter_mut().enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            *t = [x as f32, y as f32, 0.0];
        }
        let image = EquirectImage {
            width: w,
            height: h,
            texels,
        };
        let up = image.radiance(Vec3::Y);
        let down = image.radiance(-Vec3::Y);
        assert!(up.y < down.y, "{up} {down}");
        let center = image.radiance(Vec3::NEG_Z);
        assert!((center.x - 3.5).abs() < 0.6, "{center}");
    }
}
