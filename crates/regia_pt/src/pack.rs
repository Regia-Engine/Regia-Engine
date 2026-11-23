// SPDX-License-Identifier: MPL-2.0
//! Packs a [`PtScene`] into the flat arrays the kernels read (`pt.wgsl` bindings 2–10).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use regia_core::glam::{Mat4, Vec3, Vec4};

use crate::PtError;
use crate::bvh::{Aabb, Bvh, BvhNode};
use crate::scene::{PtHeightfield, PtLight, PtMesh, PtScene, PtSky};

/// "No index" in GPU data.
pub(crate) const NONE: u32 = u32::MAX;
/// Largest heightfield cell count per side the stack encoding holds (14 bits).
const MAX_HEIGHTFIELD_CELLS: u32 = 1 << 14;
/// Sky CDF resolution for a sky-view table.
const SKY_CDF: (u32, u32) = (256, 128);
/// Sky CDF resolution for a uniform sky.
const UNIFORM_CDF: (u32, u32) = (32, 16);
/// Smallest sky CDF resolution for an equirect map.
const EQUIRECT_CDF_MIN: (u32, u32) = (64, 32);
/// Largest sky CDF resolution for an equirect map.
const EQUIRECT_CDF_MAX: (u32, u32) = (1024, 512);

pub(crate) const FLAG_SUN: u32 = 1;
pub(crate) const FLAG_JITTER: u32 = 2;
pub(crate) const FLAG_TERRAIN: u32 = 4;
/// Probe mode (`probe.rs`): path slots are (probe, ray) pairs.
pub(crate) const FLAG_PROBE: u32 = 8;
/// Probe mode, geometry only (`FLAG_PROBE_GEOMETRY` in the shader): rays record their first
/// hit and end, no shading (relocation rounds, ADR-862).
pub(crate) const FLAG_PROBE_GEOMETRY: u32 = 16;
/// Instance flag (`Instance::info2.y`): alpha-tested.
const INST_ALPHA: u32 = 1;
/// Instance flag: thin transmissive (shadow rays pass through it).
const INST_TRANSMIT: u32 = 2;
/// Most medium layers the kernels read.
pub(crate) const MAX_MEDIUM_LAYERS: usize = 8;
/// Most sky bodies the kernels read.
pub(crate) const MAX_SKY_BODIES: usize = 4;
/// `vec4`s per triangle in `tri_attr` (`TRI_ATTR` in the shader).
pub(crate) const TRI_ATTR: usize = 8;

/// The tracer's rows for `suns` ([`Globals::extra_sun`]): the first
/// `regia_render::MAX_EXTRA_DIRECTIONAL` lit ones, as delta directions (ADR-7254).
pub(crate) fn extra_sun_rows(suns: &[crate::PtSun]) -> [[f32; 4]; 2 * regia_render::MAX_EXTRA_DIRECTIONAL] {
    let mut rows = [[0.0f32; 4]; 2 * regia_render::MAX_EXTRA_DIRECTIONAL];
    let lit = suns.iter().filter(|s| s.illuminance > 0.0);
    for (i, sun) in lit.take(regia_render::MAX_EXTRA_DIRECTIONAL).enumerate() {
        let toward = Vec3::from_array(sun.toward).normalize_or(Vec3::Y);
        rows[2 * i] = [toward.x, toward.y, toward.z, 1.0];
        rows[2 * i + 1] = [
            sun.color[0] * sun.illuminance,
            sun.color[1] * sun.illuminance,
            sun.color[2] * sun.illuminance,
            0.0,
        ];
    }
    rows
}

/// `Globals` in `pt.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub(crate) struct Globals {
    pub cam_pos: [f32; 4],
    pub cam_fwd: [f32; 4],
    pub cam_right: [f32; 4],
    pub cam_up: [f32; 4],
    pub dims: [u32; 4],
    pub counts: [u32; 4],
    pub sun_dir: [f32; 4],
    pub sun_l: [f32; 4],
    pub sky: [f32; 4],
    pub sky_dims: [u32; 4],
    pub sky_offs: [u32; 4],
    pub sky_sun: [f32; 4],
    pub dfg: [u32; 4],
    pub terrain0: [f32; 4],
    pub terrain1: [u32; 4],
    pub terrain_lv: [[u32; 4]; 4],
    pub misc: [f32; 4],
    /// Medium layer count, `env` offset of the layers, 1 with a cloud field, its `env` offset.
    pub media: [u32; 4],
    /// Escape distance of the non-aerial layers (metres), 0, 0, 0.
    pub media_f: [f32; 4],
    /// Sky body count, `env` offset of the headers, 0, 0.
    pub bodies: [u32; 4],
    /// Star peak radiance (0 = none), the camera's pixel angle (radians), 0, 0.
    pub stars: [f32; 4],
    /// World → clip of the terrain view albedo ([`crate::PtViewAlbedo`]), column-major.
    pub view_albedo_vp: [[f32; 4]; 4],
    /// Terrain view albedo texture index (NONE = off), its normal texture (NONE = none), all
    /// bounces (0/1), 0.
    pub view_albedo: [u32; 4],
    /// The view's eye (xyz), 0.
    pub view_albedo_eye: [f32; 4],
    /// Probe mode: rays per probe, probes in the batch, `env` offset of the probe records,
    /// octahedral moment resolution (all 0 for camera traces).
    pub probe: [u32; 4],
    /// Probe mode: moment distance clamp (metres), moment lobe exponent, luminance caps of the
    /// unscattered and the scattered contributions (cd/m², 0 = off).
    pub probe_f: [f32; 4],
    /// The frame's extra directional lights ([`crate::PtScene::extra_suns`], ADR-7254), traced
    /// as delta directions: per slot xyz toward the light (w = 1 when on), then rgb irradiance
    /// at normal incidence (w = 0).
    pub extra_sun: [[f32; 4]; 2 * regia_render::MAX_EXTRA_DIRECTIONAL],
}

/// `Instance` in `pt.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub(crate) struct GpuInstance {
    pub w2o: [[f32; 4]; 3],
    pub o2w: [[f32; 4]; 3],
    pub info: [u32; 4],
    pub info2: [u32; 4],
}

/// `Material` in `pt.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub(crate) struct GpuMaterial {
    pub base_color: [f32; 4],
    pub emissive: [f32; 4],
    pub params: [f32; 4],
    pub coat: [f32; 4],
    pub tex: [u32; 4],
    /// Anisotropy, rotation, ORM packed (0/1), one-sided emission (0/1).
    pub extra: [f32; 4],
    /// Detail strengths (albedo, normal, roughness), fade start.
    pub detail: [f32; 4],
    /// Detail UV scale, fade end, detail on (0/1).
    pub detail2: [f32; 4],
    /// ORM texture, detail image, detail normal, thin film map.
    pub tex2: [u32; 4],
    /// Transmission, dielectric F0 (from the IOR), 0, 0.
    pub transmit: [f32; 4],
    /// Volume transmittance at normal incidence (rgb), 0.
    pub volume: [f32; 4],
    /// Thin film: coverage (0 = none), IOR, thickness at map 0, thickness at map 1 (nm).
    pub film: [f32; 4],
    /// Material graph (ADR-2725): graph id (NONE = the standard surface stage), word offset of
    /// its block in `textures` (`crate::graph::GRAPH_BLOCK_WORDS`), 0, 0.
    pub graph: [u32; 4],
    /// Subsurface (ADR-2725): Burley `d` per channel (m), on (0/1).
    pub sss: [f32; 4],
    /// Subsurface: scattered share per channel, 0.
    pub sss_tint: [f32; 4],
}

/// `Light` in `pt.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub(crate) struct GpuLight {
    pub pos: [f32; 4],
    pub dir: [f32; 4],
    pub color: [f32; 4],
    pub extra: [f32; 4],
    pub refs: [u32; 4],
}

const LIGHT_POINT: u32 = 0;
const LIGHT_SPOT: u32 = 1;
const LIGHT_SPHERE: u32 = 2;
const LIGHT_TRIANGLE: u32 = 3;

/// The packed scene (everything except the per-render settings in [`Globals`]).
#[derive(Clone, Debug)]
pub(crate) struct Packed {
    pub globals: Globals,
    pub nodes: Vec<BvhNode>,
    pub tri_pos: Vec<[f32; 4]>,
    pub tri_attr: Vec<[f32; 4]>,
    pub instances: Vec<GpuInstance>,
    pub materials: Vec<GpuMaterial>,
    pub textures: Vec<u32>,
    pub lights: Vec<GpuLight>,
    pub env: Vec<f32>,
    pub terrain: Vec<f32>,
    /// Triangles across all BLASes (statistics).
    pub triangle_count: usize,
    /// The scene's material graphs: what the kernels must compose (ADR-2725).
    pub graphs: crate::graph::GraphSet,
    /// The scene has decals: the kernels compose the decal code (ADR-2951).
    pub decals: bool,
}

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn rows3x4(m: Mat4) -> [[f32; 4]; 3] {
    let r = m.transpose();
    [r.x_axis.to_array(), r.y_axis.to_array(), r.z_axis.to_array()]
}

fn pack_rgba8(c: [f32; 4]) -> f32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    f32::from_bits(q(c[0]) | (q(c[1]) << 8) | (q(c[2]) << 16) | (q(c[3]) << 24))
}

fn tex_index(t: Option<usize>, count: usize) -> u32 {
    match t {
        Some(i) if i < count => i as u32,
        _ => NONE,
    }
}

/// Mean of a texture's RGB (linear), for light power estimates.
fn texture_mean(t: &crate::scene::PtTexture) -> [f32; 3] {
    let n = (t.width as usize * t.height as usize).max(1);
    let mut sum = [0.0f64; 3];
    for px in t.rgba.as_chunks::<4>().0.iter() {
        for k in 0..3 {
            let v = f32::from(px[k]) / 255.0;
            let v = if t.srgb { srgb_to_linear(v) } else { v };
            sum[k] += f64::from(v);
        }
    }
    [
        (sum[0] / n as f64) as f32,
        (sum[1] / n as f64) as f32,
        (sum[2] / n as f64) as f32,
    ]
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// The runtime's sky-view mapping (`atmosphere.rs::sky_uv`), for building the sampling CDF.
pub(crate) fn sky_uv(view: Vec3, sun: Vec3) -> (f32, f32) {
    let up = Vec3::Y;
    let sun_h = sun - up * sun.dot(up);
    let view_h = view - up * view.dot(up);
    let sun_n = if sun_h.length() > 1e-4 {
        sun_h.normalize()
    } else {
        Vec3::Z
    };
    let view_n = if view_h.length() > 1e-4 {
        view_h.normalize()
    } else {
        up
    };
    let sin_az = sun_n.cross(view_n).dot(up);
    let cos_az = sun_n.dot(view_n).clamp(-1.0, 1.0);
    let azimuth = sin_az.atan2(cos_az);
    let u = (azimuth / std::f32::consts::TAU + 0.5).rem_euclid(1.0);
    let lat = view.y.clamp(-1.0, 1.0).asin();
    let v = 0.5 + 0.5 * lat.signum() * (lat.abs() / (0.5 * std::f32::consts::PI)).sqrt();
    (u, v.clamp(0.0, 1.0))
}

/// Bilinear lookup of an RGBA table at (u, v), U wrapped and V clamped (`pt.wgsl::sky_table`).
pub(crate) fn table_sample(width: u32, height: u32, texels: &[[f32; 4]], u: f32, v: f32) -> [f32; 3] {
    let w = width.max(1) as f32;
    let h = height.max(1) as f32;
    let fu = u.rem_euclid(1.0) * w - 0.5;
    let fv = v.clamp(0.0, 0.999) * h - 0.5;
    let x0 = fu.floor();
    let y0 = fv.floor();
    let tx = fu - x0;
    let ty = fv - y0;
    let at = |x: f32, y: f32| {
        let xi = (x.rem_euclid(w) as u32) % width;
        let yi = (y.clamp(0.0, h - 1.0) as u32) % height;
        texels[(yi * width + xi) as usize]
    };
    let (a, b, c, d) = (
        at(x0, y0),
        at(x0 + 1.0, y0),
        at(x0, y0 + 1.0),
        at(x0 + 1.0, y0 + 1.0),
    );
    let mut out = [0.0; 3];
    for k in 0..3 {
        let top = a[k] + (b[k] - a[k]) * tx;
        let bot = c[k] + (d[k] - c[k]) * tx;
        out[k] = top + (bot - top) * ty;
    }
    out
}

fn equirect_dir(u: f32, v: f32) -> Vec3 {
    let theta = v * std::f32::consts::PI;
    let phi = u * std::f32::consts::TAU;
    Vec3::new(theta.sin() * phi.cos(), theta.cos(), theta.sin() * phi.sin())
}

/// Piecewise-constant 2-D distribution over the equirect grid (PBRT 4 § A.5): returns
/// (marginal CDF, conditional CDFs, function, integral).
fn sky_distribution(
    w: u32,
    h: u32,
    radiance: impl Fn(Vec3) -> [f32; 3],
) -> (Vec<f32>, Vec<f32>, Vec<f32>, f32) {
    sky_distribution_ss(w, h, 1, radiance)
}

/// [`sky_distribution`] with each cell's function the mean of `ss × ss` sub-samples, so a map
/// finer than the grid puts a small hot spot (an HDRI's sun) in the cell that holds it.
fn sky_distribution_ss(
    w: u32,
    h: u32,
    ss: u32,
    radiance: impl Fn(Vec3) -> [f32; 3],
) -> (Vec<f32>, Vec<f32>, Vec<f32>, f32) {
    let (wu, hu) = (w as usize, h as usize);
    let ss = ss.max(1);
    let mut func = vec![0.0f32; wu * hu];
    let mut peak = 0.0f32;
    for j in 0..hu {
        let v = (j as f32 + 0.5) / h as f32;
        let sin_t = (v * std::f32::consts::PI).sin();
        for i in 0..wu {
            let mut sum = 0.0f32;
            for sj in 0..ss {
                let vs = (j as f32 + (sj as f32 + 0.5) / ss as f32) / h as f32;
                for si in 0..ss {
                    let us = (i as f32 + (si as f32 + 0.5) / ss as f32) / w as f32;
                    sum += lum(radiance(equirect_dir(us, vs))).max(0.0);
                }
            }
            let f = sum / (ss * ss) as f32 * sin_t;
            func[j * wu + i] = f;
            peak = peak.max(f);
        }
    }
    // A small floor keeps every direction reachable by NEE (MIS stays unbiased either way).
    let floor = peak * 1e-4;
    for (j, row) in func.chunks_mut(wu).enumerate() {
        let sin_t = ((j as f32 + 0.5) / h as f32 * std::f32::consts::PI).sin();
        for f in row {
            *f = (*f).max(floor * sin_t);
        }
    }
    let mut cond = vec![0.0f32; hu * (wu + 1)];
    let mut row_int = vec![0.0f64; hu];
    for j in 0..hu {
        let base = j * (wu + 1);
        let mut acc = 0.0f64;
        for i in 0..wu {
            acc += f64::from(func[j * wu + i]) / f64::from(w);
            cond[base + i + 1] = acc as f32;
        }
        row_int[j] = acc;
        for i in 1..=wu {
            cond[base + i] = if acc > 0.0 {
                (f64::from(cond[base + i]) / acc) as f32
            } else {
                i as f32 / w as f32
            };
        }
        cond[base + wu] = 1.0;
    }
    let mut marg = vec![0.0f32; hu + 1];
    let mut acc = 0.0f64;
    for j in 0..hu {
        acc += row_int[j] / f64::from(h);
        marg[j + 1] = acc as f32;
    }
    let integral = acc;
    for m in marg.iter_mut().skip(1) {
        *m = if integral > 0.0 {
            (f64::from(*m) / integral) as f32
        } else {
            0.0
        };
    }
    marg[hu] = 1.0;
    (marg, cond, func, integral as f32)
}

/// A cut heightfield sample on the GPU (a NaN height: a hole, `PtHeightfield::cut_holes`). The
/// kernel drops every cell with a corner below `HF_CUT` (−1e8 m, `pt.wgsl`); an empty pyramid
/// box sits here, a million kilometers below any map, where no hit can come from.
const CUT_HEIGHT: f32 = -1.0e9;

/// Min/max pyramid over the heightfield cells; returns (data, level offsets, levels). A cell with
/// a cut (NaN) corner is empty: it bounds nothing, and a box with nothing in it is written at
/// [`CUT_HEIGHT`].
fn heightfield_pyramid(t: &PtHeightfield, base: usize) -> (Vec<f32>, Vec<u32>) {
    let cw = t.width - 1;
    let ch = t.depth - 1;
    let mut data = Vec::new();
    let mut offsets = Vec::new();
    // Level 0: each cell's four corners.
    let mut level: Vec<[f32; 2]> = Vec::with_capacity((cw * ch) as usize);
    for j in 0..ch {
        for i in 0..cw {
            let hs = [
                t.height(i, j),
                t.height(i + 1, j),
                t.height(i, j + 1),
                t.height(i + 1, j + 1),
            ];
            if hs.iter().any(|h| !h.is_finite()) {
                level.push([f32::INFINITY, f32::NEG_INFINITY]);
                continue;
            }
            let lo = hs.iter().copied().fold(f32::INFINITY, f32::min);
            let hi = hs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            level.push([lo, hi]);
        }
    }
    let (mut w, mut h) = (cw, ch);
    loop {
        offsets.push((base + data.len()) as u32);
        for mm in &level {
            if mm[0] > mm[1] {
                data.extend_from_slice(&[CUT_HEIGHT, CUT_HEIGHT]);
            } else {
                data.push(mm[0]);
                data.push(mm[1]);
            }
        }
        if w == 1 && h == 1 {
            break;
        }
        let nw = w.div_ceil(2);
        let nh = h.div_ceil(2);
        let mut next = Vec::with_capacity((nw * nh) as usize);
        for j in 0..nh {
            for i in 0..nw {
                let mut lo = f32::INFINITY;
                let mut hi = f32::NEG_INFINITY;
                for (di, dj) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let (x, y) = (i * 2 + di, j * 2 + dj);
                    if x < w && y < h {
                        let mm = level[(y * w + x) as usize];
                        lo = lo.min(mm[0]);
                        hi = hi.max(mm[1]);
                    }
                }
                next.push([lo, hi]);
            }
        }
        level = next;
        w = nw;
        h = nh;
    }
    (data, offsets)
}

/// A mesh's BVH and triangle data with node and triangle indices relative to the mesh
/// (rebased when a scene is packed).
#[derive(Debug)]
struct CachedBlas {
    nodes: Vec<BvhNode>,
    tri_pos: Vec<[f32; 4]>,
    tri_attr: Vec<[f32; 4]>,
    bounds: Aabb,
}

/// Bottom-level BVHs kept between traces, keyed by mesh content, so tracing the same world
/// again (a moved camera, moved instances) rebuilds only the top level
/// ([`crate::PathTracer::begin_cached`]).
#[derive(Debug, Default)]
pub struct PtCache {
    blas: HashMap<u64, Arc<CachedBlas>>,
    built: u64,
    reused: u64,
}

impl PtCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bottom-level BVHs built by packs through this cache so far.
    #[must_use]
    pub fn blas_built(&self) -> u64 {
        self.built
    }

    /// Bottom-level BVHs taken from the cache so far.
    #[must_use]
    pub fn blas_reused(&self) -> u64 {
        self.reused
    }
}

/// Content hash of everything a BLAS is built from.
fn mesh_key(mesh: &PtMesh) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let bytes = |h: &mut std::collections::hash_map::DefaultHasher, b: &[u8]| {
        b.len().hash(h);
        h.write(b);
    };
    bytes(&mut h, bytemuck::cast_slice(&mesh.positions));
    bytes(&mut h, bytemuck::cast_slice(&mesh.normals));
    bytes(&mut h, bytemuck::cast_slice(&mesh.uvs));
    bytes(&mut h, bytemuck::cast_slice(&mesh.colors));
    bytes(&mut h, bytemuck::cast_slice(&mesh.tangents));
    bytes(&mut h, bytemuck::cast_slice(&mesh.indices));
    h.finish()
}

/// A BLAS built ahead of `pack_with`'s mesh loop: `None` for an empty mesh, `Err` with the
/// scene error's message.
type Prebuilt = Result<Option<Arc<CachedBlas>>, String>;

/// `f` over `items` on the machine's cores, results in item order (ADR-862: one BLAS per
/// mesh at a time per core).
fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(items.len());
    if threads <= 1 {
        return items.iter().map(f).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut out: Vec<(usize, R)> = std::thread::scope(|s| {
        let mut workers = Vec::with_capacity(threads);
        for _ in 0..threads {
            workers.push(s.spawn(|| {
                let mut mine = Vec::new();
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = items.get(i) else {
                        break;
                    };
                    mine.push((i, f(item)));
                }
                mine
            }));
        }
        let mut all = Vec::with_capacity(items.len());
        for w in workers {
            all.extend(w.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)));
        }
        all
    });
    out.sort_unstable_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}

/// Builds one mesh's BVH and triangle data (indices relative to the mesh); `None` for an
/// empty mesh.
fn build_blas(mesh: &PtMesh) -> Result<Option<CachedBlas>, PtError> {
    let tris = mesh.triangle_count();
    let vcount = mesh.positions.len();
    let valid = |i: u32| (i as usize) < vcount;
    if tris == 0 || !mesh.indices.iter().copied().all(valid) {
        if tris > 0 {
            return Err(PtError::Scene("mesh index out of range".into()));
        }
        return Ok(None);
    }
    let bounds: Vec<Aabb> = mesh
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            Aabb::from_points(&[
                mesh.positions[t[0] as usize],
                mesh.positions[t[1] as usize],
                mesh.positions[t[2] as usize],
            ])
        })
        .collect();
    let bvh = Bvh::build(&bounds);
    let mut tri_pos = Vec::with_capacity(tris * 3);
    let mut tri_attr = Vec::with_capacity(tris * TRI_ATTR);
    for (local, &t) in bvh.order.iter().enumerate() {
        let idx = &mesh.indices[t as usize * 3..t as usize * 3 + 3];
        let mut n = [[0.0f32; 3]; 3];
        let mut uv = [[0.0f32; 2]; 3];
        let mut col = [[1.0f32; 4]; 3];
        let mut tan = [[0.0f32; 4]; 3];
        for k in 0..3 {
            let v = idx[k] as usize;
            let p = mesh.positions[v];
            tri_pos.push([p[0], p[1], p[2], 0.0]);
            n[k] = mesh.normals.get(v).copied().unwrap_or([0.0; 3]);
            uv[k] = mesh.uvs.get(v).copied().unwrap_or([0.0; 2]);
            col[k] = mesh.colors.get(v).copied().unwrap_or([1.0; 4]);
            tan[k] = mesh.tangents.get(v).copied().unwrap_or([0.0; 4]);
        }
        tri_attr.push([n[0][0], n[0][1], n[0][2], uv[0][0]]);
        tri_attr.push([n[1][0], n[1][1], n[1][2], uv[0][1]]);
        tri_attr.push([n[2][0], n[2][1], n[2][2], uv[1][0]]);
        tri_attr.push([uv[1][1], uv[2][0], uv[2][1], f32::from_bits(local as u32)]);
        tri_attr.push([pack_rgba8(col[0]), pack_rgba8(col[1]), pack_rgba8(col[2]), 0.0]);
        tri_attr.extend_from_slice(&tan);
    }
    Ok(Some(CachedBlas {
        bounds: bvh.bounds(),
        nodes: bvh.nodes,
        tri_pos,
        tri_attr,
    }))
}

/// Packs `scene` (the camera and the sun/sky go into `globals`; per-render settings are filled
/// by the tracer).
#[cfg(test)]
pub(crate) fn pack(scene: &PtScene) -> Result<Packed, PtError> {
    pack_with(scene, None)
}

/// [`pack`] reusing (and refilling) the bottom-level BVHs of `cache`.
pub(crate) fn pack_with(scene: &PtScene, mut cache: Option<&mut PtCache>) -> Result<Packed, PtError> {
    let _span = tracing::info_span!("pt_pack").entered();
    // Quad area lights become emissive two-triangle meshes (NEE + MIS through the emissive
    // triangle path); the scene is only copied when it has any.
    let with_quads;
    let scene = if scene.lights.iter().any(|l| matches!(l, PtLight::Quad { .. })) {
        with_quads = quads_as_meshes(scene);
        &with_quads
    } else {
        scene
    };
    let tex_count = scene.textures.len();
    // ---- BLAS per mesh (from the cache when it holds the same mesh) ----
    let mut nodes: Vec<BvhNode> = Vec::new();
    let mut tri_pos: Vec<[f32; 4]> = Vec::new();
    let mut tri_attr: Vec<[f32; 4]> = Vec::new();
    // (root node, first triangle, triangle count, object bounds) per mesh; None for empty meshes.
    let mut blas: Vec<Option<(u32, u32, u32, Aabb)>> = Vec::with_capacity(scene.meshes.len());
    let mut used: HashMap<u64, Arc<CachedBlas>> = HashMap::new();
    // Every BLAS the scene needs is built up front, the meshes on several threads (ADR-862);
    // the loop below then takes them in mesh order, so the result, the cache counters and the
    // first error reported are those of building one mesh after another.
    let keys: Vec<Option<u64>> = scene
        .meshes
        .iter()
        .map(|m| cache.is_some().then(|| mesh_key(m)))
        .collect();
    let mut to_build: Vec<usize> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (mi, key) in keys.iter().enumerate() {
        match (key, cache.as_deref()) {
            (Some(key), Some(cache)) => {
                if !cache.blas.contains_key(key) && seen.insert(*key) {
                    to_build.push(mi);
                }
            }
            _ => to_build.push(mi),
        }
    }
    let mut prebuilt: HashMap<usize, Prebuilt> = to_build
        .iter()
        .copied()
        .zip(parallel_map(&to_build, |&mi| {
            build_blas(&scene.meshes[mi])
                .map(|b| b.map(Arc::new))
                .map_err(|e| match e {
                    PtError::Scene(m) => m,
                    other => other.to_string(),
                })
        }))
        .collect();
    let first_of_key: HashMap<u64, usize> = to_build
        .iter()
        .filter_map(|&mi| keys[mi].map(|k| (k, mi)))
        .collect();
    let mut take = |mi: usize| -> Result<Option<Arc<CachedBlas>>, PtError> {
        let source = keys[mi].and_then(|k| first_of_key.get(&k).copied()).unwrap_or(mi);
        match prebuilt.get(&source) {
            Some(Ok(b)) => Ok(b.clone()),
            Some(Err(m)) => Err(PtError::Scene(m.clone())),
            // Not prebuilt (cannot happen: every mesh not in the cache is): build it now.
            None => {
                let b = build_blas(&scene.meshes[mi])?.map(Arc::new);
                prebuilt.insert(mi, Ok(b.clone()));
                Ok(b)
            }
        }
    };
    for (mi, mesh) in scene.meshes.iter().enumerate() {
        let built = match cache.as_deref_mut() {
            Some(cache) => {
                let key = keys[mi].unwrap_or_else(|| mesh_key(mesh));
                let hit = cache.blas.get(&key).or_else(|| used.get(&key)).cloned();
                if let Some(b) = hit {
                    cache.reused += 1;
                    used.insert(key, Arc::clone(&b));
                    Some(b)
                } else {
                    cache.built += 1;
                    let b = take(mi)?;
                    if let Some(b) = &b {
                        used.insert(key, Arc::clone(b));
                    }
                    b
                }
            }
            None => take(mi)?,
        };
        let Some(b) = built else {
            blas.push(None);
            continue;
        };
        let first_tri = (tri_pos.len() / 3) as u32;
        let root = nodes.len() as u32;
        nodes.extend(b.nodes.iter().map(|n| {
            let mut n = *n;
            n.a += if n.is_leaf() { first_tri } else { root };
            n
        }));
        tri_pos.extend_from_slice(&b.tri_pos);
        tri_attr.extend_from_slice(&b.tri_attr);
        blas.push(Some((root, first_tri, (b.tri_pos.len() / 3) as u32, b.bounds)));
    }
    if let Some(cache) = cache {
        // Keep what this scene uses (bounded memory across traces).
        cache.blas = used;
    }
    let triangle_count = tri_pos.len() / 3;
    debug_assert_eq!(tri_attr.len(), triangle_count * TRI_ATTR, "tri_attr stride");

    // ---- Instances (+ the terrain) and the TLAS ----
    struct Pending {
        gpu: GpuInstance,
        bounds: Aabb,
        emissive: Option<(u32, u32)>,
    }
    let mut pending: Vec<Pending> = Vec::new();
    for inst in &scene.instances {
        let Some(Some((root, first, count, ob))) = blas.get(inst.mesh).copied() else {
            continue;
        };
        let Some(material) = scene.materials.get(inst.material) else {
            return Err(PtError::Scene(format!(
                "instance material {} missing",
                inst.material
            )));
        };
        let m = inst.transform;
        let det = m.determinant();
        if !det.is_finite() || det.abs() < 1e-20 {
            continue;
        }
        let inv = m.inverse();
        let mut wb = Aabb::EMPTY;
        for corner in 0..8 {
            let c = Vec3::new(
                if corner & 1 == 0 { ob.min[0] } else { ob.max[0] },
                if corner & 2 == 0 { ob.min[1] } else { ob.max[1] },
                if corner & 4 == 0 { ob.min[2] } else { ob.max[2] },
            );
            wb.grow_point(m.transform_point3(c).to_array());
        }
        // Glass is a thin dielectric, not a cut-out: shadow rays pass it, camera rays shade it.
        let flags = if material.transmission > 0.0 {
            INST_TRANSMIT
        } else if material.alpha_mode != 0 {
            INST_ALPHA
        } else {
            0
        };
        pending.push(Pending {
            gpu: GpuInstance {
                w2o: rows3x4(inv),
                o2w: rows3x4(m),
                info: [0, root, first, inst.material as u32],
                info2: [NONE, flags, 0, 0],
            },
            bounds: wb,
            emissive: (material.is_emissive() || material.unlit).then_some((first, count)),
        });
    }
    let mut terrain = Vec::new();
    let mut terrain_info = None;
    if let Some(t) = &scene.terrain {
        if t.width < 2 || t.depth < 2 || t.heights.len() as u64 != u64::from(t.width) * u64::from(t.depth) {
            return Err(PtError::Scene(
                "terrain heightfield has the wrong sample count".into(),
            ));
        }
        if !t.spacing.iter().all(|s| s.is_finite() && *s > 0.0) || !t.origin.iter().all(|o| o.is_finite()) {
            return Err(PtError::Scene(
                "terrain spacing must be positive and finite, and the origin finite".into(),
            ));
        }
        if t.width - 1 > MAX_HEIGHTFIELD_CELLS || t.depth - 1 > MAX_HEIGHTFIELD_CELLS {
            return Err(PtError::Scene(format!(
                "terrain {}x{} exceeds {MAX_HEIGHTFIELD_CELLS} cells per side",
                t.width, t.depth
            )));
        }
        if t.material >= scene.materials.len() {
            return Err(PtError::Scene("terrain material missing".into()));
        }
        terrain.extend(
            t.heights
                .iter()
                .map(|&h| if h.is_finite() { h } else { CUT_HEIGHT }),
        );
        let (pyramid, offsets) = heightfield_pyramid(t, terrain.len());
        terrain.extend_from_slice(&pyramid);
        if offsets.len() > 16 {
            return Err(PtError::Scene("terrain pyramid deeper than 16 levels".into()));
        }
        let (lo, hi) = t
            .heights
            .iter()
            .filter(|h| h.is_finite())
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &h| {
                (a.min(h), b.max(h))
            });
        // Every sample cut: an empty box at the abyss.
        let (lo, hi) = if lo <= hi {
            (lo, hi)
        } else {
            (CUT_HEIGHT, CUT_HEIGHT)
        };
        let far = [
            t.origin[0] + (t.width - 1) as f32 * t.spacing[0],
            t.origin[1] + (t.depth - 1) as f32 * t.spacing[1],
        ];
        let identity = rows3x4(Mat4::IDENTITY);
        pending.push(Pending {
            gpu: GpuInstance {
                w2o: identity,
                o2w: identity,
                info: [1, 0, 0, t.material as u32],
                info2: [NONE, 0, 0, 0],
            },
            bounds: Aabb {
                min: [t.origin[0].min(far[0]), lo, t.origin[1].min(far[1])],
                max: [t.origin[0].max(far[0]), hi, t.origin[1].max(far[1])],
            },
            emissive: None,
        });
        terrain_info = Some((offsets, t));
    }
    let tlas_bounds: Vec<Aabb> = pending.iter().map(|p| p.bounds).collect();
    let tlas_root;
    let mut instances = Vec::with_capacity(pending.len());
    let mut emissive_instances = Vec::new();
    if pending.is_empty() {
        tlas_root = NONE;
    } else {
        let mut tlas = Bvh::build(&tlas_bounds);
        tlas_root = nodes.len() as u32;
        tlas.rebase(tlas_root, 0);
        nodes.extend_from_slice(&tlas.nodes);
        for &i in &tlas.order {
            let p = &pending[i as usize];
            if let Some(range) = p.emissive {
                emissive_instances.push((instances.len() as u32, range));
            }
            instances.push(p.gpu);
        }
    }

    // ---- Materials and textures ----
    let mut materials: Vec<GpuMaterial> = scene
        .materials
        .iter()
        .map(|m| GpuMaterial {
            base_color: m.base_color,
            emissive: [m.emissive[0], m.emissive[1], m.emissive[2], 0.0],
            params: [m.metallic, m.roughness, m.normal_scale, m.alpha_cutoff],
            coat: [
                m.clearcoat,
                m.clearcoat_roughness,
                if m.multi_scatter { 1.0 } else { 0.0 },
                if m.unlit {
                    1.0
                } else if m.lambert {
                    2.0
                } else {
                    0.0
                },
            ],
            tex: [
                tex_index(m.albedo, tex_count),
                tex_index(m.normal, tex_count),
                tex_index(m.emissive_map, tex_count),
                m.alpha_mode,
            ],
            extra: [
                m.anisotropy.clamp(-1.0, 1.0),
                m.anisotropy_rotation,
                if m.orm_packed { 1.0 } else { 0.0 },
                if m.one_sided_emission { 1.0 } else { 0.0 },
            ],
            detail: m.detail.map_or([0.0; 4], |d| {
                [d.strength[0], d.strength[1], d.strength[2], d.fade_start]
            }),
            detail2: m.detail.map_or([1.0, 1.0, 0.0, 0.0], |d| {
                [d.uv_scale[0], d.uv_scale[1], d.fade_end, 1.0]
            }),
            tex2: [
                if m.orm_packed {
                    tex_index(m.orm, tex_count)
                } else {
                    NONE
                },
                m.detail.map_or(NONE, |d| tex_index(d.image, tex_count)),
                m.detail.map_or(NONE, |d| tex_index(d.normal, tex_count)),
                m.thin_film.map_or(NONE, |f| tex_index(f.map, tex_count)),
            ],
            // y floored above 0: the shader reads 0 as "unset" (0.04), and IOR 1.0 is a legal F0
            // of 0. z: rough transmission's perceptual roughness (ADR-2075), 0 goes straight through.
            transmit: [
                m.transmission.clamp(0.0, 1.0),
                m.dielectric_f0().max(1.0e-6),
                m.transmission_roughness.clamp(0.0, 1.0),
                0.0,
            ],
            volume: [
                m.transmission_tint[0].clamp(0.0, 1.0),
                m.transmission_tint[1].clamp(0.0, 1.0),
                m.transmission_tint[2].clamp(0.0, 1.0),
                0.0,
            ],
            film: m.thin_film.map_or([0.0; 4], |f| {
                [
                    f.factor.clamp(0.0, 1.0),
                    f.ior.max(1.0),
                    f.thickness_min.max(0.0),
                    f.thickness_max.max(0.0),
                ]
            }),
            graph: [NONE, 0, 0, 0],
            sss: [0.0; 4],
            sss_tint: [0.0; 4],
        })
        .collect();
    let mut textures: Vec<u32> = vec![0; tex_count * 4];
    for (i, t) in scene.textures.iter().enumerate() {
        let texels = u64::from(t.width) * u64::from(t.height);
        if (t.rgba.len() as u64) < texels.saturating_mul(4) || texels == 0 {
            return Err(PtError::Scene(format!("texture {i} has {} bytes", t.rgba.len())));
        }
        let texels = texels as usize;
        let offset = textures.len() as u32;
        textures[i * 4] = offset;
        textures[i * 4 + 1] = t.width;
        textures[i * 4 + 2] = t.height;
        textures[i * 4 + 3] = u32::from(t.srgb);
        textures.extend(
            t.rgba[..texels * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| u32::from_le_bytes([p[0], p[1], p[2], p[3]])),
        );
    }

    // ---- Material graphs (ADR-2725): each composed graph material's block after the textures ----
    let graphs = crate::graph::graph_set(scene);
    for g in &scene.surface_graphs {
        let Some(Some(id)) = graphs.ids.get(g.material).copied() else {
            continue;
        };
        if materials[g.material].graph[0] != NONE {
            continue;
        }
        let offset = textures.len() as u32;
        for slot in &g.params {
            textures.extend(slot.iter().map(|v| v.to_bits()));
        }
        textures.extend(g.textures.iter().map(|t| tex_index(*t, tex_count)));
        textures.push(0);
        debug_assert_eq!(textures.len() - offset as usize, crate::graph::GRAPH_BLOCK_WORDS);
        materials[g.material].graph = [id, offset, 0, 0];
    }
    // ---- Subsurface (ADR-2725): Burley's d and the scattered share ----
    for sss in &scene.subsurface {
        let Some(m) = materials.get_mut(sss.material) else {
            continue;
        };
        let d = sss.d.map(|v| if v.is_finite() { v.max(0.0) } else { 0.0 });
        let tint = sss
            .tint
            .map(|v| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 });
        // A channel without reach (d = 0) does not scatter: its light stays at the hit. Its
        // tint goes to 0 so the kernel's stay branch keeps all of it (with a tint above 0 the
        // scattered share would be lost, its profile being 0).
        let tint: [f32; 3] = std::array::from_fn(|c| if d[c] > 0.0 { tint[c] } else { 0.0 });
        if d.iter().any(|&v| v > 0.0) && tint.iter().any(|&v| v > 0.0) {
            m.sss = [d[0], d[1], d[2], 1.0];
            m.sss_tint = [tint[0], tint[1], tint[2], 0.0];
        }
    }

    // ---- Lights: analytic + one per emissive triangle, power-weighted ----
    let mut lights: Vec<(GpuLight, f32)> = Vec::new();
    // IES profile bands (ADR-2592) after the textures: raw f32 bits, 256 θ × 32 φ, one band per profile.
    let mut bands: Vec<(*const regia_asset::ies::CookedIesProfile, u32)> = Vec::new();
    for (light_index, l) in scene.lights.iter().enumerate() {
        let power = l.power();
        let mut g = match *l {
            PtLight::Point { position, intensity } => GpuLight {
                pos: [position[0], position[1], position[2], 0.0],
                dir: [0.0; 4],
                color: [intensity[0], intensity[1], intensity[2], 0.0],
                extra: [0.0; 4],
                refs: [0, 0, LIGHT_POINT, 0],
            },
            PtLight::Spot {
                position,
                direction,
                intensity,
                inner_cos,
                outer_cos,
            } => {
                let d = Vec3::from_array(direction).normalize_or(Vec3::NEG_Y);
                GpuLight {
                    pos: [position[0], position[1], position[2], 0.0],
                    dir: [d.x, d.y, d.z, 0.0],
                    color: [intensity[0], intensity[1], intensity[2], 0.0],
                    extra: [inner_cos, outer_cos, 0.0, 0.0],
                    refs: [0, 0, LIGHT_SPOT, 0],
                }
            }
            PtLight::Sphere {
                position,
                radius,
                radiance,
            } => GpuLight {
                pos: [position[0], position[1], position[2], 0.0],
                dir: [0.0, 0.0, 0.0, radius.max(1e-6)],
                color: [radiance[0], radiance[1], radiance[2], 0.0],
                extra: [0.0; 4],
                refs: [0, 0, LIGHT_SPHERE, 0],
            },
            // A sized spot (ADR-2592): the sphere, its cone in `dir.xyz` and `extra.xy`, `extra.w` = 1.
            PtLight::SphereSpot {
                position,
                radius,
                radiance,
                direction,
                inner_cos,
                outer_cos,
            } => {
                let d = Vec3::from_array(direction).normalize_or(Vec3::NEG_Y);
                GpuLight {
                    pos: [position[0], position[1], position[2], 0.0],
                    dir: [d.x, d.y, d.z, radius.max(1e-6)],
                    color: [radiance[0], radiance[1], radiance[2], 0.0],
                    extra: [inner_cos, outer_cos, 0.0, 1.0],
                    refs: [0, 0, LIGHT_SPHERE, 0],
                }
            }
            // Converted to emissive meshes by `quads_as_meshes`.
            PtLight::Quad { .. } => continue,
        };
        if let Some(p) = scene.light_profiles.iter().find(|p| p.light == light_index) {
            let key = std::sync::Arc::as_ptr(&p.profile);
            let offset = match bands.iter().find(|(k, _)| *k == key) {
                Some(&(_, offset)) => offset,
                None => {
                    let offset = textures.len() as u32;
                    textures.extend(p.profile.values.iter().map(|v| v.to_bits()));
                    bands.push((key, offset));
                    offset
                }
            };
            // refs.w = band offset + 1 (0 = none), refs.y = the turn; dir.xyz = the nadir (a spot's is its
            // direction already, a point's and a sphere's is unused).
            g.refs[3] = offset + 1;
            g.refs[1] = u32::from(p.turn);
            if !matches!(l, PtLight::Spot { .. } | PtLight::SphereSpot { .. }) {
                g.dir[0] = p.nadir[0];
                g.dir[1] = p.nadir[1];
                g.dir[2] = p.nadir[2];
            }
        }
        if power > 0.0 && power.is_finite() {
            lights.push((g, power));
        }
    }
    for (inst_index, (first, count)) in emissive_instances {
        let inst = instances[inst_index as usize];
        let material = &scene.materials[inst.info[3] as usize];
        let mut radiance = if material.unlit {
            [
                material.base_color[0],
                material.base_color[1],
                material.base_color[2],
            ]
        } else {
            material.emissive
        };
        if let Some(t) = material.emissive_map.and_then(|i| scene.textures.get(i)) {
            let mean = texture_mean(t);
            radiance = [
                radiance[0] * mean[0],
                radiance[1] * mean[1],
                radiance[2] * mean[2],
            ];
        }
        let base = lights.len() as u32;
        instances[inst_index as usize].info2[0] = base;
        let o2w = Mat4::from_cols(
            Vec4::from(inst.o2w[0]),
            Vec4::from(inst.o2w[1]),
            Vec4::from(inst.o2w[2]),
            Vec4::W,
        )
        .transpose();
        for t in 0..count {
            let tri = first + t;
            let p = |k: u32| {
                let q = tri_pos[(tri * 3 + k) as usize];
                o2w.transform_point3(Vec3::new(q[0], q[1], q[2]))
            };
            let area = 0.5 * (p(1) - p(0)).cross(p(2) - p(0)).length();
            let power = std::f32::consts::PI * lum(radiance) * area;
            let g = GpuLight {
                pos: [0.0; 4],
                dir: [0.0; 4],
                color: [0.0; 4],
                extra: [0.0; 4],
                refs: [inst_index, tri, LIGHT_TRIANGLE, 0],
            };
            // Zero-power triangles stay in the list (indices must line up) with pdf 0.
            lights.push((g, if power.is_finite() { power.max(0.0) } else { 0.0 }));
        }
    }
    let total: f64 = lights.iter().map(|(_, p)| f64::from(*p)).sum();
    let mut acc = 0.0f64;
    let mut gpu_lights = Vec::with_capacity(lights.len());
    for (mut g, p) in lights {
        let pdf = if total > 0.0 { f64::from(p) / total } else { 0.0 };
        acc += pdf;
        g.color[3] = pdf as f32;
        g.extra[2] = acc as f32;
        gpu_lights.push(g);
    }
    if let Some(last) = gpu_lights.last_mut() {
        last.extra[2] = 2.0;
    }

    // ---- Environment: DFG table, sky table, sky CDF ----
    let mut env: Vec<f32> = Vec::new();
    let lut = regia_render::build_brdf_lut();
    let dfg_offset = env.len() as u32;
    // Four floats per texel: scale, bias, the single-scatter albedo and its hemispherical
    // average (`pt.wgsl::dfg`).
    for px in lut.rgba.as_chunks::<4>().0.iter() {
        env.extend(px.iter().map(|&c| f32::from(c) / 255.0));
    }
    let mut sky = [0.0f32; 4];
    let mut sky_dims = [0u32; 4];
    let mut sky_offs = [0u32; 4];
    let mut sky_sun = [0.0f32; 4];
    let mut sky_scale = 1.0f32;
    let distribution = match &scene.sky {
        PtSky::None => None,
        PtSky::Uniform(c) => {
            sky = [c[0], c[1], c[2], 1.0];
            let c = *c;
            Some((
                UNIFORM_CDF,
                sky_distribution(UNIFORM_CDF.0, UNIFORM_CDF.1, move |_| c),
            ))
        }
        PtSky::Table {
            width,
            height,
            texels,
            sun_direction,
            scale,
        } => {
            if texels.len() != (*width as usize) * (*height as usize) || *width == 0 || *height == 0 {
                return Err(PtError::Scene("sky table has the wrong texel count".into()));
            }
            sky = [0.0, 0.0, 0.0, 2.0];
            sky_scale = *scale;
            sky_offs[0] = env.len() as u32;
            for t in texels {
                env.extend_from_slice(t);
            }
            sky_dims[0] = *width;
            sky_dims[1] = *height;
            let sun = Vec3::from_array(*sun_direction).normalize_or(Vec3::Y);
            sky_sun[0] = sun.x;
            sky_sun[1] = sun.y;
            sky_sun[2] = sun.z;
            let (w, h, s) = (*width, *height, *scale);
            Some((
                SKY_CDF,
                sky_distribution(SKY_CDF.0, SKY_CDF.1, |d| {
                    let (u, v) = sky_uv(d, sun);
                    let c = table_sample(w, h, texels, u, v);
                    [c[0] * s, c[1] * s, c[2] * s]
                }),
            ))
        }
        PtSky::Equirect {
            width,
            height,
            texels,
            rotation,
            scale,
        } => {
            if texels.len() != (*width as usize) * (*height as usize) || *width == 0 || *height == 0 {
                return Err(PtError::Scene("equirect sky has the wrong texel count".into()));
            }
            if texels.len() > crate::env_map::MAX_EQUIRECT_TEXELS {
                return Err(PtError::Scene(format!(
                    "a {width} × {height} equirect sky is over the tracer's {} texels: downsample it",
                    crate::env_map::MAX_EQUIRECT_TEXELS
                )));
            }
            if !(rotation.is_finite() && scale.is_finite() && *scale >= 0.0) {
                return Err(PtError::Scene(
                    "equirect sky rotation and scale must be finite".into(),
                ));
            }
            // Mode 3: the table lookup through `equirect_uv`, the yaw as (cos, sin).
            sky = [0.0, 0.0, 0.0, 3.0];
            sky_scale = *scale;
            sky_offs[0] = env.len() as u32;
            for t in texels {
                env.extend_from_slice(t);
            }
            sky_dims[0] = *width;
            sky_dims[1] = *height;
            let (sin_r, cos_r) = rotation.sin_cos();
            sky_sun[0] = cos_r;
            sky_sun[1] = sin_r;
            // The CDF follows the map's resolution (within bounds), sub-sampled when the map
            // is finer than the grid.
            let cdf = (
                (*width).clamp(EQUIRECT_CDF_MIN.0, EQUIRECT_CDF_MAX.0),
                (*height).clamp(EQUIRECT_CDF_MIN.1, EQUIRECT_CDF_MAX.1),
            );
            let ss = width.div_ceil(cdf.0).max(height.div_ceil(cdf.1)).clamp(1, 4);
            let (w, h, r, s) = (*width, *height, *rotation, *scale);
            Some((
                cdf,
                sky_distribution_ss(cdf.0, cdf.1, ss, |d| {
                    let (u, v) = crate::env_map::equirect_uv(d.to_array(), r);
                    let c = table_sample(w, h, texels, u, v);
                    [c[0] * s, c[1] * s, c[2] * s]
                }),
            ))
        }
    };
    if let Some(((cw, ch), (marg, cond, func, integral))) = distribution {
        sky_dims[2] = cw;
        sky_dims[3] = ch;
        sky_offs[1] = env.len() as u32;
        env.extend_from_slice(&marg);
        sky_offs[2] = env.len() as u32;
        env.extend_from_slice(&cond);
        sky_offs[3] = env.len() as u32;
        env.extend_from_slice(&func);
        sky_sun[3] = integral;
    }

    // ---- Media and sky bodies ----
    let (media_count, media_offset) = pack_media(&scene.media, &mut env)?;
    let (body_count, body_offset) = pack_bodies(&scene.sky_bodies, &mut env)?;
    let (cloud_on, cloud_offset) = pack_clouds(scene.clouds.as_ref(), &mut env, &mut textures)?;

    // ---- Globals (camera, sun, sky, terrain) ----
    let cam = &scene.camera;
    if cam.width == 0 || cam.height == 0 {
        return Err(PtError::Scene("camera has a zero-sized image".into()));
    }
    if !(cam.fov_y > 0.0 && cam.fov_y < std::f32::consts::PI) {
        return Err(PtError::Scene(
            "camera vertical field of view must be in (0, pi) radians".into(),
        ));
    }
    let (fwd, right, up) = cam.basis();
    let mut flags = 0u32;
    let mut sun_dir = [0.0f32, 1.0, 0.0, 1.0];
    let mut sun_l = [0.0f32; 4];
    if let Some(sun) = &scene.sun {
        let toward = Vec3::from_array(sun.toward).normalize_or(Vec3::Y);
        flags |= FLAG_SUN;
        let e = [
            sun.color[0] * sun.illuminance,
            sun.color[1] * sun.illuminance,
            sun.color[2] * sun.illuminance,
        ];
        if sun.angular_radius >= MIN_DISC_RADIUS {
            let a = f64::from(sun.angular_radius);
            let omc = 2.0 * (0.5 * a).sin().powi(2);
            let proj = std::f64::consts::PI * a.sin().powi(2);
            let k = (1.0 / proj) as f32;
            sun_dir = [toward.x, toward.y, toward.z, a.cos().min(1.0 - 1e-12) as f32];
            sun_l = [e[0] * k, e[1] * k, e[2] * k, omc as f32];
        } else {
            sun_dir = [toward.x, toward.y, toward.z, 1.0];
            sun_l = [e[0], e[1], e[2], 0.0];
        }
    }
    // The terrain view albedo only applies with a terrain and a texture that exists.
    let view_albedo = match &scene.terrain_view_albedo {
        Some(v) if scene.terrain.is_some() => {
            if v.texture >= tex_count || v.normal.is_some_and(|n| n >= tex_count) {
                return Err(PtError::Scene(format!(
                    "terrain view albedo names texture {} / {:?} of {tex_count}",
                    v.texture, v.normal
                )));
            }
            Some(v)
        }
        _ => None,
    };
    let mut terrain0 = [0.0f32; 4];
    let mut terrain1 = [0u32; 4];
    let mut terrain_lv = [[0u32; 4]; 4];
    if let Some((offsets, t)) = terrain_info {
        flags |= FLAG_TERRAIN;
        terrain0 = [t.origin[0], t.origin[1], t.spacing[0], t.spacing[1]];
        terrain1 = [t.width, t.depth, offsets.len() as u32, t.material as u32];
        for (i, o) in offsets.iter().enumerate() {
            terrain_lv[i / 4][i % 4] = *o;
        }
    }
    let globals = Globals {
        cam_pos: [cam.eye[0], cam.eye[1], cam.eye[2], (0.5 * cam.fov_y).tan()],
        cam_fwd: [fwd.x, fwd.y, fwd.z, cam.width as f32 / cam.height as f32],
        cam_right: [right.x, right.y, right.z, cam.aperture_radius.max(0.0)],
        cam_up: [up.x, up.y, up.z, cam.focus_distance.max(1e-3)],
        dims: [cam.width, cam.height, 1, 0],
        counts: [gpu_lights.len() as u32, tlas_root, 3, flags],
        sun_dir,
        sun_l,
        sky,
        sky_dims,
        sky_offs,
        sky_sun,
        dfg: [dfg_offset, lut.width, scene.brdf_energy.shader_code(), 0],
        terrain0,
        terrain1,
        terrain_lv,
        misc: [0.0, sky_scale, 0.0, 0.0],
        media: [media_count, media_offset, cloud_on, cloud_offset],
        media_f: [scene.media.escape_distance.max(0.0), 0.0, 0.0, 0.0],
        bodies: [body_count, body_offset, 0, 0],
        // The raster's pixel angle (`outdoor.rs` `pixel_angle`): the star footprint.
        stars: [
            scene.stars.max(0.0),
            cam.fov_y / cam.height.max(1) as f32,
            0.0,
            0.0,
        ],
        view_albedo_vp: view_albedo.map_or([[0.0; 4]; 4], |v| v.view_proj),
        view_albedo: [
            view_albedo.map_or(NONE, |v| v.texture as u32),
            view_albedo.and_then(|v| v.normal).map_or(NONE, |n| n as u32),
            u32::from(view_albedo.is_some_and(|v| v.all_bounces)),
            0,
        ],
        view_albedo_eye: view_albedo.map_or([0.0; 4], |v| [v.eye[0], v.eye[1], v.eye[2], 0.0]),
        probe: [0; 4],
        probe_f: [0.0; 4],
        extra_sun: extra_sun_rows(&scene.extra_suns),
    };
    // ---- Decals (ADR-2951): their words, then the trailer at the very end (`decal::push_trailer`) ----
    let decals = !scene.decals.is_empty();
    if decals {
        let first = textures.len() as u32;
        let packed = crate::decal::pack_decals(&scene.decals, tex_count);
        // The kernel walks `count` decals from `first`. A dropped decal (a zero rotation,
        // a non-finite field) is not in `packed`, so the count is the packed length.
        let count = (packed.len() / crate::decal::DECAL_WORDS) as u32;
        textures.extend(packed);
        crate::decal::push_trailer(&mut textures, first, count);
    }
    Ok(Packed {
        globals,
        nodes,
        tri_pos,
        tri_attr,
        instances,
        materials,
        textures,
        lights: gpu_lights,
        env,
        terrain,
        triangle_count,
        graphs,
        decals,
    })
}

/// `env` words per medium layer (`MEDIUM_WORDS` in the shader).
const MEDIUM_WORDS: usize = 16;
/// `env` words per sky-body header (`BODY_WORDS` in the shader).
const BODY_WORDS: usize = 16;

/// Medium layers into `env`: scattering rgb + phase kind, absorption rgb + falloff, HG g rgb +
/// aerial flag, reference/min/max height. Returns (count, offset).
fn pack_media(media: &crate::scene::PtMedia, env: &mut Vec<f32>) -> Result<(u32, u32), PtError> {
    use crate::scene::PtPhase;
    let layers: Vec<_> = media
        .layers
        .iter()
        .filter(|l| l.max_height > l.min_height)
        .filter(|l| l.scattering.iter().chain(l.absorption.iter()).any(|v| *v > 0.0))
        .collect();
    if layers.len() > MAX_MEDIUM_LAYERS {
        return Err(PtError::Scene(format!(
            "{} medium layers; the tracer reads at most {MAX_MEDIUM_LAYERS}",
            layers.len()
        )));
    }
    let offset = env.len() as u32;
    for l in &layers {
        if l.scattering
            .iter()
            .chain(l.absorption.iter())
            .any(|v| !v.is_finite() || *v < 0.0)
            || !l.falloff.is_finite()
            || l.falloff < 0.0
        {
            return Err(PtError::Scene(
                "medium coefficients must be finite and >= 0".into(),
            ));
        }
        if !l.reference_height.is_finite() {
            return Err(PtError::Scene("medium reference height must be finite".into()));
        }
        if let PtPhase::HenyeyGreenstein(g) = l.phase
            && g.iter().any(|v| !v.is_finite())
        {
            return Err(PtError::Scene("medium asymmetry must be finite".into()));
        }
        let (kind, g) = match l.phase {
            PtPhase::Rayleigh => (1.0, [0.0; 3]),
            PtPhase::HenyeyGreenstein(g) => (0.0, g.map(|v| v.clamp(-0.99, 0.99))),
        };
        let words: [f32; MEDIUM_WORDS] = [
            l.scattering[0],
            l.scattering[1],
            l.scattering[2],
            kind,
            l.absorption[0],
            l.absorption[1],
            l.absorption[2],
            l.falloff,
            g[0],
            g[1],
            g[2],
            if l.aerial { 1.0 } else { 0.0 },
            l.reference_height,
            l.min_height.max(-1e9),
            l.max_height.min(1e9),
            0.0,
        ];
        env.extend_from_slice(&words);
    }
    Ok((layers.len() as u32, offset))
}

/// `env` words of the cloud field (`CLOUD_*` word indices in the shader).
const CLOUD_WORDS: usize = 28;

/// The cloud field into `env` and its texels into `textures` (appended after the scene's own, so
/// no texture moves). Returns (1 when present, `env` offset).
///
/// Words: base y, top y, extinction, coverage; drift x, z, weather tile, weather texels; shape
/// tile, detail tile, erosion, breakup; breakup tile, curl swirl (m), curl tile, planet radius;
/// albedo, first g, second g, first weight; majorant, uniform density (< 0: noise), then the
/// `textures` offsets of the weather map, shape, detail and curl volumes as raw bits, and the two
/// noise edge sizes.
fn pack_clouds(
    field: Option<&crate::scene::PtCloudField>,
    env: &mut Vec<f32>,
    textures: &mut Vec<u32>,
) -> Result<(u32, u32), PtError> {
    use crate::scene::PtCloudDensity;
    let Some(field) = field else {
        return Ok((0, 0));
    };
    let s = &field.settings;
    let finite = [
        s.altitude_bottom,
        s.altitude_top,
        s.density,
        s.coverage,
        s.domain_m,
        s.shape_scale_m,
        s.detail_scale_m,
        field.planet_radius_m,
        field.albedo,
        field.phase.forward_g,
        field.phase.back_g,
        field.phase.forward_weight,
    ];
    if finite.iter().any(|v| !v.is_finite()) {
        return Err(PtError::Scene("cloud field values must be finite".into()));
    }
    if s.density < 0.0 || s.altitude_top <= s.altitude_bottom || field.planet_radius_m < 1000.0 {
        return Err(PtError::Scene(
            "cloud field needs density >= 0, top > base and a planet radius of at least 1 km".into(),
        ));
    }
    let look = s.look.sanitized();
    let weather = regia_render::build_weather_map(s);
    let noise = regia_render::cloud_noise();
    let curl = regia_render::cloud_curl();
    let (uniform, majorant) = match field.density {
        PtCloudDensity::Noise => (-1.0, regia_render::cloud_density_bound(s, &weather)),
        PtCloudDensity::Uniform(d) => {
            if !d.is_finite() {
                return Err(PtError::Scene(
                    "cloud field uniform density must be finite".into(),
                ));
            }
            let d = d.clamp(0.0, 1.0);
            (d, d)
        }
    };
    let mut volume = |rgba: &[u8]| -> f32 {
        let at = textures.len() as u32;
        textures.extend(
            rgba.as_chunks::<4>()
                .0
                .iter()
                .map(|p| u32::from_le_bytes([p[0], p[1], p[2], p[3]])),
        );
        f32::from_bits(at)
    };
    let weather_at = volume(&weather.rgba);
    let shape_at = volume(&noise.shape);
    let detail_at = volume(&noise.detail);
    let curl_at = volume(curl);
    let drift = s.drift_m();
    let (swirl, curl_tile) = if field.curl {
        (
            regia_render::CLOUD_CURL_STRENGTH * s.detail_scale_m.max(1.0),
            regia_render::CLOUD_CURL_TILE * s.detail_scale_m.max(1.0),
        )
    } else {
        (0.0, 1.0)
    };
    let words: [f32; CLOUD_WORDS] = [
        s.altitude_bottom,
        s.altitude_top,
        s.density,
        s.coverage.clamp(0.0, 1.0),
        drift[0],
        drift[1],
        s.domain_m.max(1.0),
        weather.resolution as f32,
        s.shape_scale_m.max(1.0),
        s.detail_scale_m.max(1.0),
        look.erosion,
        look.breakup,
        look.breakup_scale_m,
        swirl,
        curl_tile,
        field.planet_radius_m,
        field.albedo.clamp(0.0, 1.0),
        field.phase.forward_g.clamp(-0.99, 0.99),
        field.phase.back_g.clamp(-0.99, 0.99),
        field.phase.forward_weight.clamp(0.0, 1.0),
        majorant,
        uniform,
        weather_at,
        shape_at,
        detail_at,
        curl_at,
        regia_render::SHAPE_NOISE_SIZE as f32,
        regia_render::CURL_SIZE as f32,
    ];
    let offset = env.len() as u32;
    env.extend_from_slice(&words);
    Ok((1, offset))
}

/// Smallest angular radius, radians, packed as a disc. A smaller sun is packed as a delta light
/// (same illuminance) and a smaller body is dropped: `1 / (π sin² α)` and the cone pdf
/// `1 / (2π(1 − cos α))` overflow `f32` for tiny radii and turn the NEE weights into NaN.
const MIN_DISC_RADIUS: f32 = 1e-4;

/// Sky bodies into `env`: a 16-word header (centre + cos r, e1 + tan r, e2 + size, data offset
/// bits, light scale, cone pdf) per body, then each body's radiance table. Returns (count,
/// header offset).
fn pack_bodies(bodies: &[crate::scene::PtSkyBody], env: &mut Vec<f32>) -> Result<(u32, u32), PtError> {
    let bodies: Vec<_> = bodies
        .iter()
        .filter(|b| b.angular_radius >= MIN_DISC_RADIUS && b.size > 0)
        .collect();
    if bodies.len() > MAX_SKY_BODIES {
        return Err(PtError::Scene(format!(
            "{} sky bodies; the tracer reads at most {MAX_SKY_BODIES}",
            bodies.len()
        )));
    }
    let header = env.len();
    env.resize(header + bodies.len() * BODY_WORDS, 0.0);
    for (i, b) in bodies.iter().enumerate() {
        let n = b.size as usize;
        if b.radiance.len() != n * n || b.angular_radius >= 1.5 {
            return Err(PtError::Scene(format!(
                "sky body {i}: {} texels for a {n}² table, radius {}",
                b.radiance.len(),
                b.angular_radius
            )));
        }
        let (c, e1, e2) = b.frame();
        let data = env.len() as u32;
        for t in &b.radiance {
            env.extend_from_slice(t);
        }
        let cos_r = f64::from(b.angular_radius).cos();
        let omc = 2.0 * (0.5 * f64::from(b.angular_radius)).sin().powi(2);
        let words: [f32; BODY_WORDS] = [
            c.x,
            c.y,
            c.z,
            cos_r as f32,
            e1.x,
            e1.y,
            e1.z,
            b.angular_radius.tan(),
            e2.x,
            e2.y,
            e2.z,
            b.size as f32,
            f32::from_bits(data),
            b.light_scale.max(0.0),
            (1.0 / (std::f64::consts::TAU * omc)) as f32,
            omc as f32,
        ];
        let at = header + i * BODY_WORDS;
        env[at..at + BODY_WORDS].copy_from_slice(&words);
    }
    Ok((bodies.len() as u32, header as u32))
}

/// A copy of `scene` with every [`PtLight::Quad`] turned into a two-triangle emitter mesh
/// (black, rough, emissive = the radiance) and removed from the analytic lights.
pub(crate) fn quads_as_meshes(scene: &PtScene) -> PtScene {
    let mut out = scene.clone();
    out.lights.clear();
    // Profiles name lights by index; the quads leave the list, so the others move up.
    let mut moved: Vec<Option<usize>> = Vec::with_capacity(scene.lights.len());
    for l in &scene.lights {
        moved.push((!matches!(l, PtLight::Quad { .. })).then_some(moved.iter().flatten().count()));
    }
    out.light_profiles
        .retain_mut(|p| match moved.get(p.light).copied().flatten() {
            Some(i) => {
                p.light = i;
                true
            }
            None => false,
        });
    for l in &scene.lights {
        let PtLight::Quad {
            corner,
            edge_u,
            edge_v,
            radiance,
            two_sided,
        } = *l
        else {
            out.lights.push(*l);
            continue;
        };
        let c = Vec3::from_array(corner);
        let (u, v) = (Vec3::from_array(edge_u), Vec3::from_array(edge_v));
        let n = u.cross(v).normalize_or(Vec3::Y).to_array();
        let p = [c, c + u, c + u + v, c + v].map(|q| q.to_array());
        let mesh = out.add_mesh(crate::scene::PtMesh {
            positions: p.to_vec(),
            normals: vec![n; 4],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            colors: Vec::new(),
            tangents: Vec::new(),
            // Counter-clockwise about `u × v`: the winding normal is the emitting face.
            indices: vec![0, 1, 2, 0, 2, 3],
        });
        let material = out.add_material(crate::scene::PtMaterial {
            one_sided_emission: !two_sided,
            ..crate::scene::PtMaterial::emitter(radiance)
        });
        out.add_instance(mesh, material, Mat4::IDENTITY);
    }
    out
}

/// Contract rows of the packed scene's structs (`regia_render::ContractRow`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<regia_render::ContractRow> {
    use regia_render::{ContractRow, contract_row};
    use std::mem::offset_of;
    let w2o = offset_of!(GpuInstance, w2o);
    let o2w = offset_of!(GpuInstance, o2w);
    vec![
        contract_row!(
            "pt.wgsl",
            "Globals",
            Globals {
                cam_pos,
                cam_fwd,
                cam_right,
                cam_up,
                dims,
                counts,
                sun_dir,
                sun_l,
                sky,
                sky_dims,
                sky_offs,
                sky_sun,
                dfg,
                terrain0,
                terrain1,
                terrain_lv,
                misc,
                media,
                media_f,
                bodies,
                stars,
                view_albedo_vp,
                view_albedo,
                view_albedo_eye,
                probe,
                probe_f,
                extra_sun
            }
        ),
        ContractRow::new(
            "pt.wgsl",
            "Instance",
            size_of::<GpuInstance>(),
            vec![
                ("w2o0", w2o),
                ("w2o1", w2o + 16),
                ("w2o2", w2o + 32),
                ("o2w0", o2w),
                ("o2w1", o2w + 16),
                ("o2w2", o2w + 32),
                ("info", offset_of!(GpuInstance, info)),
                ("info2", offset_of!(GpuInstance, info2)),
            ],
        ),
        contract_row!(
            "pt.wgsl",
            "Material",
            GpuMaterial {
                base_color,
                emissive,
                params,
                coat,
                tex,
                extra,
                detail,
                detail2,
                tex2,
                transmit,
                volume,
                film,
                graph,
                sss,
                sss_tint
            }
        ),
        contract_row!(
            "pt.wgsl",
            "Light",
            GpuLight {
                pos,
                dir,
                color,
                extra,
                refs
            }
        ),
        contract_row!("pt.wgsl", "Node", crate::bvh::BvhNode { bmin => min, a, bmax => max, count }),
        contract_row!(
            "lightmap_bake.wgsl",
            "Node",
            crate::bvh::BvhNode { min, a, max, count }
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{PtCamera, PtMaterial, PtMesh, PtSkyBody};

    fn camera() -> PtCamera {
        PtCamera::pinhole([0.0, 0.0, 5.0], [0.0; 3], 0.8, 8, 8)
    }

    fn quad() -> PtMesh {
        PtMesh {
            positions: vec![
                [-1.0, 0.0, -1.0],
                [1.0, 0.0, -1.0],
                [1.0, 0.0, 1.0],
                [-1.0, 0.0, 1.0],
            ],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            colors: Vec::new(),
            tangents: Vec::new(),
            indices: vec![0, 1, 2, 0, 2, 3],
        }
    }

    /// A zero rotation and a NaN centre are not decals the tracer can place. The trailer
    /// count is the number of words actually packed: `scene.decals.len()` would walk the
    /// kernel past those words into the trailer.
    #[test]
    fn a_non_finite_decal_does_not_count_in_the_trailer() {
        use regia_core::glam::{Quat, Vec3};
        let mut scene = PtScene::new(camera());
        let mut zero = crate::PtDecal::from_box(Vec3::ZERO, Quat::IDENTITY, Vec3::ONE, [1.0, 0.0, 0.0, 1.0]);
        zero.rotation = [0.0; 4];
        let mut nan_center =
            crate::PtDecal::from_box(Vec3::ZERO, Quat::IDENTITY, Vec3::ONE, [0.0, 1.0, 0.0, 1.0]);
        nan_center.center = [f32::NAN, 0.0, 0.0];
        let good = crate::PtDecal::from_box(
            Vec3::new(2.0, 0.0, 0.0),
            Quat::IDENTITY,
            Vec3::ONE,
            [0.0, 0.0, 1.0, 1.0],
        );
        scene.decals = vec![zero, nan_center, good];
        let packed = pack(&scene).expect("pack");
        let words = &packed.textures;
        let n = words.len();
        assert!(n >= 4, "no trailer");
        assert_eq!(words[n - 2], crate::decal::TRAILER_MAGIC);
        let count = words[n - 3];
        assert_eq!(count, 1, "trailer count {count}");
        let first = words[n - 4] as usize;
        let decal = &words[first..first + crate::decal::DECAL_WORDS];
        assert!(
            decal[..32].iter().all(|w| f32::from_bits(*w).is_finite()),
            "packed a non-finite decal"
        );
        assert_eq!(
            f32::from_bits(decal[0]),
            1.0,
            "the kept decal is the identity one"
        );
    }

    #[test]
    fn heightfield_sample_count_check_does_not_wrap() {
        // 65536 x 65536 wraps a u32 product to 0, which an empty buffer used to satisfy.
        let t = crate::scene::PtHeightfield {
            width: 65_536,
            depth: 65_536,
            origin: [0.0; 2],
            spacing: [1.0; 2],
            heights: Vec::new(),
            material: 0,
        };
        assert!(t.height_at(1.0, 1.0).is_none());
        let mut scene = PtScene::new(camera());
        scene.add_material(PtMaterial::diffuse([0.5; 3]));
        scene.terrain = Some(t);
        match pack(&scene) {
            Err(PtError::Scene(m)) => assert!(m.contains("wrong sample count"), "{m}"),
            other => panic!("expected a sample-count error, got {:?}", other.map(|_| ())),
        }
    }

    /// Member names of one WGSL struct, comments stripped, in source order.
    fn wgsl_struct_members(src: &str, name: &str) -> Vec<String> {
        let marker = format!("struct {name} {{");
        let start = src.find(&marker).unwrap_or_else(|| panic!("no struct {name}"));
        let bytes = src.as_bytes();
        let mut i = start + marker.len();
        let mut depth = 1i32;
        let mut body = String::new();
        while i < bytes.len() && depth > 0 {
            if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
            match bytes[i] {
                b'{' => {
                    depth += 1;
                    body.push('{');
                }
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    body.push('}');
                }
                c => body.push(c as char),
            }
            i += 1;
        }
        body.split(',')
            .filter_map(|part| {
                let name = part.split(':').next()?.split_whitespace().last()?;
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    .then(|| name.to_string())
            })
            .collect()
    }

    /// The Material row must name every WGSL member. `graph`, `sss` and `sss_tint` sit after
    /// `film` and are the same size as the fields the row already lists, so a size check passes
    /// while a reorder of those three is invisible to the ratchet (ADR-3002).
    #[test]
    fn material_contract_row_names_every_wgsl_member() {
        use std::mem::offset_of;
        let members = wgsl_struct_members(include_str!("../shaders/pt.wgsl"), "Material");
        let row = contract_rows()
            .into_iter()
            .find(|r| r.shader == "pt.wgsl" && r.wgsl_struct == "Material")
            .expect("Material row");
        assert!(!row.partial, "a partial row hides an unlisted member");
        let listed: Vec<&str> = row.members.iter().map(|(n, _)| *n).collect();
        for name in &members {
            assert!(
                listed.contains(&name.as_str()),
                "pt.wgsl::Material member `{name}` has no Rust field in the row; listed {listed:?}"
            );
        }
        for (name, offset) in [
            ("graph", offset_of!(GpuMaterial, graph)),
            ("sss", offset_of!(GpuMaterial, sss)),
            ("sss_tint", offset_of!(GpuMaterial, sss_tint)),
        ] {
            assert_eq!(
                row.members.iter().find(|(m, _)| *m == name).map(|(_, o)| *o),
                Some(offset),
                "{name}"
            );
        }
    }

    #[test]
    fn struct_sizes_match_the_shader() {
        assert_eq!(std::mem::size_of::<GpuInstance>(), 128);
        assert_eq!(std::mem::size_of::<GpuMaterial>(), 240);
        assert_eq!(std::mem::size_of::<GpuLight>(), 80);
        // + `probe`, `probe_f` (probe mode), + the six `extra_sun` rows (ADR-7254).
        assert_eq!(std::mem::size_of::<Globals>(), 38 * 16);
    }

    #[test]
    fn emissive_instances_become_power_weighted_lights() {
        let mut scene = PtScene::new(camera());
        let mesh = scene.add_mesh(quad());
        let lit = scene.add_material(PtMaterial::emitter([10.0; 3]));
        let dull = scene.add_material(PtMaterial::diffuse([0.5; 3]));
        scene.add_instance(mesh, lit, Mat4::IDENTITY);
        scene.add_instance(mesh, dull, Mat4::from_translation(Vec3::new(0.0, 3.0, 0.0)));
        scene.add_instance(mesh, lit, Mat4::from_scale(Vec3::splat(2.0)));
        let p = pack(&scene).expect("pack");
        assert_eq!(p.lights.len(), 4, "two triangles per emissive quad");
        let pdf_sum: f32 = p.lights.iter().map(|l| l.color[3]).sum();
        assert!((pdf_sum - 1.0).abs() < 1e-5);
        // The scaled quad has 4× the area, so its triangles carry 4× the selection weight.
        let mut pdfs: Vec<f32> = p.lights.iter().map(|l| l.color[3]).collect();
        pdfs.sort_by(f32::total_cmp);
        assert!((pdfs[3] / pdfs[0] - 4.0).abs() < 1e-3);
        // Each emissive instance points at its first light.
        let bases: Vec<u32> = p.instances.iter().map(|i| i.info2[0]).collect();
        assert_eq!(bases.iter().filter(|&&b| b != NONE).count(), 2);
    }

    #[test]
    fn oversized_empty_texture_is_rejected() {
        let mut scene = PtScene::new(camera());
        scene.textures.push(crate::scene::PtTexture {
            width: 1 << 31,
            height: 1 << 31,
            rgba: std::sync::Arc::from(Vec::new()),
            srgb: false,
        });
        assert!(pack(&scene).is_err());
    }

    #[test]
    fn sky_distribution_is_a_cdf() {
        let (marg, cond, func, integral) = sky_distribution(16, 8, |d| [d.y.max(0.0) * 4.0 + 0.1; 3]);
        assert_eq!(marg.len(), 9);
        assert_eq!(cond.len(), 8 * 17);
        assert_eq!(func.len(), 16 * 8);
        assert!(integral > 0.0);
        assert!(marg.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(marg[8], 1.0);
        // Brighter upper rows get more of the marginal mass.
        assert!(marg[4] > 0.5);
    }

    #[test]
    fn pyramid_bounds_every_level() {
        let heights: Vec<f32> = (0..(9 * 5)).map(|i| ((i * 37) % 11) as f32).collect();
        let t = PtHeightfield {
            width: 9,
            depth: 5,
            origin: [0.0, 0.0],
            spacing: [1.0, 1.0],
            heights,
            material: 0,
        };
        let (data, offsets) = heightfield_pyramid(&t, 0);
        // 8x4 → 4x2 → 2x1 → 1x1.
        assert_eq!(offsets.len(), 4);
        let top = offsets[3] as usize;
        let lo = t.heights.iter().copied().fold(f32::INFINITY, f32::min);
        let hi = t.heights.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        assert_eq!(data[top], lo);
        assert_eq!(data[top + 1], hi);
    }

    #[test]
    fn cut_cells_bound_nothing() {
        // 5 x 5 samples, the centre sample cut: its four cells are empty.
        let mut heights = vec![1.0f32; 25];
        heights[12] = f32::NAN;
        heights[0] = 7.0;
        let t = PtHeightfield {
            width: 5,
            depth: 5,
            origin: [0.0, 0.0],
            spacing: [1.0, 1.0],
            heights,
            material: 0,
        };
        let (data, offsets) = heightfield_pyramid(&t, 0);
        let cell = |level: usize, i: usize, j: usize, w: usize| {
            let k = offsets[level] as usize + 2 * (j * w + i);
            (data[k], data[k + 1])
        };
        for (i, j) in [(1, 1), (2, 1), (1, 2), (2, 2)] {
            assert_eq!(cell(0, i, j, 4), (CUT_HEIGHT, CUT_HEIGHT), "cell ({i}, {j})");
        }
        assert_eq!(cell(0, 0, 0, 4), (1.0, 7.0));
        // The parents ignore the cut cells: the top box is the uncut range.
        let top = offsets[offsets.len() - 1] as usize;
        assert_eq!((data[top], data[top + 1]), (1.0, 7.0));
        assert!(data.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn cached_blas_survive_a_moved_camera_and_moved_instances() {
        let mut scene = PtScene::new(camera());
        let mesh = scene.add_mesh(crate::scenes::uv_sphere(1.0, 12, 8, false));
        let floor = scene.add_mesh(quad());
        let mat = scene.add_material(PtMaterial::diffuse([0.5; 3]));
        scene.add_instance(mesh, mat, Mat4::IDENTITY);
        scene.add_instance(floor, mat, Mat4::IDENTITY);
        let plain = pack(&scene).expect("pack");
        let mut cache = PtCache::new();
        let first = pack_with(&scene, Some(&mut cache)).expect("pack");
        assert_eq!((cache.blas_built(), cache.blas_reused()), (2, 0));
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&first.nodes),
            bytemuck::cast_slice::<_, u8>(&plain.nodes)
        );
        // Bitwise: packed colours and indices are NaN-patterned floats.
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&first.tri_pos),
            bytemuck::cast_slice::<_, u8>(&plain.tri_pos)
        );
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&first.tri_attr),
            bytemuck::cast_slice::<_, u8>(&plain.tri_attr)
        );
        // Moved camera, moved instance, one more instance of the same mesh: no BLAS rebuilt.
        scene.camera.eye = [3.0, 2.0, 4.0];
        scene.instances[0].transform = Mat4::from_translation(Vec3::new(2.0, 0.5, 0.0));
        scene.add_instance(mesh, mat, Mat4::from_translation(Vec3::new(-2.0, 0.5, 0.0)));
        let second = pack_with(&scene, Some(&mut cache)).expect("pack");
        assert_eq!((cache.blas_built(), cache.blas_reused()), (2, 2));
        let uncached = pack(&scene).expect("pack");
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&second.nodes),
            bytemuck::cast_slice::<_, u8>(&uncached.nodes)
        );
        assert_eq!(second.instances.len(), 3);
    }

    /// IOR 1.0 (F0 = 0) must not pack as exactly 0: the shader reads 0 as "unset" and would
    /// substitute 0.04 (and rebuild IOR 1.5 for rough transmission).
    #[test]
    fn index_matched_dielectric_packs_a_nonzero_f0() {
        let mut scene = PtScene::new(camera());
        let mesh = scene.add_mesh(quad());
        let m = scene.add_material(PtMaterial {
            ior: 1.0,
            transmission: 0.5,
            ..PtMaterial::default()
        });
        scene.add_instance(mesh, m, Mat4::IDENTITY);
        let p = pack(&scene).expect("pack");
        let f0 = p.materials[m].transmit[1];
        assert!(f0 > 0.0 && f0 < 1.0e-5, "{f0}");
    }

    /// A subsurface channel without reach (d = 0) keeps its diffuse light at the hit: pack sets
    /// its tint to 0 (the kernel's stay branch keeps `1 - tint`, and its scattered share would be
    /// lost with a profile of 0). The channels with reach keep their tint.
    #[test]
    fn subsurface_channel_without_reach_keeps_its_light() {
        let mut scene = PtScene::new(camera());
        let mesh = scene.add_mesh(quad());
        let m = scene.add_material(PtMaterial::default());
        scene.add_instance(mesh, m, Mat4::IDENTITY);
        scene.subsurface.push(crate::scene::PtSubsurface {
            material: m,
            d: [0.01, 0.004, 0.0],
            tint: [1.0, 0.5, 1.0],
        });
        let p = pack(&scene).expect("pack");
        assert_eq!(p.materials[m].sss, [0.01, 0.004, 0.0, 1.0]);
        assert_eq!(p.materials[m].sss_tint[..3], [1.0, 0.5, 0.0]);
        // No channel with reach: no subsurface at all.
        scene.subsurface[0].d = [0.0; 3];
        let p = pack(&scene).expect("pack");
        assert_eq!(p.materials[m].sss[3], 0.0);
    }

    /// The kernel picks a radius only among the channels with reach and averages the radius pdf
    /// over that count, so a d = 0 channel adds no radius-0 delta the mixture pdf leaves out.
    #[test]
    fn the_subsurface_pick_skips_channels_without_reach() {
        let source = include_str!("../shaders/pt.wgsl");
        assert!(source.contains("var pick = rand() * channels;"), "channel pick");
        assert!(
            source.contains("burley_radius_pdf(dd.z, r)) / channels;"),
            "the radius pdf averages over the live channels"
        );
        assert!(!source.contains("burley_radius_pdf(dd.z, r)) / 3.0;"));
    }

    #[test]
    fn transmissive_instances_are_not_alpha_tested() {
        let mut scene = PtScene::new(camera());
        let mesh = scene.add_mesh(quad());
        let glass = scene.add_material(PtMaterial {
            transmission: 1.0,
            ior: 1.33,
            alpha_mode: 3,
            ..PtMaterial::default()
        });
        let leaf = scene.add_material(PtMaterial {
            alpha_mode: 1,
            ..PtMaterial::default()
        });
        scene.add_instance(mesh, glass, Mat4::IDENTITY);
        scene.add_instance(mesh, leaf, Mat4::from_translation(Vec3::Y));
        let p = pack(&scene).expect("pack");
        let mut flags: Vec<u32> = p.instances.iter().map(|i| i.info2[1]).collect();
        flags.sort_unstable();
        assert_eq!(flags, vec![INST_ALPHA, INST_TRANSMIT]);
        assert_eq!(p.materials[glass].transmit[0], 1.0);
        assert!((p.materials[glass].transmit[1] - (0.33f32 / 2.33).powi(2)).abs() < 1e-6);
        assert!(
            (p.materials[leaf].transmit[1] - 0.04).abs() < 1e-6,
            "glTF default IOR"
        );
    }

    #[test]
    fn stars_reach_the_globals() {
        let mut scene = PtScene::new(camera());
        scene.stars = 12.0;
        let p = pack(&scene).expect("pack");
        assert_eq!(p.globals.stars[0], 12.0);
        let cam = camera();
        assert!((p.globals.stars[1] - cam.fov_y / cam.height as f32).abs() < 1e-7);
    }

    #[test]
    fn rejects_bad_meshes() {
        let mut scene = PtScene::new(camera());
        let mut m = quad();
        m.indices.push(99);
        m.indices.push(0);
        m.indices.push(1);
        let mesh = scene.add_mesh(m);
        let mat = scene.add_material(PtMaterial::default());
        scene.add_instance(mesh, mat, Mat4::IDENTITY);
        assert!(pack(&scene).is_err());
    }

    #[test]
    fn terrain_with_degenerate_spacing_is_rejected() {
        for spacing in [[0.0, 1.0], [1.0, -1.0], [f32::NAN, 1.0], [1.0, f32::INFINITY]] {
            let mut scene = PtScene::new(camera());
            let material = scene.add_material(PtMaterial::diffuse([0.5; 3]));
            scene.terrain = Some(crate::scene::PtHeightfield {
                width: 2,
                depth: 2,
                origin: [0.0, 0.0],
                spacing,
                heights: vec![0.0; 4],
                material,
            });
            assert!(pack(&scene).is_err(), "{spacing:?}");
        }
    }

    fn fog_layer() -> crate::scene::PtMediumLayer {
        crate::scene::PtMediumLayer {
            scattering: [0.01; 3],
            absorption: [0.0; 3],
            reference_height: 0.0,
            falloff: 0.04,
            min_height: -100.0,
            max_height: 1000.0,
            phase: crate::scene::PtPhase::HenyeyGreenstein([0.3; 3]),
            aerial: false,
        }
    }

    #[test]
    fn rejects_non_finite_medium_asymmetry_and_reference_height() {
        let mut scene = PtScene::new(camera());
        scene.media.layers.push(fog_layer());
        assert!(pack(&scene).is_ok());

        let mut bad_g = scene.clone();
        bad_g.media.layers[0].phase = crate::scene::PtPhase::HenyeyGreenstein([0.3, f32::NAN, 0.3]);
        assert!(pack(&bad_g).is_err());

        let mut bad_ref = scene.clone();
        bad_ref.media.layers[0].reference_height = f32::NAN;
        assert!(pack(&bad_ref).is_err());
    }

    #[test]
    fn rejects_a_field_of_view_outside_zero_to_pi() {
        for fov in [0.0, -0.5, std::f32::consts::PI, 4.0, f32::NAN] {
            let mut scene = PtScene::new(camera());
            scene.camera.fov_y = fov;
            assert!(pack(&scene).is_err(), "fov {fov}");
        }
        let mut ok = PtScene::new(camera());
        ok.camera.fov_y = 3.0;
        assert!(pack(&ok).is_ok());
    }

    #[test]
    fn a_tiny_sun_disc_packs_as_a_delta_light_with_finite_radiance() {
        let mut scene = PtScene::new(camera());
        scene.sun = Some(crate::scene::PtSun {
            toward: [0.0, 1.0, 0.0],
            color: [1.0; 3],
            illuminance: 1000.0,
            angular_radius: 1e-9,
        });
        let p = pack(&scene).expect("pack");
        assert!(
            p.globals.sun_l.iter().all(|v| v.is_finite()),
            "{:?}",
            p.globals.sun_l
        );
        assert_eq!(p.globals.sun_l[3], 0.0, "delta: no cone");
        assert_eq!(p.globals.sun_l[0], 1000.0);
    }

    #[test]
    fn a_tiny_sky_body_is_dropped_instead_of_packing_an_infinite_cone_pdf() {
        let mut scene = PtScene::new(camera());
        scene
            .sky_bodies
            .push(PtSkyBody::uniform([0.0, 1.0, 0.0], 1e-12, [1.0; 3]));
        let p = pack(&scene).expect("pack");
        assert_eq!(p.globals.bodies[0], 0);
    }

    /// ADR-862: the meshes' BLASes build in parallel, ahead of the mesh loop. The packed scene
    /// is the one a one-mesh-at-a-time build gives (`build_blas` in mesh order), the cache
    /// counts as before (duplicates and empty meshes included), and the first bad mesh in
    /// order is the error.
    #[test]
    fn parallel_blas_builds_pack_what_a_sequential_build_packs() {
        let mut scene = PtScene::new(camera());
        let mat = scene.add_material(PtMaterial::diffuse([0.5; 3]));
        for k in 0..24u32 {
            let mesh = match k % 4 {
                0 => crate::scenes::uv_sphere(1.0 + k as f32 * 0.1, 8 + k, 6, false),
                1 => quad(),
                2 => PtMesh::default(),
                _ => crate::scenes::uv_sphere(1.0, 12, 8, false),
            };
            let m = scene.add_mesh(mesh);
            scene.add_instance(
                m,
                mat,
                Mat4::from_translation(Vec3::new(k as f32 * 3.0, 0.0, 0.0)),
            );
        }
        // The reference: every mesh built in order, concatenated as `pack_with` does.
        let mut nodes: Vec<BvhNode> = Vec::new();
        let mut tri_pos: Vec<[f32; 4]> = Vec::new();
        for mesh in &scene.meshes {
            if let Some(b) = build_blas(mesh).expect("build") {
                let first_tri = (tri_pos.len() / 3) as u32;
                let root = nodes.len() as u32;
                nodes.extend(b.nodes.iter().map(|n| {
                    let mut n = *n;
                    n.a += if n.is_leaf() { first_tri } else { root };
                    n
                }));
                tri_pos.extend_from_slice(&b.tri_pos);
            }
        }
        let plain = pack(&scene).expect("pack");
        assert_eq!(&plain.nodes[..nodes.len()], &nodes[..]);
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&plain.tri_pos),
            bytemuck::cast_slice::<_, u8>(&tri_pos)
        );
        // Through a cache: 6 distinct spheres (k % 4 == 0), one quad, one 12×8 sphere, and the
        // 6 empty meshes count as built each time (an empty mesh is not cached).
        let mut cache = PtCache::new();
        let cached = pack_with(&scene, Some(&mut cache)).expect("pack");
        assert_eq!((cache.blas_built(), cache.blas_reused()), (6 + 1 + 1 + 6, 5 + 5));
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&cached.nodes),
            bytemuck::cast_slice::<_, u8>(&plain.nodes)
        );
        let again = pack_with(&scene, Some(&mut cache)).expect("pack");
        assert_eq!((cache.blas_built(), cache.blas_reused()), (14 + 6, 10 + 18));
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&again.tri_attr),
            bytemuck::cast_slice::<_, u8>(&plain.tri_attr)
        );
        // A bad mesh fails the pack, with or without a cache.
        let mut bad = quad();
        bad.indices.extend_from_slice(&[99, 0, 1]);
        let m = scene.add_mesh(bad);
        scene.add_instance(m, mat, Mat4::IDENTITY);
        assert!(pack(&scene).is_err());
        assert!(pack_with(&scene, Some(&mut PtCache::new())).is_err());
    }
}
