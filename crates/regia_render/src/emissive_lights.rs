// SPDX-License-Identifier: MPL-2.0
//! Emissive meshes as local area lights (lane emissive-local, 2026-10-09).
//!
//! Emissive trim (unit lights, building lights, strips) lights the surfaces right next to it.
//! The probes and the screen-space GI carry only a blurred share of that (measured on DO27's
//! constructor_a52 against the path tracer: 25–44 % onto the ground, about 10 % onto the hull
//! next to a strip). This module fits the emissive triangles of a mesh with a few area lights
//! once per mesh (cached by the renderer), and the forward pass shades them in the clustered
//! light loop with linearly transformed cosines ([`crate::ltc`]): exact polygon irradiance for
//! the diffuse lobe, the fitted LTC for GGX.
//!
//! Fitting, per connected emissive component (welded positions):
//!
//! * a **tube** where the piece is thin in two directions (a rod, a bar): the axis and a radius
//!   whose lateral area equals the piece's emitting area. The shader turns it into the quad of
//!   its silhouette facing the receiver (a Lambertian cylinder seen side-on is that quad);
//! * one-sided **quads** (symmetric trapezoids, so sectors of a ring fit too) for the faces of a
//!   plate: the triangles facing `+n` and the ones facing `−n` each give a quad, and an open
//!   single sheet of a double-sided material gives a two-sided quad, as the path tracer emits;
//! * anything else (curved, chunky, a quad that covers its triangles poorly) splits in two at
//!   the area median along its long axis and recurses, up to [`FitParams::max_depth`].
//!
//! Each light keeps the piece's flux: a quad's radiance is scaled by `emitting area / quad
//! area`. The first time a mesh is drawn, each light traces a distance probe: 16 × 16
//! directions from ten points on a quad, or from five on a tube (its centre and four points
//! along the axis). The forward pass compares a receiver's distance with that map, so the
//! light stops at the first surface. Rays that stop on the emitter scale its radiance by the
//! open share, and a face with almost no open share is dropped. One-sided faces emit from
//! their front only.
//!
//! Under a baked GI volume that names its emitters (ADR-1386), the lights of an emitter the bake
//! traced carry [`BAKED_EMITTER_TAG`]: the forward pass leaves their light to the volume where it
//! covers, so the emitter is not counted twice.

use std::collections::HashMap;

use regia_asset::baked_gi::BakedEmitter;
use regia_core::glam::{Mat3, Mat4, Vec3};

use crate::lab_frame::{
    ALPHA_ADDITIVE, ALPHA_BLEND, ALPHA_PREMULTIPLIED, LabMaterial, LabTexture, LabVertex,
};

mod fit_queue;

use fit_queue::{ContentKey, FitQueue};
pub use fit_queue::{EMISSIVE_FRAME_BUDGET_MS, EmissiveFitStats};

/// How a fitted light emits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EmitterShape {
    /// A planar symmetric trapezoid `centre ± axis_x ± axis_y·(1 ± taper)` (`taper` along
    /// `axis_x`, in `(−1, 1)`; 0 = rectangle). One-sided quads emit toward `axis_x × axis_y`.
    Quad {
        /// Emits from both faces (an open sheet of a double-sided material).
        two_sided: bool,
        /// Width change along `axis_x`: the `−axis_x` end is `1 − taper`, the `+axis_x` end
        /// `1 + taper` times the mean width.
        taper: f32,
    },
    /// A Lambertian cylinder around `centre ± axis_x` (`axis_y` unused).
    Tube {
        /// Cylinder radius, metres.
        radius: f32,
    },
}

/// One area light fitted to emissive triangles.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EmissiveLight {
    /// Centre.
    pub centre: [f32; 3],
    /// Half extent along the first axis (a tube's half axis).
    pub axis_x: [f32; 3],
    /// Half extent along the second axis (zero for a tube).
    pub axis_y: [f32; 3],
    /// Linear RGB radiance, cd/m² (nits) per channel.
    pub radiance: [f32; 3],
    /// Shape.
    pub shape: EmitterShape,
    /// A point on the emitting triangles near the centre, as `(s, t)` in `[−1, 1]²`
    /// (`centre + axis_x·s + axis_y·t`): where the distance probe looks from (a fitted quad's
    /// centre can fall in a hole of the emitter). `[0, 0]` for tubes.
    pub anchor: [f32; 2],
}

/// Rec. 709 luminance of linear RGB.
fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

impl EmissiveLight {
    /// Emitting area (m²): the quad's area, or a tube's lateral area.
    #[must_use]
    pub fn area(&self) -> f32 {
        let ex = Vec3::from_array(self.axis_x).length();
        match self.shape {
            EmitterShape::Quad { .. } => 4.0 * ex * Vec3::from_array(self.axis_y).length(),
            EmitterShape::Tube { radius } => 2.0 * std::f32::consts::PI * radius * 2.0 * ex,
        }
    }

    /// Luminous flux (lm): `π · L · A` per emitting face.
    #[must_use]
    pub fn flux(&self) -> f32 {
        let faces = match self.shape {
            EmitterShape::Quad { two_sided: true, .. } => 2.0,
            _ => 1.0,
        };
        std::f32::consts::PI * luminance(self.radiance).max(0.0) * self.area() * faces
    }

    /// Largest luminous intensity (cd) in any direction: luminance × the largest projected area.
    #[must_use]
    pub fn peak_intensity(&self) -> f32 {
        let projected = match self.shape {
            EmitterShape::Quad { .. } => self.area(),
            EmitterShape::Tube { radius } => 2.0 * radius * 2.0 * Vec3::from_array(self.axis_x).length(),
        };
        luminance(self.radiance).max(0.0) * projected
    }

    /// Half diagonal: the distance from the centre to the farthest point of the emitter.
    #[must_use]
    pub fn half_diagonal(&self) -> f32 {
        let ex = Vec3::from_array(self.axis_x).length();
        match self.shape {
            EmitterShape::Quad { taper, .. } => {
                let ey = Vec3::from_array(self.axis_y).length() * (1.0 + taper.abs());
                (ex * ex + ey * ey).sqrt()
            }
            EmitterShape::Tube { radius } => ex + radius,
        }
    }

    /// Distance at which the light's peak illuminance falls to `cutoff_lux` (at least the
    /// emitter's own size): the windowed range the forward pass and the cluster build use.
    #[must_use]
    pub fn range(&self, cutoff_lux: f32) -> f32 {
        let far = (self.peak_intensity() / cutoff_lux.max(1e-6)).sqrt();
        far.max(2.0 * self.half_diagonal()) + self.half_diagonal()
    }

    /// The light under `model` (object to world). Radiance is unchanged; the axes take the
    /// matrix's scale, a tube's radius the mean scale across its axis.
    #[must_use]
    pub fn transformed(&self, model: &Mat4) -> Self {
        let linear = Mat3::from_mat4(*model);
        let ax = linear * Vec3::from_array(self.axis_x);
        let ay = linear * Vec3::from_array(self.axis_y);
        let shape = match self.shape {
            EmitterShape::Tube { radius } => {
                let along = Vec3::from_array(self.axis_x).length().max(1e-9);
                let axis_scale = ax.length() / along;
                let volume_scale = linear.determinant().abs();
                let across = (volume_scale / axis_scale.max(1e-9)).sqrt();
                EmitterShape::Tube {
                    radius: radius * across,
                }
            }
            quad => quad,
        };
        Self {
            centre: model.transform_point3(Vec3::from_array(self.centre)).to_array(),
            axis_x: ax.to_array(),
            axis_y: ay.to_array(),
            radiance: self.radiance,
            shape,
            anchor: self.anchor,
        }
    }
}

/// Fitting thresholds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FitParams {
    /// A piece is thin along an axis when its extent there is at most this share of the
    /// longest extent (rods: both short axes; plates: the normal axis).
    pub thin_ratio: f32,
    /// A triangle belongs to a plate face when its normal is within this cosine of the plate
    /// normal.
    pub face_cos: f32,
    /// A quad covering its triangles worse than this (emitting area / quad area) splits.
    pub min_coverage: f32,
    /// Recursion depth (pieces per component ≤ 2^depth).
    pub max_depth: u32,
    /// Lights per mesh; the brightest (by flux) are kept.
    pub max_lights_per_mesh: usize,
    /// Triangles dimmer than this share of the mesh's brightest are not emitters.
    pub min_relative_luminance: f32,
    /// Lights with less flux than this share of the mesh's strongest light are left out: thin
    /// rims and edges of a big emitter cost as much per pixel as the emitter and add little.
    pub min_flux_share: f32,
}

impl Default for FitParams {
    fn default() -> Self {
        Self {
            thin_ratio: 0.2,
            face_cos: 0.64, // 50°
            min_coverage: 0.75,
            max_depth: 5,
            max_lights_per_mesh: 48,
            min_relative_luminance: 0.01,
            min_flux_share: 0.03,
        }
    }
}

/// `true` when `material` draws emission the fitter turns into lights: lit, opaque or masked,
/// no material graph, some emission.
#[must_use]
pub fn material_emits(material: &LabMaterial) -> bool {
    let blended = matches!(
        crate::hair::effective_alpha_mode(material),
        ALPHA_BLEND | ALPHA_PREMULTIPLIED | ALPHA_ADDITIVE
    );
    !material.unlit
        && !blended
        && material.surface_graph.is_none()
        && material.emissive_intensity > 0.0
        && material.emissive.iter().any(|&e| e > 0.0)
}

#[cfg(test)]
thread_local! {
    static SIGNATURE_BYTES_HASHED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A key for the emission-relevant state of `material` (the renderer re-fits a cached mesh
/// when it changes). The emissive map's identity is its pixels, not its allocation: an
/// in-place texel edit changes the signature, and the same bytes at a new address keep it,
/// so the content cache answers instead of a re-fit.
///
/// The map's bytes mix word-wise (`lab_frame::mix_bytes_wide`, four folded-multiply
/// lanes), not byte at a time. This function always hashes the complete map; the collection
/// memoizes its result for registered immutable material generations and dynamic write epochs
/// (ADR-7869). Untracked borrowed materials still hash once per collection to detect in-place
/// edits. Alpha is hashed with RGB: the sampler reads RGB only, so an
/// alpha-only edit re-fits lights that cannot change — deterministic, only wasted work, and
/// whole-buffer hashing keeps "any pixel edit re-fits" true without a per-texel branch.
#[must_use]
pub fn material_signature(material: &LabMaterial) -> u64 {
    fn mix(h: &mut u64, v: u64) {
        *h ^= v;
        *h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for e in material.emissive {
        mix(&mut h, u64::from(e.to_bits()));
    }
    mix(&mut h, u64::from(material.emissive_intensity.to_bits()));
    mix(&mut h, u64::from(material.double_sided));
    if let Some(t) = &material.emissive_map {
        mix(&mut h, u64::from(t.width));
        mix(&mut h, u64::from(t.height));
        #[cfg(test)]
        SIGNATURE_BYTES_HASHED.with(|count| count.set(count.get() + t.rgba.len() as u64));
        crate::lab_frame::mix_bytes_wide(&mut h, &t.rgba);
        mix(&mut h, t.rgba.len() as u64);
    }
    h
}

fn srgb_to_linear(v: u8) -> f32 {
    let c = f32::from(v) / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Nearest-texel emissive map sample at `uv` (repeat), linear RGB; white without a map.
fn emissive_texel(map: Option<&LabTexture>, uv: [f32; 2]) -> [f32; 3] {
    let Some(t) = map else {
        return [1.0; 3];
    };
    if t.width == 0
        || t.height == 0
        || crate::mips::rgba8_bytes(t.width, t.height).is_none_or(|n| t.rgba.len() < n)
    {
        return [1.0; 3];
    }
    let x = ((uv[0].rem_euclid(1.0) * t.width as f32) as u32).min(t.width - 1);
    let y = ((uv[1].rem_euclid(1.0) * t.height as f32) as u32).min(t.height - 1);
    let i = (y as usize * t.width as usize + x as usize) * 4;
    [
        srgb_to_linear(t.rgba[i]),
        srgb_to_linear(t.rgba[i + 1]),
        srgb_to_linear(t.rgba[i + 2]),
    ]
}

#[derive(Clone, Copy)]
struct Tri {
    p: [Vec3; 3],
    /// Unit geometric normal (winding).
    n: Vec3,
    area: f32,
    radiance: [f32; 3],
}

impl Tri {
    fn centroid(&self) -> Vec3 {
        (self.p[0] + self.p[1] + self.p[2]) / 3.0
    }
}

/// Fits `material`'s emission on `vertices`/`indices` (object space). Empty when the material
/// does not emit ([`material_emits`]).
#[must_use]
pub fn fit_mesh(
    vertices: &[LabVertex],
    indices: &[u32],
    material: &LabMaterial,
    params: &FitParams,
) -> Vec<EmissiveLight> {
    if !material_emits(material) {
        return Vec::new();
    }
    fit_emission(vertices, indices, &Emission::of(material), params)
}

/// What a fit reads from the material: the emitted radiance, sidedness and the emissive map.
#[derive(Clone, Copy)]
pub(crate) struct Emission<'a> {
    /// `emissive × emissive_intensity`.
    pub(crate) base: [f32; 3],
    pub(crate) double_sided: bool,
    pub(crate) map: Option<&'a LabTexture>,
}

impl<'a> Emission<'a> {
    pub(crate) fn of(material: &'a LabMaterial) -> Self {
        Self {
            base: material.emissive.map(|e| e * material.emissive_intensity),
            double_sided: material.double_sided,
            map: material.emissive_map.as_ref(),
        }
    }
}

/// Vertex positions and texture coordinates a fit reads: a mesh's own vertices, or the copy a
/// background fit owns ([`fit_queue`]).
pub(crate) trait FitVertices {
    /// Vertex count.
    fn count(&self) -> usize;
    /// Object-space position of vertex `i < count`.
    fn position(&self, i: usize) -> [f32; 3];
    /// Texture coordinates of vertex `i < count`.
    fn uv(&self, i: usize) -> [f32; 2];
}

impl FitVertices for [LabVertex] {
    fn count(&self) -> usize {
        self.len()
    }
    fn position(&self, i: usize) -> [f32; 3] {
        self[i].position
    }
    fn uv(&self, i: usize) -> [f32; 2] {
        self[i].uv
    }
}

/// [`fit_mesh`] past the material test, over any [`FitVertices`].
pub(crate) fn fit_emission<V: FitVertices + ?Sized>(
    vertices: &V,
    indices: &[u32],
    emission: &Emission<'_>,
    params: &FitParams,
) -> Vec<EmissiveLight> {
    let base = emission.base;
    let mut tris = Vec::with_capacity(indices.len() / 3);
    let mut keys = Vec::with_capacity(indices.len());
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for i in 0..vertices.count() {
        let p = Vec3::from_array(vertices.position(i));
        lo = lo.min(p);
        hi = hi.max(p);
    }
    let weld = ((hi - lo).max_element() * 1e-4).max(1e-6);
    let n = vertices.count();
    for t in indices.as_chunks::<3>().0 {
        if t.iter().any(|&i| i as usize >= n) {
            continue;
        }
        let vs = t.map(|i| i as usize);
        let p = [0, 1, 2].map(|k| Vec3::from_array(vertices.position(vs[k])));
        let c = (p[1] - p[0]).cross(p[2] - p[0]);
        let area = 0.5 * c.length();
        if area.is_nan() || area <= 1e-12 {
            continue;
        }
        // Mean of the texel at the centroid and halfway to each corner.
        let uv = [0, 1, 2].map(|k| vertices.uv(vs[k]));
        let centre = [
            (uv[0][0] + uv[1][0] + uv[2][0]) / 3.0,
            (uv[0][1] + uv[1][1] + uv[2][1]) / 3.0,
        ];
        let mut tex = [0.0f32; 3];
        for s in [centre, mid(centre, uv[0]), mid(centre, uv[1]), mid(centre, uv[2])] {
            let e = emissive_texel(emission.map, s);
            for k in 0..3 {
                tex[k] += 0.25 * e[k];
            }
        }
        let radiance = [base[0] * tex[0], base[1] * tex[1], base[2] * tex[2]];
        if luminance(radiance) <= 0.0 {
            continue;
        }
        tris.push(Tri {
            p,
            n: c / (2.0 * area),
            area,
            radiance,
        });
        for q in p {
            keys.push(((q - lo) / weld).round().as_ivec3());
        }
    }
    let peak = tris.iter().map(|t| luminance(t.radiance)).fold(0.0f32, f32::max);
    let keep: Vec<usize> = (0..tris.len())
        .filter(|&i| luminance(tris[i].radiance) >= params.min_relative_luminance * peak)
        .collect();
    // Connected components over welded corners (union-find on the corner keys).
    let mut ids: HashMap<regia_core::glam::IVec3, usize> = HashMap::new();
    let mut parent: Vec<usize> = Vec::new();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    let mut corner = |key: regia_core::glam::IVec3, parent: &mut Vec<usize>| {
        *ids.entry(key).or_insert_with(|| {
            parent.push(parent.len());
            parent.len() - 1
        })
    };
    let mut tri_corner = Vec::with_capacity(keep.len());
    for &i in &keep {
        let c = [0, 1, 2].map(|k| corner(keys[i * 3 + k], &mut parent));
        for k in 1..3 {
            let (a, b) = (find(&mut parent, c[0]), find(&mut parent, c[k]));
            if a != b {
                parent[b] = a;
            }
        }
        tri_corner.push(c);
    }
    let mut components: HashMap<usize, Vec<Tri>> = HashMap::new();
    let mut edges: HashMap<usize, HashMap<(usize, usize), u32>> = HashMap::new();
    for (j, &i) in keep.iter().enumerate() {
        let root = find(&mut parent, tri_corner[j][0]);
        components.entry(root).or_default().push(tris[i]);
        let c = tri_corner[j];
        let e = edges.entry(root).or_default();
        for k in 0..3 {
            let (a, b) = (c[k], c[(k + 1) % 3]);
            *e.entry((a.min(b), a.max(b))).or_default() += 1;
        }
    }
    let mut roots: Vec<usize> = components.keys().copied().collect();
    roots.sort_unstable();
    let mut out = Vec::new();
    for root in roots {
        let Some(mut piece) = components.remove(&root) else {
            continue;
        };
        // Closed (every edge shared by two triangles): the inner faces are hidden, so each
        // face emits outward only. An open sheet of a double-sided material emits both ways.
        let closed = edges
            .get(&root)
            .is_some_and(|e| e.values().filter(|&&n| n == 2).count() * 20 >= e.len() * 19);
        let two_sided_sheets = !closed && emission.double_sided;
        // A closed mesh wound inside out (negative signed volume) still emits outward.
        if closed {
            let volume: f32 = piece.iter().map(|t| t.p[0].dot(t.p[1].cross(t.p[2]))).sum();
            if volume < 0.0 {
                for t in &mut piece {
                    t.n = -t.n;
                }
            }
        }
        fit_piece(&piece, two_sided_sheets, 0, params, &mut out);
    }
    let strongest = out.iter().map(EmissiveLight::flux).fold(0.0f32, f32::max);
    out.retain(|l| l.flux() >= params.min_flux_share * strongest);
    if out.len() > params.max_lights_per_mesh {
        out.sort_by(|a, b| b.flux().total_cmp(&a.flux()));
        out.truncate(params.max_lights_per_mesh);
    }
    out
}

fn mid(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5]
}

/// Area-weighted principal axes (longest first) and the area.
fn principal_axes(tris: &[Tri]) -> ([Vec3; 3], f32, Vec3) {
    let area: f32 = tris.iter().map(|t| t.area).sum();
    let mean = tris.iter().map(|t| t.centroid() * t.area).sum::<Vec3>() / area.max(1e-12);
    // Second moment of each triangle about the mean (vertex + centroid point masses).
    let mut cov = Mat3::ZERO;
    for t in tris {
        for q in [t.p[0], t.p[1], t.p[2], t.centroid()] {
            let d = q - mean;
            let w = t.area * 0.25;
            cov += Mat3::from_cols(d * d.x * w, d * d.y * w, d * d.z * w);
        }
    }
    (jacobi_axes(cov), area, mean)
}

/// Eigenvectors of a symmetric 3×3 matrix, largest eigenvalue first (cyclic Jacobi).
#[allow(clippy::needless_range_loop)] // Row/column rotations index both axes.
fn jacobi_axes(m: Mat3) -> [Vec3; 3] {
    let mut a = m.to_cols_array_2d();
    let mut v = Mat3::IDENTITY.to_cols_array_2d();
    for _ in 0..24 {
        let off = a[0][1].abs() + a[0][2].abs() + a[1][2].abs();
        if off < 1e-20 {
            break;
        }
        for (p, q) in [(0usize, 1usize), (0, 2), (1, 2)] {
            if a[p][q].abs() < 1e-30 {
                continue;
            }
            let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
            let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
            let c = 1.0 / (t * t + 1.0).sqrt();
            let s = t * c;
            for k in 0..3 {
                let (akp, akq) = (a[k][p], a[k][q]);
                a[k][p] = c * akp - s * akq;
                a[k][q] = s * akp + c * akq;
            }
            for k in 0..3 {
                let (apk, aqk) = (a[p][k], a[q][k]);
                a[p][k] = c * apk - s * aqk;
                a[q][k] = s * apk + c * aqk;
            }
            for row in &mut v {
                let (vp, vq) = (row[p], row[q]);
                row[p] = c * vp - s * vq;
                row[q] = s * vp + c * vq;
            }
        }
    }
    // `v` holds the eigenvectors as columns in the [col][row] layout of `to_cols_array_2d`
    // transposed by the row updates above: column j = (v[0][j], v[1][j], v[2][j]).
    let mut pairs: Vec<(f32, Vec3)> = (0..3)
        .map(|j| {
            (
                a[j][j],
                Vec3::new(v[0][j], v[1][j], v[2][j]).normalize_or(Vec3::AXES[j]),
            )
        })
        .collect();
    pairs.sort_by(|x, y| y.0.total_cmp(&x.0));
    let u1 = pairs[0].1;
    let u2 = (pairs[1].1 - u1 * pairs[1].1.dot(u1)).normalize_or(u1.any_orthonormal_vector());
    [u1, u2, u1.cross(u2)]
}

fn extents(tris: &[Tri], axes: &[Vec3; 3], origin: Vec3) -> ([f32; 3], [f32; 3]) {
    let mut lo = [f32::MAX; 3];
    let mut hi = [f32::MIN; 3];
    for t in tris {
        for q in t.p {
            for k in 0..3 {
                let s = (q - origin).dot(axes[k]);
                lo[k] = lo[k].min(s);
                hi[k] = hi[k].max(s);
            }
        }
    }
    (lo, hi)
}

fn mean_radiance(tris: &[Tri]) -> [f32; 3] {
    let area: f32 = tris.iter().map(|t| t.area).sum::<f32>().max(1e-12);
    let mut r = [0.0f32; 3];
    for t in tris {
        for (rk, tk) in r.iter_mut().zip(t.radiance) {
            *rk += tk * t.area / area;
        }
    }
    r
}

fn fit_piece(
    tris: &[Tri],
    two_sided_sheets: bool,
    depth: u32,
    params: &FitParams,
    out: &mut Vec<EmissiveLight>,
) {
    if tris.is_empty() {
        return;
    }
    let (axes, area, mean) = principal_axes(tris);
    let (lo, hi) = extents(tris, &axes, mean);
    let half = [0, 1, 2].map(|k| ((hi[k] - lo[k]) * 0.5).max(0.0));
    let centre = mean + (0..3).map(|k| axes[k] * (lo[k] + hi[k]) * 0.5).sum::<Vec3>();
    let radiance = mean_radiance(tris);
    let thin = params.thin_ratio;
    // A rod: thin in both short directions.
    if half[1] <= thin * half[0] && half[2] <= thin * half[0] {
        let radius = area / (4.0 * std::f32::consts::PI * half[0].max(1e-6));
        out.push(EmissiveLight {
            centre: centre.to_array(),
            axis_x: (axes[0] * half[0]).to_array(),
            axis_y: [0.0; 3],
            radiance,
            anchor: [0.0, 0.0],
            shape: EmitterShape::Tube { radius },
        });
        return;
    }
    // A plate: thin along its normal, faces coherent.
    if half[2] <= thin * half[1] {
        let n = axes[2];
        let (mut front, mut back, mut edge) = (Vec::new(), Vec::new(), Vec::new());
        for t in tris {
            let c = t.n.dot(n);
            if c >= params.face_cos {
                front.push(*t);
            } else if c <= -params.face_cos {
                back.push(*t);
            } else {
                edge.push(*t);
            }
        }
        let face_area: f32 = front.iter().chain(&back).map(|t| t.area).sum();
        if face_area >= 0.6 * area || depth >= params.max_depth {
            let sheet = two_sided_sheets && (front.is_empty() || back.is_empty());
            for (set, normal) in [(front, n), (back, -n)] {
                fit_face(&set, normal, sheet, depth, params, out);
            }
            let edge_area: f32 = edge.iter().map(|t| t.area).sum();
            if edge_area > 0.05 * area && depth < params.max_depth {
                fit_piece(&edge, two_sided_sheets, depth + 1, params, out);
            }
            return;
        }
    }
    if depth >= params.max_depth {
        // Out of splits: a tube of the same area along the long axis.
        let radius = area / (4.0 * std::f32::consts::PI * half[0].max(1e-6));
        out.push(EmissiveLight {
            centre: centre.to_array(),
            axis_x: (axes[0] * half[0]).to_array(),
            axis_y: [0.0; 3],
            radiance,
            anchor: [0.0, 0.0],
            shape: EmitterShape::Tube { radius },
        });
        return;
    }
    let (a, b) = split(tris, axes[0]);
    if a.is_empty() || b.is_empty() {
        fit_piece(tris, two_sided_sheets, params.max_depth, params, out);
        return;
    }
    fit_piece(&a, two_sided_sheets, depth + 1, params, out);
    fit_piece(&b, two_sided_sheets, depth + 1, params, out);
}

/// Splits at the area median of the centroids along `axis`.
fn split(tris: &[Tri], axis: Vec3) -> (Vec<Tri>, Vec<Tri>) {
    let mut order: Vec<(f32, Tri)> = tris.iter().map(|t| (t.centroid().dot(axis), *t)).collect();
    order.sort_by(|x, y| x.0.total_cmp(&y.0));
    let total: f32 = tris.iter().map(|t| t.area).sum();
    let mut acc = 0.0;
    let mut cut = order.len();
    for (i, (_, t)) in order.iter().enumerate() {
        acc += t.area;
        if acc >= 0.5 * total {
            cut = (i + 1).min(order.len().saturating_sub(1)).max(1);
            break;
        }
    }
    let b = order.split_off(cut.min(order.len()));
    (
        order.into_iter().map(|(_, t)| t).collect(),
        b.into_iter().map(|(_, t)| t).collect(),
    )
}

/// One plate face (triangles facing `normal`) as a trapezoid, split while it covers its
/// triangles poorly.
fn fit_face(
    tris: &[Tri],
    normal: Vec3,
    two_sided: bool,
    depth: u32,
    params: &FitParams,
    out: &mut Vec<EmissiveLight>,
) {
    if tris.is_empty() {
        return;
    }
    let area: f32 = tris.iter().map(|t| t.area).sum();
    let mean = tris.iter().map(|t| t.centroid() * t.area).sum::<Vec3>() / area.max(1e-12);
    // Candidate in-plane axes: the face's principal axes (degenerate for a square) and the
    // edges of its largest triangles (a quad's sides), each projected into the plane.
    let (axes, _, _) = principal_axes(tris);
    let mut by_area: Vec<&Tri> = tris.iter().collect();
    by_area.sort_by(|a, b| b.area.total_cmp(&a.area));
    let mut candidates = vec![axes[0]];
    for t in by_area.iter().take(4) {
        for k in 0..3 {
            candidates.push(t.p[(k + 1) % 3] - t.p[k]);
        }
    }
    let mut best: Option<(f32, Vec3, Vec3, Vec3, f32, f32)> = None;
    let pairs = candidates.iter().filter_map(|a| {
        let p = a - normal * a.dot(normal);
        (p.length_squared() > 1e-12).then(|| {
            let p = p.normalize();
            (p, normal.cross(p))
        })
    });
    for (p, q) in pairs.flat_map(|(p, q)| [(p, q), (q, p)]) {
        let pts: Vec<(f32, f32)> = tris
            .iter()
            .flat_map(|t| t.p)
            .map(|v| ((v - mean).dot(p), (v - mean).dot(q)))
            .collect();
        let (s0, s1) = pts
            .iter()
            .fold((f32::MAX, f32::MIN), |(a, b), &(s, _)| (a.min(s), b.max(s)));
        let (t0, t1) = pts
            .iter()
            .fold((f32::MAX, f32::MIN), |(a, b), &(_, t)| (a.min(t), b.max(t)));
        let len = (s1 - s0).max(1e-6);
        let t_mid = 0.5 * (t0 + t1);
        let (w0, w1) = containing_widths(&pts, s0, len, t_mid);
        let quad_area = len * 0.5 * (w0 + w1);
        // The trapezoid contains every vertex, so this is ≤ 1 (overlapping triangles aside).
        let coverage = area / quad_area.max(1e-12);
        let fit = coverage;
        if best.is_none_or(|b| fit > b.0 + 1e-4) {
            let centre = mean + p * 0.5 * (s0 + s1) + q * t_mid;
            let taper = ((w1 - w0) / (w0 + w1).max(1e-9)).clamp(-0.98, 0.98);
            best = Some((fit, centre, p * 0.5 * len, q * 0.25 * (w0 + w1), taper, coverage));
        }
    }
    let Some((_, centre, ex, mut ey, taper, coverage)) = best else {
        return;
    };
    if coverage < params.min_coverage && depth < params.max_depth && tris.len() > 1 {
        // Split across the face's longest in-plane extent (a half ring into quarters, not into
        // an inner and an outer arc).
        let long = axes[0] - normal * axes[0].dot(normal);
        let (a, b) = split(tris, long.normalize_or(ex.normalize_or_zero()));
        if !a.is_empty() && !b.is_empty() {
            fit_face(&a, normal, two_sided, depth + 1, params, out);
            fit_face(&b, normal, two_sided, depth + 1, params, out);
            return;
        }
    }
    // Front face along `normal`.
    if ex.cross(ey).dot(normal) < 0.0 {
        ey = -ey;
    }
    let scale = coverage.clamp(0.0, 1.25);
    // The probe's anchor: the emitting triangle centroid nearest the centre.
    let anchor_point = tris
        .iter()
        .map(Tri::centroid)
        .min_by(|a, b| {
            (*a - centre)
                .length_squared()
                .total_cmp(&(*b - centre).length_squared())
        })
        .unwrap_or(centre);
    let rel = anchor_point - centre;
    let anchor = [
        (rel.dot(ex) / ex.length_squared().max(1e-12)).clamp(-1.0, 1.0),
        (rel.dot(ey) / ey.length_squared().max(1e-12)).clamp(-1.0, 1.0),
    ];
    out.push(EmissiveLight {
        centre: centre.to_array(),
        axis_x: ex.to_array(),
        axis_y: ey.to_array(),
        radiance: mean_radiance(tris).map(|r| r * scale),
        anchor,
        shape: EmitterShape::Quad { two_sided, taper },
    });
}

/// End widths `(w0, w1)` of the smallest symmetric trapezoid about `t = t_mid` along `s` (from
/// `s0`, length `len`) that contains every point `(s, t)`: its half-width is linear in `s`, so
/// the best one is the line under the upper convex hull of `(u, 2|t − t_mid|)` at the middle.
fn containing_widths(pts: &[(f32, f32)], s0: f32, len: f32, t_mid: f32) -> (f32, f32) {
    let mut uh: Vec<(f32, f32)> = pts
        .iter()
        .map(|&(s, t)| (((s - s0) / len).clamp(0.0, 1.0), 2.0 * (t - t_mid).abs()))
        .collect();
    uh.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    let mut hull: Vec<(f32, f32)> = Vec::new();
    for p in uh.iter().copied() {
        while hull.len() >= 2 {
            let (o, a) = (hull[hull.len() - 2], hull[hull.len() - 1]);
            // Keep the chain turning clockwise (upper hull).
            if (a.0 - o.0) * (p.1 - o.1) - (a.1 - o.1) * (p.0 - o.0) >= 0.0 {
                hull.pop();
            } else {
                break;
            }
        }
        hull.push(p);
    }
    let max_h = uh.iter().map(|p| p.1).fold(0.0f32, f32::max);
    let edge = hull.windows(2).find(|w| w[0].0 <= 0.5 && w[1].0 >= 0.5);
    let Some(&[a, b]) = edge else {
        return (max_h, max_h);
    };
    if b.0 - a.0 <= 1e-6 {
        return (max_h, max_h);
    }
    let at = |u: f32| a.1 + (b.1 - a.1) * (u - a.0) / (b.0 - a.0);
    let (mut w0, mut w1) = (at(0.0), at(1.0));
    if w0 < 0.0 {
        w0 = 0.0;
        w1 = uh
            .iter()
            .filter(|p| p.0 > 1e-6)
            .map(|p| p.1 / p.0)
            .fold(0.0f32, f32::max);
    } else if w1 < 0.0 {
        w1 = 0.0;
        w0 = uh
            .iter()
            .filter(|p| p.0 < 1.0 - 1e-6)
            .map(|p| p.1 / (1.0 - p.0))
            .fold(0.0f32, f32::max);
    }
    (w0, w1)
}

/// Per-tier budget and range of the fitted lights.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EmissiveBudget {
    /// Lights uploaded per view (0 = the feature is off).
    pub max_lights: u32,
    /// Illuminance (lux) at which a light's range ends, as a share of the illuminance that
    /// puts a white Lambertian surface at the frame's middle grey (so the cut is ≤ that share
    /// of middle grey on screen whatever the exposure).
    pub cutoff_share: f32,
}

impl EmissiveBudget {
    /// Default cut: 4 % of middle grey on a white surface (the range, and so the pixels each
    /// light shades, sets the cost; 2 % measured 0.196 ms on the 5090 at 1440p).
    pub const CUTOFF_SHARE: f32 = 0.04;

    /// Illuminance cut for a frame whose exposure maps luminance `mid_grey_luminance` (cd/m²)
    /// to middle grey.
    #[must_use]
    pub fn cutoff_lux(&self, mid_grey_luminance: f32) -> f32 {
        std::f32::consts::PI * self.cutoff_share * mid_grey_luminance.max(1e-6)
    }
}

/// GPU record of a fitted light, in the clustered light buffer's 4 × vec4 layout:
/// `position_kind = [centre, 4 + shape + taper code]`, `color_intensity = [rgb, anchor code]`
/// ([`encode_anchor`]),
/// `direction_radius = [axis_x, range]`, `cone_shadow = [axis_y (tube: radius, 0, 0), 0]`.
/// `kind` is `4 + s + f` with `s` = 0 one-sided quad, 1 two-sided quad, 2 tube and
/// `f = (taper + 1) / 2 · 0.96` (< 1): the forward pass's `kind ≥ 3.5` branch reads it; the
/// cluster build treats it as a sphere of the range.
#[must_use]
pub fn gpu_record(light: &EmissiveLight, range: f32) -> [[f32; 4]; 4] {
    let (shape, taper, y) = match light.shape {
        EmitterShape::Quad { two_sided, taper } => (f32::from(u8::from(two_sided)), taper, light.axis_y),
        EmitterShape::Tube { radius } => (2.0, 0.0, [radius, 0.0, 0.0]),
    };
    let code = 4.0 + shape + (taper.clamp(-1.0, 1.0) + 1.0) * 0.5 * 0.96;
    [
        [light.centre[0], light.centre[1], light.centre[2], code],
        [
            light.radiance[0],
            light.radiance[1],
            light.radiance[2],
            encode_anchor(light.anchor),
        ],
        [light.axis_x[0], light.axis_x[1], light.axis_x[2], range],
        [y[0], y[1], y[2], 0.0],
    ]
}

/// The quad a receiver at `p` shades for `light` (corners in order, front face toward the
/// receiver for tubes), and whether it emits from both faces. Mirror of the shader's
/// `emitter_quad`.
#[must_use]
pub fn shading_quad(light: &EmissiveLight, p: Vec3) -> ([Vec3; 4], bool) {
    let c = Vec3::from_array(light.centre);
    let ex = Vec3::from_array(light.axis_x);
    match light.shape {
        EmitterShape::Quad { two_sided, taper } => {
            let ey = Vec3::from_array(light.axis_y);
            (
                [
                    c - ex - ey * (1.0 - taper),
                    c + ex - ey * (1.0 + taper),
                    c + ex + ey * (1.0 + taper),
                    c - ex + ey * (1.0 - taper),
                ],
                two_sided,
            )
        }
        EmitterShape::Tube { radius } => {
            let axis = ex.normalize_or(Vec3::Y);
            let to_p = p - c;
            let side = to_p - axis * to_p.dot(axis);
            let facing = side.normalize_or(axis.any_orthonormal_vector());
            let ey = facing.cross(axis) * radius;
            // ex × ey = axis × (facing × axis)·|ex|·r = facing·(…): front toward the receiver.
            ([c - ex - ey, c + ex - ey, c + ex + ey, c - ex + ey], true)
        }
    }
}

/// CPU reference of the diffuse irradiance (lux) the light delivers to a surface at `p` with
/// normal `n`: radiance × π × form factor (shadowless). Used by the tests and the budget.
#[must_use]
pub fn irradiance_at(light: &EmissiveLight, p: Vec3, n: Vec3) -> f32 {
    let (quad, two_sided) = shading_quad(light, p);
    let t1 = n.any_orthonormal_vector();
    let t2 = n.cross(t1);
    let local = quad.map(|q| {
        let d = q - p;
        Vec3::new(d.dot(t1), d.dot(t2), d.dot(n))
    });
    let ff = crate::ltc::polygon_integral(&local);
    // `t1 × t2 = n`; the shader's frame has the same handedness.
    let ff = if two_sided { ff.abs() } else { ff.max(0.0) };
    std::f32::consts::PI * luminance(light.radiance) * ff
}

/// What the last frame did with emissive lights ([`crate::FrameRenderer::emissive_lights`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EmissiveLightReport {
    /// Emissive meshes in the frame.
    pub meshes: u32,
    /// Lights fitted to them (before the view cull and the budget).
    pub fitted: u32,
    /// Lights whose range reaches the view.
    pub in_view: u32,
    /// Lights written into the clustered light buffer.
    pub uploaded: u32,
    /// In-view lights the budget dropped (dimmest on screen first).
    pub dropped: u32,
    /// The budget that applied (0 = off).
    pub cap: u32,
    /// Mesh keys that took new content this frame (a new mesh, a changed mesh or emissive
    /// material). Since v0.1.18 most are served by the content cache:
    /// [`crate::FrameRenderer::emissive_fit_stats`] counts the fits that ran. A mesh submitted
    /// with key 0 is counted only when its slice fingerprint misses (ADR-5208), not every frame.
    pub refits: u32,
    /// CPU time of the collection, milliseconds.
    pub cpu_ms: f32,
    /// Lights whose distance probe was traced this frame (rays against the scene).
    pub probes_traced: u32,
    /// Lights left out because almost none of their emission escapes (buried faces).
    pub buried: u32,
    /// Lights waiting for their distance probe. The load frame traces every light. Later
    /// frames admit whole emitters, nearest the eye first: the first pending emitter is
    /// always traced, a mesh key of 0 is always traced, and any further emitter is traced
    /// only while adding it still fits in [`PROBE_LIGHTS_PER_FRAME`]. The rest wait.
    pub probes_deferred: u32,
    /// Uploaded lights of emitters the frame's baked GI volume holds (ADR-1386,
    /// [`BAKED_EMITTER_TAG`]): the forward pass leaves their light to the volume where it
    /// covers. Over the budget they go after the other lights.
    pub baked: u32,
}

/// `cone_shadow.w` (`record[3][3]`, [`gpu_record`]) of a light whose emitter the frame's baked GI
/// volume holds (ADR-1386) is `BAKED_EMITTER_TAG − code`, where `code` is its distance-probe code
/// (k + 1 for probe tile k, 0 = none): always ≤ −1, which the forward pass's `emitter_tag` reads
/// as "baked". The pass then takes the light's diffuse back out where the volume covers the
/// receiver, and its specular where the probes stand in for the rough lobe.
pub const BAKED_EMITTER_TAG: f32 = -1.0;

struct CacheEntry {
    /// The mesh content the entry was made for ([`ContentKey`]): a key that names other
    /// content (a reused address, a changed mesh) takes another fit.
    content: ContentKey,
    last_frame: u64,
    /// The fit; `None` while the background worker runs it ([`fit_queue`]).
    lights: Option<std::sync::Arc<Vec<EmissiveLight>>>,
    /// Each light's distance probe ([`PROBE_TEXELS`] first-hit distances, [`light_probe`]),
    /// traced once in world space when the mesh is first drawn; `None` for a buried light.
    probes: Option<ProbeSet>,
    /// Model matrix the probes were traced for. An ephemeral mesh (ADR-5208) drops `probes`
    /// when this changes, because the probes are world-space. A keyed mesh keeps them: nearby
    /// occluders are taken to move with it.
    pose: [[f32; 4]; 4],
}

/// Exact signature of one immutable material generation, without retaining its pixel buffers.
struct MaterialSignatureMemo {
    owner: std::sync::Weak<LabMaterial>,
    epochs: [u64; 7],
    signature: u64,
    last_frame: u64,
}

/// Fitted lights per mesh key, kept while the mesh is drawn (renderer-owned).
#[derive(Default)]
pub(crate) struct EmissiveCache {
    entries: HashMap<u64, CacheEntry>,
    /// ADR-7869: runtime generation plus dynamic publication epochs, proven by weak ownership.
    material_signatures: HashMap<u64, MaterialSignatureMemo>,
    /// Object-space bounds per mesh key (occluder gathering).
    bounds: HashMap<u64, (Vec3, Vec3, usize)>,
    frame: u64,
    /// A collection with emitters ran. Later frames apply [`PROBE_LIGHTS_PER_FRAME`] per emitter.
    primed: bool,
    /// Per mesh of the last [`collect_frame_lights`] call (same order as its `meshes`): the
    /// largest range among that mesh's uploaded lights, 0 when none was uploaded.
    mesh_ranges: Vec<f32>,
    /// v0.1.18: content keys, content-keyed fits, per-object probes, the background worker.
    queue: FitQueue,
    /// ADR-1386: the emitters the baked volume lighting this frame holds (`None`: no volume,
    /// or one that does not name them). Their lights are tagged [`BAKED_EMITTER_TAG`].
    baked: Option<std::sync::Arc<crate::baked_gi::BakedEmitterSet>>,
    /// The local origin in the volume's space, added to drawn bounds before they are matched.
    origin_offset: [f32; 3],
    /// Object-space bounds of the vertices an emitting mesh's triangles use, per mesh key and
    /// content: what a drawn emitter is matched against the baked ones by.
    emitter_bounds: HashMap<u64, (ContentKey, BakedEmitter)>,
    /// Authored rect and tube lights (ADR-2592), shaded after the fitted ones; empty by default.
    authored: Vec<crate::light_profiles::AreaLightInput>,
    /// Each authored light's placement key and distance probe (`None`: buried, or shadowless).
    authored_probes: Vec<AuthoredProbe>,
    /// The tier has no emitter budget and authored lights exist: said once ([`collect_frame_lights`]).
    authored_unshaded_warned: bool,
}

/// An authored area light's placement key and its distance probe with its open share (`None`: buried, or
/// shadowless).
type AuthoredProbe = (u64, Option<(Vec<f32>, f32)>);

impl EmissiveCache {
    fn cached_material_signature(&mut self, key: u64, material: &LabMaterial) -> u64 {
        if material.emissive_map.is_none() {
            return material_signature(material);
        }
        let epochs = material.images.dynamic_write_epochs();
        if let Some(memo) = self.material_signatures.get_mut(&key)
            && std::ptr::eq(memo.owner.as_ptr(), material)
            && memo.owner.strong_count() > 0
        {
            if memo.epochs != epochs {
                memo.signature = material_signature(material);
                memo.epochs = epochs;
            }
            memo.last_frame = self.frame;
            return memo.signature;
        }
        let owner = crate::runtime::material_identity(key, material);
        let signature = material_signature(material);
        if let Some(owner) = owner {
            self.material_signatures.insert(
                key,
                MaterialSignatureMemo {
                    owner,
                    epochs,
                    signature,
                    last_frame: self.frame,
                },
            );
        }
        signature
    }

    fn prune_material_signatures(&mut self) {
        let frame = self.frame;
        self.material_signatures.retain(|_, memo| {
            memo.owner.strong_count() > 0 && frame.saturating_sub(memo.last_frame) < CACHE_KEEP_FRAMES
        });
        if self.material_signatures.len() > CACHE_MAX_ENTRIES {
            // Preserve the current working set even above the usual budget: evicting it would
            // make unchanged large scenes hash their maps again on every frame.
            let mut ages: Vec<_> = self
                .material_signatures
                .iter()
                .filter(|(_, memo)| memo.last_frame < frame)
                .map(|(key, memo)| (memo.last_frame, *key))
                .collect();
            ages.sort_unstable();
            for (_, key) in ages
                .iter()
                .take(self.material_signatures.len() - CACHE_MAX_ENTRIES)
            {
                self.material_signatures.remove(key);
            }
        }
    }

    /// The authored rect and tube lights the next collections shade (ADR-2592): after the fitted emitters,
    /// in the same budget; each traces its distance probe once per placement (the scene taken as static).
    pub(crate) fn set_authored_area_lights(&mut self, lights: &[crate::light_profiles::AreaLightInput]) {
        if self.authored.as_slice() != lights {
            self.authored.clear();
            self.authored.extend_from_slice(lights);
        }
    }

    /// Per mesh of the last [`collect_frame_lights`] call (indexed like its `meshes`): the
    /// largest range (m) of the mesh's lights this frame shades, 0 for meshes with none (not
    /// an emitter, every light buried, out of view or over the budget). The GPU irradiance
    /// probes take only the far part of such a mesh's emission, so it is counted once.
    pub(crate) fn mesh_ranges(&self) -> &[f32] {
        &self.mesh_ranges
    }

    /// The emitters the baked GI volume lighting the next collections holds (ADR-1386), `None`
    /// when no volume lights them or it does not name its emitters.
    pub(crate) fn set_baked_emitters(
        &mut self,
        set: Option<std::sync::Arc<crate::baked_gi::BakedEmitterSet>>,
    ) {
        self.baked = set;
    }

    /// The local origin in the volume's space (`FrameRenderer::origin_offset`).
    pub(crate) fn set_origin_offset(&mut self, offset: [f32; 3]) {
        self.origin_offset = offset;
    }

    /// Past the load frame, run new fits and probe traces on the background worker (the
    /// renderer's cache); off, everything runs in the collection (v0.1.17, the unit tests).
    /// Complete frames (ADR-2582): fits and traces run on the frame thread, and work already
    /// on the worker is waited for.
    pub(crate) fn set_complete(&mut self, on: bool) {
        self.queue.complete = on;
    }

    /// Whether fits run complete ([`Self::set_complete`]). The offscreen capture path reads it
    /// to save and restore the flag around a captured frame.
    pub(crate) fn complete(&self) -> bool {
        self.queue.complete
    }

    pub(crate) fn set_background(&mut self, on: bool) {
        self.queue.set_background(on);
    }

    /// Lends this frame's emissive meshes ([`crate::FrameRenderer::hold_mesh_identities`]).
    pub(crate) fn hold<'a>(
        &mut self,
        meshes: impl IntoIterator<Item = &'a std::sync::Arc<crate::runtime::CpuMesh>>,
    ) {
        self.queue.hold(meshes);
    }

    /// Cumulative fit-cache and worker counters.
    pub(crate) fn stats(&self) -> EmissiveFitStats {
        self.queue.stats
    }
}

/// Per light of a mesh: its distance probe and open share (`None` = buried).
type ProbeSet = std::sync::Arc<Vec<Option<(Vec<f32>, f32)>>>;

/// Frames a mesh may go undrawn before its fit and probes are dropped (~10 minutes at 60 Hz):
/// a unit that leaves the view and comes back is not fitted and traced again.
const CACHE_KEEP_FRAMES: u64 = 36_000;

/// Most fits kept; past it the least recently drawn go first.
const CACHE_MAX_ENTRIES: usize = 4096;

/// Most object-space bounds kept for occluder gathering before they are recomputed.
const BOUNDS_MAX_ENTRIES: usize = 65_536;

/// After the first collection with emitters (the load frame, which traces all), further
/// emitters are traced only while the frame's running total stays within this many lights.
/// The nearest pending emitter is admitted whole even past this count, and a mesh key of 0
/// always traces. One mesh keeps at most [`FitParams::max_lights_per_mesh`] lights (48).
pub const PROBE_LIGHTS_PER_FRAME: usize = 16;

/// Distance-probe resolution per light: an octahedral map of the sphere of directions around
/// the light's centre (Cigolle et al., "A Survey of Efficient Representations for Independent
/// Unit Vectors", JCGT 2014), each texel the distance to the first surface that way.
pub const PROBE_RES: u32 = 16;
/// Texels per probe.
pub const PROBE_TEXELS: usize = (PROBE_RES * PROBE_RES) as usize;
/// Probes per row of the atlas texture (`PROBE_ATLAS_COLUMNS · PROBE_RES` texels wide).
pub const PROBE_ATLAS_COLUMNS: u32 = 16;

/// How far probe rays look (metres): beyond it a light's range has faded (the window) or the
/// occluder is far enough that the probe's angular resolution could not place its shadow.
const PROBE_REACH: f32 = 12.0;

/// A light whose emitting directions nearly all hit something right at its surface is buried
/// (a face resting on other geometry) and is left out.
const BURIED_SHARE: f32 = 0.97;

/// Floats of a probe atlas holding `lights` tiles (whole rows of [`PROBE_ATLAS_COLUMNS`]).
#[must_use]
pub fn probe_atlas_len(lights: usize) -> usize {
    let rows = lights.div_ceil(PROBE_ATLAS_COLUMNS as usize);
    rows * (PROBE_ATLAS_COLUMNS * PROBE_RES) as usize * PROBE_RES as usize
}

/// Copies `probe` into tile `k` of a row-major atlas `PROBE_ATLAS_COLUMNS · PROBE_RES` wide.
fn write_probe_tile(atlas: &mut [f32], k: usize, probe: &[f32]) {
    let res = PROBE_RES as usize;
    let width = PROBE_ATLAS_COLUMNS as usize * res;
    let (tx, ty) = (k % PROBE_ATLAS_COLUMNS as usize, k / PROBE_ATLAS_COLUMNS as usize);
    for y in 0..res {
        let row = (ty * res + y) * width + tx * res;
        if let (Some(dst), Some(src)) = (atlas.get_mut(row..row + res), probe.get(y * res..(y + 1) * res)) {
            dst.copy_from_slice(src);
        }
    }
}

/// Unit direction to octahedral coordinates in `[0, 1]²` (+Y at the centre).
#[must_use]
pub fn oct_encode(d: Vec3) -> [f32; 2] {
    let n = d / (d.x.abs() + d.y.abs() + d.z.abs()).max(1e-12);
    let (x, z) = if n.y < 0.0 {
        ((1.0 - n.z.abs()) * sign(n.x), (1.0 - n.x.abs()) * sign(n.z))
    } else {
        (n.x, n.z)
    };
    [x * 0.5 + 0.5, z * 0.5 + 0.5]
}

/// Inverse of [`oct_encode`].
#[must_use]
pub fn oct_decode(uv: [f32; 2]) -> Vec3 {
    let (x, z) = (uv[0] * 2.0 - 1.0, uv[1] * 2.0 - 1.0);
    let y = 1.0 - x.abs() - z.abs();
    let (x, z) = if y < 0.0 {
        ((1.0 - z.abs()) * sign(x), (1.0 - x.abs()) * sign(z))
    } else {
        (x, z)
    };
    Vec3::new(x, y, z).normalize_or(Vec3::Y)
}

/// `±1`, with `+1` at zero (WGSL's `select(-1, 1, v >= 0)`; `f32::signum` keeps −0's sign).
fn sign(v: f32) -> f32 {
    if v >= 0.0 { 1.0 } else { -1.0 }
}

/// The point a light's distance probe looks from: its anchor on the emitter (see
/// [`EmissiveLight::anchor`]). Mirror of the shader's `emitter_probe_origin`.
#[must_use]
pub fn probe_origin(light: &EmissiveLight) -> Vec3 {
    if matches!(light.shape, EmitterShape::Tube { .. }) {
        return Vec3::from_array(light.centre);
    }
    let [s, t] = decode_anchor(encode_anchor(light.anchor));
    Vec3::from_array(light.centre) + Vec3::from_array(light.axis_x) * s + Vec3::from_array(light.axis_y) * t
}

/// `(s, t)` in `[−1, 1]²` as one float holding two 8-bit codes (exact in f32): the GPU record's
/// `color_intensity.w`.
#[must_use]
pub fn encode_anchor(anchor: [f32; 2]) -> f32 {
    let q = |v: f32| ((v.clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0).round();
    q(anchor[0]) * 256.0 + q(anchor[1])
}

/// Inverse of [`encode_anchor`].
#[must_use]
pub fn decode_anchor(code: f32) -> [f32; 2] {
    let hi = (code / 256.0).floor();
    let lo = code - hi * 256.0;
    [hi / 255.0 * 2.0 - 1.0, lo / 255.0 * 2.0 - 1.0]
}

/// Where a light's probe rays start along each direction from its centre: just off a quad's
/// plane, outside a tube's cylinder.
fn probe_start(light: &EmissiveLight) -> f32 {
    match light.shape {
        EmitterShape::Quad { .. } => 0.002 + 0.005 * light.half_diagonal(),
        EmitterShape::Tube { radius } => 1.5 * radius + 0.005,
    }
}

/// Points spread over the emitter the probe rays leave from: the anchor, then a 3 × 3 grid over
/// a quad (inside its trapezoid) or four points along a tube's axis.
fn probe_origins(light: &EmissiveLight) -> Vec<Vec3> {
    let c = Vec3::from_array(light.centre);
    let ex = Vec3::from_array(light.axis_x);
    let ey = Vec3::from_array(light.axis_y);
    let mut out = vec![probe_origin(light)];
    match light.shape {
        EmitterShape::Quad { taper, .. } => {
            for s in [-0.67f32, 0.0, 0.67] {
                for t in [-0.67f32, 0.0, 0.67] {
                    out.push(c + ex * s + ey * (t * (1.0 + taper * s)));
                }
            }
        }
        EmitterShape::Tube { .. } => {
            for u in [-0.75f32, -0.25, 0.25, 0.75] {
                out.push(c + ex * u);
            }
        }
    }
    out
}

/// The light's distance probe and open share. Per octahedral texel: the farthest first hit,
/// measured from the anchor ([`probe_origin`]), of rays leaving that way from points spread
/// over the emitter ([`probe_origins`]) — a receiver beyond it is in shadow. The open share is
/// the fraction of emitting rays not stopped right at the emitter (a grille or a housing pressed
/// against part of it); the light's radiance is scaled by it. `None` when the light is buried
/// (open share below `1 − BURIED_SHARE`).
fn light_probe(light: &EmissiveLight, bvh: &Bvh, reach: f32) -> Option<(Vec<f32>, f32)> {
    let anchor = probe_origin(light);
    let origins = probe_origins(light);
    let start = probe_start(light);
    let normal = Vec3::from_array(light.axis_x)
        .cross(Vec3::from_array(light.axis_y))
        .normalize_or_zero();
    let one_sided = matches!(light.shape, EmitterShape::Quad { two_sided: false, .. });
    let is_quad = matches!(light.shape, EmitterShape::Quad { .. });
    let close = start + 0.02 + 0.05 * light.half_diagonal();
    let (mut emitting, mut blocked) = (0u32, 0u32);
    let mut out = Vec::with_capacity(PROBE_TEXELS);
    for y in 0..PROBE_RES {
        for x in 0..PROBE_RES {
            let uv = [
                (x as f32 + 0.5) / PROBE_RES as f32,
                (y as f32 + 0.5) / PROBE_RES as f32,
            ];
            let d = oct_decode(uv);
            let emits = !one_sided || d.dot(normal) > 0.1;
            let mut far = 0.0f32;
            for o in &origins {
                let hit = bvh.first_hit(*o + d * start, d, reach);
                let t = hit.map_or(reach, |t| t + start);
                far = far.max((*o + d * t - anchor).length());
                if emits {
                    emitting += 1;
                    // How far the hit lies off the emitter (a quad: across its plane), so a face
                    // pressed against a surface counts as blocked at grazing angles too. A ray
                    // that hits nothing is open.
                    let off = if is_quad { t * d.dot(normal).abs() } else { t };
                    if hit.is_some() && off < close {
                        blocked += 1;
                    }
                }
            }
            out.push(far.min(reach));
        }
    }
    let open = if emitting == 0 {
        1.0
    } else {
        1.0 - blocked as f32 / emitting as f32
    };
    (open > 1.0 - BURIED_SHARE).then_some((out, open))
}

/// Where a world-space light's probe rays can reach: `(centre, reach, pad)`. Its rays leave
/// from at most `pad` off the centre ([`probe_origins`] plus [`probe_start`], with a rounding
/// margin) and run `reach`, the distance [`light_probe`] traces.
fn probe_sphere(light: &EmissiveLight, cutoff_lux: f32) -> (Vec3, f32, f32) {
    let centre = Vec3::from_array(light.centre);
    let reach = PROBE_REACH.min(light.range(cutoff_lux));
    let offset = probe_origins(light)
        .iter()
        .map(|o| (*o - centre).length())
        .fold(0.0f32, f32::max);
    (centre, reach, offset + probe_start(light) + 1e-3 * (1.0 + reach))
}

/// World-space triangles around the emitters, in a bounding volume hierarchy (median split),
/// for the distance-probe rays.
struct Bvh {
    nodes: Vec<BvhNode>,
    tris: Vec<[Vec3; 3]>,
}

#[derive(Clone, Copy)]
struct BvhNode {
    lo: Vec3,
    hi: Vec3,
    /// Leaf: first triangle; inner: index of the left child (the right one follows it).
    first: u32,
    /// Triangles in a leaf, 0 for an inner node.
    count: u32,
}

/// The probe BVH's candidate triangles: those of the opaque rigid meshes inside the box of the
/// probe spheres their bounds reach, world space, in mesh order, with the spheres each mesh
/// reached. [`Self::near`] keeps the ones a probe ray can reach. A background trace gathers on
/// the worker from a [`GatherInput`] the frame thread copied off the meshes ([`fit_queue`]).
pub(crate) struct Gathered {
    tris: Vec<[Vec3; 3]>,
    /// Per reaching mesh: the probe spheres it reached and the end of its triangles in `tris`.
    groups: Vec<(Vec<usize>, usize)>,
}

/// One mesh whose world bounds reach a probe sphere: which spheres, its model matrix and the box
/// a triangle must touch to be a candidate.
struct Reach {
    reached: Vec<usize>,
    model: Mat4,
    lo: Vec3,
    hi: Vec3,
}

impl Reach {
    /// Whether `mesh` (opaque, rigid, with triangles) reaches any of `probes`: its object bounds
    /// (cached per mesh key in `bounds`) under its model matrix against each sphere.
    fn of(
        mesh: &crate::FrameMesh<'_>,
        probes: &[(Vec3, f32, f32)],
        bounds: &mut HashMap<u64, (Vec3, Vec3, usize)>,
    ) -> Option<Self> {
        let blended = matches!(
            crate::hair::effective_alpha_mode(mesh.material),
            ALPHA_BLEND | ALPHA_PREMULTIPLIED | ALPHA_ADDITIVE
        );
        if blended || !mesh.joints.is_empty() || mesh.indices.is_empty() {
            return None;
        }
        let object = |vs: &[LabVertex]| {
            vs.iter().fold(
                (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN), vs.len()),
                |(lo, hi, n), v| {
                    let p = Vec3::from_array(v.position);
                    (lo.min(p), hi.max(p), n)
                },
            )
        };
        // Key 0 has no identity across frames (`FrameMesh::mesh_key`): its box is measured on
        // every call. A cache under the slice fingerprint (addresses and lengths) kept the
        // first frame's box for a buffer rebuilt or edited in place, and the occluder then
        // left the probe BVH.
        let (lo, hi, _) = if mesh.mesh_key == 0 {
            object(mesh.vertices)
        } else {
            let b = bounds
                .entry(mesh.mesh_key)
                .or_insert_with(|| object(mesh.vertices));
            if b.2 != mesh.vertices.len() {
                *b = object(mesh.vertices);
            }
            *b
        };
        let model = Mat4::from_cols_array_2d(&mesh.model);
        // World bounds of the object box (eight corners).
        let (mut wlo, mut whi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for k in 0..8 {
            let c = Vec3::new(
                if k & 1 == 0 { lo.x } else { hi.x },
                if k & 2 == 0 { lo.y } else { hi.y },
                if k & 4 == 0 { lo.z } else { hi.z },
            );
            let w = model.transform_point3(c);
            wlo = wlo.min(w);
            whi = whi.max(w);
        }
        let reached: Vec<usize> = probes
            .iter()
            .enumerate()
            .filter(|(_, (c, r, _))| (c.clamp(wlo, whi) - *c).length_squared() <= r * r)
            .map(|(i, _)| i)
            .collect();
        if reached.is_empty() {
            return None;
        }
        // Only triangles a probe ray can reach: a ray leaves at most `pad` from its light's
        // centre and runs `r`, so a triangle outside every reached sphere of radius `r + pad`
        // is never hit. Dropping those leaves every first hit as it was.
        let (mut ulo, mut uhi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for &i in &reached {
            let (c, r, pad) = probes[i];
            ulo = ulo.min(c - Vec3::splat(r + pad));
            uhi = uhi.max(c + Vec3::splat(r + pad));
        }
        Some(Self {
            reached,
            model,
            lo: ulo,
            hi: uhi,
        })
    }

    /// Appends the mesh's triangles that touch the reach box to `tris`, in world space: the
    /// object-space `positions` under the model matrix, in index order (the same arithmetic
    /// whichever thread runs it and wherever the positions were read from).
    fn gather(&self, positions: impl Iterator<Item = [f32; 3]>, indices: &[u32], tris: &mut Vec<[Vec3; 3]>) {
        let model = self.model;
        let world: Vec<Vec3> = positions
            .map(|p| model.transform_point3(Vec3::from_array(p)))
            .collect();
        for t in indices.as_chunks::<3>().0 {
            if let (Some(a), Some(b), Some(c)) = (
                world.get(t[0] as usize),
                world.get(t[1] as usize),
                world.get(t[2] as usize),
            ) {
                let (tlo, thi) = (a.min(*b).min(*c), a.max(*b).max(*c));
                if tlo.cmpgt(self.hi).any() || thi.cmplt(self.lo).any() {
                    continue;
                }
                tris.push([*a, *b, *c]);
            }
        }
    }
}

/// The frame thread's share of a background probe gather: for each mesh a probe sphere reaches,
/// its [`Reach`] and a copy of its object-space positions and indices (the frame's meshes are
/// borrowed). [`Self::gather`] then world-transforms and filters the triangles on the worker:
/// the per-triangle work of a large mesh is the cost a landing frame must not pay
/// ([`EMISSIVE_FRAME_BUDGET_MS`]).
pub(crate) struct GatherInput {
    meshes: Vec<(Reach, Vec<[f32; 3]>, Vec<u32>)>,
}

impl GatherInput {
    /// The meshes of `meshes` that reach `probes` (centre, reach, pad), copied.
    pub(crate) fn collect(
        meshes: &[crate::FrameMesh<'_>],
        probes: &[(Vec3, f32, f32)],
        bounds: &mut HashMap<u64, (Vec3, Vec3, usize)>,
    ) -> Self {
        let meshes = meshes
            .iter()
            .filter_map(|mesh| {
                let reach = Reach::of(mesh, probes, bounds)?;
                let positions = mesh.vertices.iter().map(|v| v.position).collect();
                Some((reach, positions, mesh.indices.to_vec()))
            })
            .collect();
        Self { meshes }
    }

    /// The candidate triangles, exactly as [`Gathered::collect`] gathers them from the meshes.
    pub(crate) fn gather(self) -> Gathered {
        let mut tris = Vec::new();
        let mut groups = Vec::with_capacity(self.meshes.len());
        for (reach, positions, indices) in self.meshes {
            reach.gather(positions.into_iter(), &indices, &mut tris);
            groups.push((reach.reached, tris.len()));
        }
        Gathered { tris, groups }
    }
}

impl Gathered {
    /// Candidate triangles of `meshes` for `probes` (centre, reach, pad), gathered here.
    pub(crate) fn collect(
        meshes: &[crate::FrameMesh<'_>],
        probes: &[(Vec3, f32, f32)],
        bounds: &mut HashMap<u64, (Vec3, Vec3, usize)>,
    ) -> Self {
        let mut tris = Vec::new();
        let mut groups = Vec::new();
        for mesh in meshes {
            let Some(reach) = Reach::of(mesh, probes, bounds) else {
                continue;
            };
            reach.gather(mesh.vertices.iter().map(|v| v.position), mesh.indices, &mut tris);
            groups.push((reach.reached, tris.len()));
        }
        Self { tris, groups }
    }

    /// Candidate triangles gathered (before [`Self::near`]).
    pub(crate) fn candidates(&self) -> usize {
        self.tris.len()
    }

    /// The candidates a probe ray can reach: within `r + pad` of a sphere their mesh reached.
    pub(crate) fn near(self, probes: &[(Vec3, f32, f32)]) -> Vec<[Vec3; 3]> {
        let mut out = Vec::with_capacity(self.tris.len());
        let mut start = 0;
        for (reached, end) in &self.groups {
            for t in &self.tris[start..*end] {
                let [a, b, c] = *t;
                let (tlo, thi) = (a.min(b).min(c), a.max(b).max(c));
                let near = reached.iter().any(|&i| {
                    let (centre, r, pad) = probes[i];
                    (centre.clamp(tlo, thi) - centre).length_squared() <= (r + pad) * (r + pad)
                });
                if near {
                    out.push(*t);
                }
            }
            start = *end;
        }
        out
    }
}

impl Bvh {
    /// Triangles of the opaque rigid meshes whose bounds reach any of `probes` (centre, radius).
    fn from_meshes(
        meshes: &[crate::FrameMesh<'_>],
        probes: &[(Vec3, f32, f32)],
        bounds: &mut HashMap<u64, (Vec3, Vec3, usize)>,
    ) -> Self {
        Self::from_tris(Gathered::collect(meshes, probes, bounds).near(probes))
    }

    fn from_tris(tris: Vec<[Vec3; 3]>) -> Self {
        let mut bvh = Self {
            nodes: Vec::new(),
            tris,
        };
        let n = bvh.tris.len() as u32;
        if n > 0 {
            bvh.nodes.push(BvhNode {
                lo: Vec3::ZERO,
                hi: Vec3::ZERO,
                first: 0,
                count: n,
            });
            bvh.split(0);
        }
        bvh
    }

    fn split(&mut self, node: usize) {
        let BvhNode { first, count, .. } = self.nodes[node];
        let range = first as usize..(first + count) as usize;
        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for t in &self.tris[range.clone()] {
            for p in t {
                lo = lo.min(*p);
                hi = hi.max(*p);
            }
        }
        self.nodes[node].lo = lo;
        self.nodes[node].hi = hi;
        if count <= 4 {
            return;
        }
        let axis = (hi - lo).max_position();
        let slice = &mut self.tris[range];
        let mid = slice.len() / 2;
        slice.select_nth_unstable_by(mid, |a, b| {
            let ca = a[0][axis] + a[1][axis] + a[2][axis];
            let cb = b[0][axis] + b[1][axis] + b[2][axis];
            ca.total_cmp(&cb)
        });
        let left = self.nodes.len();
        self.nodes.push(BvhNode {
            lo,
            hi,
            first,
            count: mid as u32,
        });
        self.nodes.push(BvhNode {
            lo,
            hi,
            first: first + mid as u32,
            count: count - mid as u32,
        });
        self.nodes[node].first = left as u32;
        self.nodes[node].count = 0;
        self.split(left);
        self.split(left + 1);
    }

    /// Distance to the nearest triangle along `o + t·d`, `t ∈ (0, t_max)`.
    fn first_hit(&self, o: Vec3, d: Vec3, t_max: f32) -> Option<f32> {
        if self.nodes.is_empty() {
            return None;
        }
        let mut best = t_max;
        let inv = Vec3::ONE / d;
        let mut stack = [0u32; 64];
        let mut top = 1;
        while top > 0 {
            top -= 1;
            let node = self.nodes[stack[top] as usize];
            // Slab test (Kay & Kajiya).
            let t0 = (node.lo - o) * inv;
            let t1 = (node.hi - o) * inv;
            let near = t0.min(t1).max_element().max(0.0);
            let far = t0.max(t1).min_element().min(best);
            if near > far {
                continue;
            }
            if node.count > 0 {
                for t in &self.tris[node.first as usize..(node.first + node.count) as usize] {
                    if let Some(hit) = ray_hit(o, d, t, best) {
                        best = hit;
                    }
                }
            } else if top + 2 <= stack.len() {
                stack[top] = node.first;
                stack[top + 1] = node.first + 1;
                top += 2;
            }
        }
        (best < t_max).then_some(best)
    }
}

/// Möller–Trumbore ray/triangle test (either face): the hit distance in `(1e-5, t_max)`.
fn ray_hit(o: Vec3, d: Vec3, t: &[Vec3; 3], t_max: f32) -> Option<f32> {
    let e1 = t[1] - t[0];
    let e2 = t[2] - t[0];
    let p = d.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    let s = o - t[0];
    let u = s.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = d.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let dist = e2.dot(q) * inv;
    (dist > 1e-5 && dist < t_max).then_some(dist)
}

/// `true` when the sphere `centre`/`radius` can reach `view`'s frustum (a cone test).
fn reaches_view(view: &crate::light_budget::LightView, centre: Vec3, radius: f32) -> bool {
    let eye = Vec3::from_array(view.eye);
    let forward = (Vec3::from_array(view.target) - eye).normalize_or(Vec3::NEG_Z);
    let to = centre - eye;
    let depth = to.dot(forward);
    if depth < -radius || depth > view.far + radius {
        return false;
    }
    let half_y = (view.fov_y * 0.5).tan().max(0.05);
    let half = half_y * view.aspect.max(1.0);
    // Distance from the cone axis allowed at this depth, widened by the sphere radius
    // measured across the cone's slant.
    let lateral = (to - forward * depth).length();
    lateral <= depth.max(0.0) * half + radius * (1.0 + half * half).sqrt()
}

/// The world bounds a baked volume names `mesh` by (ADR-1386): the object-space bounds of the
/// vertices its triangles use ([`BakedEmitter::from_points`], cached per mesh key and
/// `content`) under its model matrix ([`BakedEmitter::from_object_bounds`], the bake's own
/// definition).
fn drawn_emitter_bounds(
    cache: &mut HashMap<u64, (ContentKey, BakedEmitter)>,
    mesh: &crate::FrameMesh<'_>,
    content: Option<ContentKey>,
) -> Option<BakedEmitter> {
    let object = |mesh: &crate::FrameMesh<'_>| {
        BakedEmitter::from_points(
            mesh.indices
                .iter()
                .filter_map(|&i| mesh.vertices.get(i as usize))
                .map(|v| v.position),
        )
    };
    let bounds = match content.filter(|_| mesh.mesh_key != 0) {
        Some(c) => match cache.get(&mesh.mesh_key) {
            Some(&(k, b)) if k == c => b,
            _ => {
                let b = object(mesh)?;
                cache.insert(mesh.mesh_key, (c, b));
                b
            }
        },
        None => object(mesh)?,
    };
    BakedEmitter::from_object_bounds(bounds.min, bounds.max, &mesh.model)
}

/// One fitted light in world space on its way to the GPU ([`collect_frame_lights`]).
struct WorldLight<'a> {
    light: EmissiveLight,
    range: f32,
    probe: Option<&'a [f32]>,
    /// Index of its mesh in the frame's meshes.
    mesh: usize,
    /// The frame's baked GI volume holds its emitter ([`BAKED_EMITTER_TAG`]).
    baked: bool,
}

/// Cache key for a mesh submitted with `mesh_key` 0 (ADR-5208).
///
/// The low 56 bits mix the borrowed vertex and index slices' addresses and lengths. The top
/// byte is [`crate::EPHEMERAL_KEY`], so the key is never 0 and does not collide with a host
/// mesh key. The same allocation next frame hits the content fingerprint; another mesh does
/// not share the entry. The object's own key stays 0, so probes are not shared across submits.
fn ephemeral_fit_key(mesh: &crate::FrameMesh<'_>) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = crate::key_hash::KeyHasher::default();
    hasher.write_usize(mesh.vertices.as_ptr() as usize);
    hasher.write_usize(mesh.indices.as_ptr() as usize);
    hasher.write_usize(mesh.vertices.len());
    hasher.write_usize(mesh.indices.len());
    crate::EPHEMERAL_KEY | (hasher.finish() & 0x00FF_FFFF_FFFF_FFFF)
}

/// The frame's emissive lights as GPU records (at most `cap`), fitted per mesh through
/// `cache`, moved to world space, culled to `view` and ranked by projected importance.
pub(crate) fn collect_frame_lights(
    cache: &mut EmissiveCache,
    meshes: &[crate::FrameMesh<'_>],
    view: &crate::light_budget::LightView,
    cap: u32,
    cutoff_lux: f32,
) -> (Vec<[[f32; 4]; 4]>, Vec<f32>, EmissiveLightReport) {
    let _span = tracing::info_span!("emissive_lights").entered();
    let started = std::time::Instant::now();
    let mut report = EmissiveLightReport {
        cap,
        ..EmissiveLightReport::default()
    };
    cache.mesh_ranges.clear();
    cache.mesh_ranges.resize(meshes.len(), 0.0);
    if cap == 0 {
        if !cache.authored.is_empty() && !cache.authored_unshaded_warned {
            cache.authored_unshaded_warned = true;
            tracing::warn!(
                "{} rect or tube lights emit nothing: this tier has no emitter budget (FrameQuality::emissive_lights is 0)",
                cache.authored.len()
            );
        }
        return (Vec::new(), Vec::new(), report);
    }
    cache.frame += 1;
    let frame = cache.frame;
    let params = FitParams::default();
    // v0.1.18: past the load frame, a fit or probe trace that is not cached runs on the
    // background worker, and the frame thread starts no new piece past its budget
    // ([`fit_queue`]). The load frame does everything here, as before.
    let background = cache.primed && cache.queue.background();
    cache.queue.stats.background = background;
    report.probes_traced += cache.queue.land(&mut cache.entries, frame);
    let mut budget = fit_queue::FrameBudget::new(started, background);
    // Pass 1: the fitted lights of every emitting mesh (object space, cached per key, and per
    // content across keys).
    struct Emitter {
        mesh: usize,
        key: u64,
        object: u64,
        content: Option<ContentKey>,
        model: Mat4,
        /// `None` while its fit is pending (or its hash waits for the budget).
        lights: Option<std::sync::Arc<Vec<EmissiveLight>>>,
        probes: Option<ProbeSet>,
        /// Its probes wait for a later frame ([`PROBE_LIGHTS_PER_FRAME`], or the worker).
        deferred: bool,
        /// The frame's baked GI volume holds it (ADR-1386).
        baked: bool,
        /// Submitted with key 0: `key` is its fingerprint. Its probes still trace on the frame
        /// it appears (the per-frame budget applies to host-keyed meshes only).
        ephemeral: bool,
    }
    let mut emitters: Vec<Emitter> = Vec::new();
    // One lookup per material per frame. Tracked generations reuse their exact signature across
    // collections; untracked borrowed materials still hash once to detect arbitrary pixel edits.
    let mut signatures: Vec<(usize, u64)> = Vec::new();
    for (mesh_index, mesh) in meshes.iter().enumerate() {
        if mesh.vertices.is_empty()
            || mesh.indices.is_empty()
            || !mesh.joints.is_empty()
            || !material_emits(mesh.material)
        {
            continue;
        }
        report.meshes += 1;
        let material_ptr = mesh.material as *const crate::lab_frame::LabMaterial as usize;
        let signature = match signatures.iter().find(|(ptr, _)| *ptr == material_ptr) {
            Some(&(_, signature)) => signature,
            None => {
                let signature = cache.cached_material_signature(mesh.material_key, mesh.material);
                signatures.push((material_ptr, signature));
                signature
            }
        };
        // Key 0 has no host identity (ADR-5208). A fingerprint of the borrowed slices stands
        // in, inside the ephemeral range, so the content cache, the frame budget and the worker
        // apply. The object key stays 0: those probes are not shared with another submit.
        let ephemeral = mesh.mesh_key == 0;
        let mut keyed = *mesh;
        if ephemeral {
            keyed.mesh_key = ephemeral_fit_key(mesh);
        }
        let mut emitter = Emitter {
            mesh: mesh_index,
            key: keyed.mesh_key,
            object: mesh.object_key,
            content: None,
            model: Mat4::from_cols_array_2d(&mesh.model),
            lights: None,
            probes: None,
            deferred: false,
            baked: false,
            ephemeral,
        };
        // What the key names now: memoized per key, hashed once per new mesh.
        let Some(content) = cache.queue.content_key(&keyed, signature, frame, &mut budget) else {
            emitters.push(emitter);
            continue;
        };
        emitter.content = Some(content);
        let current = cache
            .entries
            .get(&keyed.mesh_key)
            .is_some_and(|e| e.content == content);
        if !current {
            // A new mesh under this key: the probes of the same content on this object, if it
            // had them (the same mesh landed again), else traced below.
            report.refits += 1;
            // An ephemeral mesh's probes belong to its current pose only: it inherits none
            // traced elsewhere for the same object and content.
            let probes = if ephemeral {
                None
            } else {
                cache.queue.inherited_probes(mesh.object_key, content)
            };
            cache.entries.insert(
                keyed.mesh_key,
                CacheEntry {
                    content,
                    last_frame: frame,
                    lights: None,
                    probes,
                    pose: keyed.model,
                },
            );
        }
        if let Some(entry) = cache.entries.get_mut(&keyed.mesh_key) {
            entry.last_frame = frame;
            // Probes are world-space. An ephemeral mesh that moved no longer matches them.
            // A keyed mesh keeps the standing assumption that nearby occluders move with it.
            if ephemeral && bytemuck::bytes_of(&entry.pose) != bytemuck::bytes_of(&keyed.model) {
                entry.probes = None;
                entry.pose = keyed.model;
            }
            if entry.lights.is_none() {
                entry.lights = cache
                    .queue
                    .lights(content, &keyed, &params, !current, frame, &mut budget);
            }
            emitter.lights = entry.lights.clone();
            emitter.probes = entry.probes.clone();
        }
        emitters.push(emitter);
    }
    cache.prune_material_signatures();
    // Pass 2: lights not yet probed trace their distance probes against the scene around them
    // (one BVH for all of them). Static thereafter: the fit is in the mesh's own space and its
    // nearby occluders are taken to move with it.
    for e in &mut emitters {
        if e.key != 0 && !e.ephemeral && cache.queue.is_tracing(e.key) {
            e.deferred = true;
        }
    }
    let mut pending: Vec<usize> = (0..emitters.len())
        .filter(|&i| {
            let e = &emitters[i];
            e.lights.is_some() && e.probes.is_none() && !e.deferred
        })
        .collect();
    // The load frame (the first collection with emitters) traces every light; later frames at
    // most PROBE_LIGHTS_PER_FRAME, the emitters nearest the eye first (at least one). Emitters
    // past it wait for a later frame. A mesh submitted with key 0 has a fingerprint key for the
    // caches (ADR-5208) and still traces on its frame, outside the budget (integ-v047).
    if cache.primed {
        let eye = Vec3::from_array(view.eye);
        let distance = |i: usize| (emitters[i].model.w_axis.truncate() - eye).length_squared();
        pending.sort_by(|&a, &b| distance(a).total_cmp(&distance(b)).then(a.cmp(&b)));
        let mut lights = 0usize;
        pending.retain(|&i| {
            let e = &mut emitters[i];
            let n = e.lights.as_ref().map_or(0, |l| l.len());
            if e.ephemeral || e.key == 0 || lights == 0 || lights + n <= PROBE_LIGHTS_PER_FRAME {
                lights += n;
                true
            } else {
                e.deferred = true;
                false
            }
        });
    }
    // On the worker: the keyed emitters' batch, one at a time and within the budget; the
    // frame thread only gathers the triangles around them. A key-0 (ephemeral) mesh traces
    // here on its frame: its probes are for the pose of that frame, and a worker trace of a
    // mesh that moves every frame would land after the pose it was traced for had changed
    // (dark while moving, stale probes once it stops).
    let on_worker = |e: &Emitter| e.key != 0 && !e.ephemeral;
    if background && pending.iter().any(|&i| on_worker(&emitters[i])) {
        let keyed: Vec<usize> = pending
            .iter()
            .copied()
            .filter(|&i| on_worker(&emitters[i]))
            .collect();
        let held = budget.exhausted();
        let sent = if held || cache.queue.trace_in_flight() {
            if held {
                cache.queue.stats.budget_holds += keyed.len() as u64;
            }
            true
        } else {
            let _span = tracing::info_span!("emissive_probes_gather").entered();
            budget.admit(None);
            let mut spheres = Vec::new();
            let mut items = Vec::new();
            for &i in &keyed {
                let e = &emitters[i];
                let (Some(content), Some(lights)) = (e.content, e.lights.as_ref()) else {
                    continue;
                };
                let world: Vec<EmissiveLight> = lights.iter().map(|l| l.transformed(&e.model)).collect();
                spheres.extend(world.iter().map(|w| probe_sphere(w, cutoff_lux)));
                items.push(fit_queue::TraceItem {
                    key: e.key,
                    content,
                    reach: world
                        .iter()
                        .map(|w| PROBE_REACH.min(w.range(cutoff_lux)))
                        .collect(),
                    lights: world,
                });
            }
            // Only the copy of the reached meshes runs here; their world transform and
            // triangle filter run on the worker with the trace.
            let input = GatherInput::collect(meshes, &spheres, &mut cache.bounds);
            cache.queue.trace(items, spheres, input)
        };
        if sent {
            for &i in &keyed {
                emitters[i].deferred = true;
            }
            pending.retain(|&i| !on_worker(&emitters[i]));
        }
    }
    if !pending.is_empty() {
        let _span = tracing::info_span!("emissive_probes").entered();
        // (centre, reach, how far from the centre its rays may start).
        let spheres: Vec<(Vec3, f32, f32)> = pending
            .iter()
            .flat_map(|&i| {
                let e = &emitters[i];
                e.lights
                    .iter()
                    .flat_map(|l| l.iter())
                    .map(move |l| probe_sphere(&l.transformed(&e.model), cutoff_lux))
            })
            .collect();
        let gathered = Gathered::collect(meshes, &spheres, &mut cache.bounds);
        cache.queue.frame_thread_gathered += gathered.candidates() as u64;
        let bvh = Bvh::from_tris(gathered.near(&spheres));
        if debug_env() {
            eprintln!(
                "emissive probes: {} occluder triangles from {} meshes",
                bvh.tris.len(),
                meshes.len()
            );
        }
        for &i in &pending {
            let e = &mut emitters[i];
            let lights: &[EmissiveLight] = e.lights.as_ref().map_or(&[][..], |l| l.as_slice());
            let probes: Vec<Option<(Vec<f32>, f32)>> = lights
                .iter()
                .map(|l| {
                    let w = l.transformed(&e.model);
                    light_probe(&w, &bvh, PROBE_REACH.min(w.range(cutoff_lux)))
                })
                .collect();
            report.probes_traced += probes.len() as u32;
            cache.queue.stats.probes_traced += probes.len() as u64;
            cache.queue.frame_thread_probes += probes.len() as u64;
            // Diagnostic: REGIA_EMISSIVE_DEBUG=1 prints every light and its probe once.
            if debug_env() {
                for (l, p) in lights.iter().zip(&probes) {
                    let w = l.transformed(&e.model);
                    let summary = p.as_ref().map(|(p, open)| {
                        let near = p.iter().filter(|&&d| d < 0.3).count();
                        let min = p.iter().copied().fold(f32::MAX, f32::min);
                        format!("open {open:.2}, near(<0.3 m) {near}/{PROBE_TEXELS}, min {min:.3} m")
                    });
                    eprintln!(
                        "emissive light {:?} center {:?} axis {:?} area {:.4} flux {:.0}: {}",
                        w.shape,
                        w.centre,
                        w.axis_x,
                        w.area(),
                        w.flux(),
                        summary.unwrap_or_else(|| "buried".into())
                    );
                }
            }
            let probes = std::sync::Arc::new(probes);
            if let Some(entry) = cache.entries.get_mut(&e.key).filter(|_| e.key != 0) {
                entry.probes = Some(probes.clone());
            }
            e.probes = Some(probes);
        }
    }
    // A keyed emitter whose own lights or probes are pending draws with the lights its object
    // drew with the mesh it replaced, if any (a damage step, a LOD switch); landed emitters are
    // remembered for that and for a later mesh of the same content.
    let drawn: std::collections::HashSet<u64> =
        emitters.iter().filter(|e| e.key != 0).map(|e| e.key).collect();
    for e in &mut emitters {
        if e.key == 0 {
            continue;
        }
        if !e.deferred
            && let (Some(content), Some(lights), Some(probes)) = (e.content, &e.lights, &e.probes)
        {
            cache
                .queue
                .note_object(e.object, e.key, content, lights, probes, frame);
            continue;
        }
        if let Some((lights, probes)) = cache.queue.stand_in(e.object, &drawn, frame) {
            if e.deferred {
                report.probes_deferred += e.lights.as_ref().map_or(0, |l| l.len()) as u32;
            }
            e.lights = Some(lights);
            e.probes = Some(probes);
            e.deferred = false;
        } else if e.lights.is_none() {
            // Nothing to draw yet (the fit is pending).
            e.deferred = true;
        }
    }
    // ADR-1386: the emitters the frame's baked volume traced; their lights give way to it.
    if let Some(set) = cache.baked.clone() {
        let origin_offset = cache.origin_offset;
        for e in &mut emitters {
            if e.deferred || e.lights.as_ref().is_none_or(|l| l.is_empty()) {
                continue;
            }
            e.baked = meshes
                .get(e.mesh)
                .and_then(|mesh| drawn_emitter_bounds(&mut cache.emitter_bounds, mesh, e.content))
                .is_some_and(|mut b| {
                    // The bake's records stay in the volume's space; draws are local (spec 78 § 12.2).
                    b.min = std::array::from_fn(|axis| b.min[axis] + origin_offset[axis]);
                    b.max = std::array::from_fn(|axis| b.max[axis] + origin_offset[axis]);
                    set.holds(&b)
                });
        }
    }
    if cache.emitter_bounds.len() > CACHE_MAX_ENTRIES {
        cache.emitter_bounds.clear();
    }
    let mut world: Vec<WorldLight<'_>> = Vec::new();
    for e in &emitters {
        let lights: &[EmissiveLight] = e.lights.as_ref().map_or(&[][..], |l| l.as_slice());
        if e.deferred {
            report.probes_deferred += lights.len() as u32;
            continue;
        }
        for (k, light) in lights.iter().enumerate() {
            let (probe, open) = match e.probes.as_ref().and_then(|p| p.get(k)) {
                Some(Some((p, open))) => (Some(p.as_slice()), *open),
                Some(None) => {
                    report.buried += 1;
                    continue;
                }
                None => (None, 1.0),
            };
            let mut w = light.transformed(&e.model);
            w.radiance = w.radiance.map(|r| r * open);
            let range = w.range(cutoff_lux);
            world.push(WorldLight {
                light: w,
                range,
                probe,
                mesh: e.mesh,
                baked: e.baked,
            });
        }
    }
    // Authored rect and tube lights (ADR-2592) after the fitted ones.
    trace_authored_probes(cache, meshes, cutoff_lux, &mut report);
    for (a, (_, probe)) in cache.authored.iter().zip(&cache.authored_probes) {
        let (probe, open) = match probe {
            Some((p, open)) if a.cast_shadows => (Some(p.as_slice()), *open),
            None if a.cast_shadows => {
                report.buried += 1;
                continue;
            }
            _ => (None, 1.0),
        };
        let mut light = a.light;
        light.radiance = light.radiance.map(|r| r * open);
        let range = if a.range > 0.0 {
            a.range
        } else {
            light.range(cutoff_lux)
        };
        world.push(WorldLight {
            light,
            range,
            probe,
            mesh: usize::MAX,
            baked: false,
        });
    }
    cache
        .entries
        .retain(|_, e| frame.saturating_sub(e.last_frame) < CACHE_KEEP_FRAMES);
    if cache.entries.len() > CACHE_MAX_ENTRIES {
        let mut ages: Vec<(u64, u64)> = cache.entries.iter().map(|(k, e)| (e.last_frame, *k)).collect();
        ages.sort_unstable();
        for (_, key) in ages.iter().take(cache.entries.len() - CACHE_MAX_ENTRIES) {
            cache.entries.remove(key);
        }
    }
    cache.queue.prune(frame, CACHE_KEEP_FRAMES, CACHE_MAX_ENTRIES);
    if cache.bounds.len() > BOUNDS_MAX_ENTRIES {
        cache.bounds.clear();
    }
    cache.primed |= !emitters.is_empty();
    report.fitted = world.len() as u32;
    world.retain(|w| reaches_view(view, Vec3::from_array(w.light.centre), w.range));
    report.in_view = world.len() as u32;
    if world.len() > cap as usize {
        let eye = Vec3::from_array(view.eye);
        let score = |w: &WorldLight<'_>| {
            let d = (Vec3::from_array(w.light.centre) - eye)
                .length()
                .max(view.near)
                .max(1e-2);
            let projected = (w.range / d).min(4.0);
            w.light.peak_intensity() * projected * projected
        };
        // Each light scored once, not once per comparison (ADR-385); the same stable order. The
        // lights the bake holds go last: where the volume covers, they add nothing (ADR-1386).
        let mut keyed: Vec<_> = world.drain(..).map(|light| (score(&light), light)).collect();
        keyed.sort_by(|a, b| a.1.baked.cmp(&b.1.baked).then(b.0.total_cmp(&a.0)));
        world.extend(keyed.into_iter().take(cap as usize).map(|(_, light)| light));
    }
    report.uploaded = world.len() as u32;
    report.dropped = report.in_view - report.uploaded;
    // Probe tile k of the atlas belongs to uploaded light k (`cone_shadow.w` = k + 1).
    let mut atlas = vec![0.0f32; probe_atlas_len(world.len())];
    let mut records = Vec::with_capacity(world.len());
    for (k, w) in world.iter().enumerate() {
        if let Some(r) = cache.mesh_ranges.get_mut(w.mesh) {
            *r = r.max(w.range);
        }
        let mut record = gpu_record(&w.light, w.range);
        // Diagnostic: REGIA_EMISSIVE_NO_PROBES=1 shades every light unshadowed.
        let probe = w.probe.filter(|_| !no_probes_env());
        if let Some(probe) = probe {
            write_probe_tile(&mut atlas, k, probe);
            record[3][3] = k as f32 + 1.0;
        }
        if w.baked {
            record[3][3] = BAKED_EMITTER_TAG - record[3][3];
            report.baked += 1;
        }
        records.push(record);
    }
    report.cpu_ms = started.elapsed().as_secs_f32() * 1000.0;
    cache.queue.stats.frame_ms += f64::from(report.cpu_ms);
    (records, atlas, report)
}

/// A key for an authored light's placement, shape, range and shadow flag: its probe is traced again when any
/// of them changes.
fn authored_key(a: &crate::light_profiles::AreaLightInput) -> u64 {
    use std::hash::Hasher as _;
    let mut h = crate::key_hash::KeyHasher::default();
    let l = &a.light;
    for v in l
        .centre
        .iter()
        .chain(&l.axis_x)
        .chain(&l.axis_y)
        .chain(std::iter::once(&a.range))
    {
        h.write_u32(v.to_bits());
    }
    match l.shape {
        EmitterShape::Quad { two_sided, taper } => {
            h.write_u8(u8::from(two_sided));
            h.write_u32(taper.to_bits());
        }
        EmitterShape::Tube { radius } => {
            h.write_u8(2);
            h.write_u32(radius.to_bits());
        }
    }
    h.write_u8(u8::from(a.cast_shadows));
    h.finish()
}

/// Traces the distance probes of authored lights whose placement changed (one BVH for all of them); a
/// shadowless light keeps no probe.
fn trace_authored_probes(
    cache: &mut EmissiveCache,
    meshes: &[crate::FrameMesh<'_>],
    cutoff_lux: f32,
    report: &mut EmissiveLightReport,
) {
    let keys: Vec<u64> = cache.authored.iter().map(authored_key).collect();
    // A new slot holds no probe under a key no placement has, so a shadowed light there traces.
    cache.authored_probes.resize(keys.len(), (u64::MAX, None));
    let stale: Vec<usize> = (0..keys.len())
        .filter(|&i| cache.authored[i].cast_shadows && cache.authored_probes[i].0 != keys[i])
        .collect();
    if !stale.is_empty() {
        let _span = tracing::info_span!("authored_area_probes").entered();
        let spheres: Vec<(Vec3, f32, f32)> = stale
            .iter()
            .map(|&i| probe_sphere(&cache.authored[i].light, cutoff_lux))
            .collect();
        let bvh = Bvh::from_meshes(meshes, &spheres, &mut cache.bounds);
        for &i in &stale {
            let light = &cache.authored[i].light;
            let probe = light_probe(light, &bvh, PROBE_REACH.min(light.range(cutoff_lux)));
            report.probes_traced += 1;
            cache.authored_probes[i] = (keys[i], probe);
        }
    }
}

/// `REGIA_EMISSIVE_NO_PROBES` is set (a diagnostic), read once per process: the frame used to
/// read the environment, under its lock, once per uploaded light (ADR-385).
fn no_probes_env() -> bool {
    static NO_PROBES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NO_PROBES.get_or_init(|| regia_core::brand::env_var_os("REGIA_EMISSIVE_NO_PROBES").is_some())
}

/// `REGIA_EMISSIVE_DEBUG` is set (a diagnostic), read once per process.
fn debug_env() -> bool {
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEBUG.get_or_init(|| regia_core::brand::env_var_os("REGIA_EMISSIVE_DEBUG").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab_frame::LabMaterial;

    fn emissive_material(double_sided: bool) -> LabMaterial {
        LabMaterial {
            emissive: [1.0, 1.0, 1.0],
            emissive_intensity: 1000.0,
            double_sided,
            ..LabMaterial::EMPTY
        }
    }

    /// 65536² × 4 wrapped to 0 in `u32`, so an empty buffer passed the length guard and indexed.
    #[test]
    fn emissive_texel_with_wrapping_dimensions_is_white() {
        let t = LabTexture {
            width: 65_536,
            height: 65_536,
            rgba: Vec::new(),
        };
        assert_eq!(emissive_texel(Some(&t), [0.5, 0.5]), [1.0; 3]);
    }

    fn vertex(p: [f32; 3]) -> LabVertex {
        LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], [0.0, 0.0])
    }

    /// Closed axis-aligned box `lo..hi`, outward winding.
    fn box_mesh(lo: Vec3, hi: Vec3) -> (Vec<LabVertex>, Vec<u32>) {
        let (vs, is) = crate::box_mesh(((hi - lo) * 0.5).to_array());
        let off = (lo + hi) * 0.5;
        let vs = vs
            .into_iter()
            .map(|mut v| {
                v.position = (Vec3::from_array(v.position) + off).to_array();
                v
            })
            .collect();
        (vs, is)
    }

    fn total_flux(lights: &[EmissiveLight]) -> f32 {
        lights.iter().map(EmissiveLight::flux).sum()
    }

    #[test]
    fn a_thin_bar_becomes_one_tube_with_its_flux() {
        let (v, i) = box_mesh(Vec3::new(-0.5, -0.01, -0.01), Vec3::new(0.5, 0.01, 0.01));
        let lights = fit_mesh(&v, &i, &emissive_material(true), &FitParams::default());
        assert_eq!(lights.len(), 1, "{lights:?}");
        assert!(matches!(lights[0].shape, EmitterShape::Tube { .. }));
        // Emitting area: four long faces 1 × 0.02 and two 0.02² caps.
        let area = 4.0 * 0.02 + 2.0 * 0.0004;
        let expect = std::f32::consts::PI * 1000.0 * area;
        let got = total_flux(&lights);
        assert!((got - expect).abs() < 0.02 * expect, "{got} vs {expect}");
        assert!((Vec3::from_array(lights[0].axis_x).length() - 0.5).abs() < 1e-3);
    }

    #[test]
    fn a_closed_slab_emits_from_both_big_faces_one_sided() {
        let (v, i) = box_mesh(Vec3::new(-0.5, -0.01, -0.3), Vec3::new(0.5, 0.01, 0.3));
        let lights = fit_mesh(&v, &i, &emissive_material(true), &FitParams::default());
        let quads: Vec<_> = lights
            .iter()
            .filter(|l| matches!(l.shape, EmitterShape::Quad { two_sided: false, .. }))
            .collect();
        assert_eq!(quads.len(), 2, "{lights:?}");
        let normals: Vec<Vec3> = quads
            .iter()
            .map(|l| {
                Vec3::from_array(l.axis_x)
                    .cross(Vec3::from_array(l.axis_y))
                    .normalize()
            })
            .collect();
        assert!(
            normals.iter().any(|n| n.y > 0.99) && normals.iter().any(|n| n.y < -0.99),
            "{normals:?}"
        );
        // Each big face is 1 × 0.6 at the material's radiance.
        for q in quads {
            assert!((q.area() - 0.6).abs() < 1e-3, "{}", q.area());
            assert!((luminance(q.radiance) - 1000.0).abs() < 1.0);
        }
    }

    #[test]
    fn an_open_sheet_of_a_double_sided_material_is_two_sided() {
        let v = [
            [-1.0, 0.0, -1.0],
            [1.0, 0.0, -1.0],
            [1.0, 0.0, 1.0],
            [-1.0, 0.0, 1.0],
        ]
        .map(vertex)
        .to_vec();
        let i = vec![0, 2, 1, 0, 3, 2];
        let lights = fit_mesh(&v, &i, &emissive_material(true), &FitParams::default());
        assert_eq!(lights.len(), 1);
        assert!(
            matches!(lights[0].shape, EmitterShape::Quad { two_sided: true, taper } if taper.abs() < 1e-4),
            "{lights:?}"
        );
        assert!((lights[0].area() - 4.0).abs() < 1e-4, "{lights:?}");
        let one = fit_mesh(&v, &i, &emissive_material(false), &FitParams::default());
        assert!(matches!(
            one[0].shape,
            EmitterShape::Quad { two_sided: false, .. }
        ));
    }

    #[test]
    fn an_annulus_splits_into_sectors_that_keep_its_flux() {
        // Flat ring, inner radius 0.25, outer 0.65, 32 segments, facing +y (open, one-sided).
        let (r0, r1, n) = (0.25f32, 0.65f32, 32u32);
        let mut v = Vec::new();
        let mut idx = Vec::new();
        for k in 0..n {
            let a = k as f32 / n as f32 * std::f32::consts::TAU;
            v.push(vertex([r0 * a.cos(), 0.0, r0 * a.sin()]));
            v.push(vertex([r1 * a.cos(), 0.0, r1 * a.sin()]));
        }
        for k in 0..n {
            let (a0, a1) = (2 * k, 2 * k + 1);
            let (b0, b1) = (2 * ((k + 1) % n), 2 * ((k + 1) % n) + 1);
            idx.extend_from_slice(&[a0, b0, a1, a1, b0, b1]);
        }
        let mat = emissive_material(false);
        let lights = fit_mesh(&v, &idx, &mat, &FitParams::default());
        let summary: Vec<String> = lights
            .iter()
            .map(|l| format!("c {:?} area {:.3} taper {:?}", l.centre, l.area(), l.shape))
            .collect();
        assert!(
            lights.len() >= 2 && lights.len() <= 16,
            "{} lights: {summary:#?}",
            lights.len()
        );
        let polygon_area = 0.5 * n as f32 * (r1 * r1 - r0 * r0) * (std::f32::consts::TAU / n as f32).sin();
        let expect = std::f32::consts::PI * 1000.0 * polygon_area;
        let got = total_flux(&lights);
        assert!((got - expect).abs() < 0.03 * expect, "{got} vs {expect}");
        // No light covers the hole: every quad centre is outside the inner radius.
        for l in &lights {
            let c = Vec3::from_array(l.centre);
            assert!(Vec3::new(c.x, 0.0, c.z).length() > r0, "{l:?}");
        }
    }

    #[test]
    fn a_bent_bar_splits_into_straight_tubes() {
        // Quarter-circle arc of radius 1 made of 12 thin boxes' worth of quads (open tube).
        let (r, w, segs) = (1.0f32, 0.02f32, 24u32);
        let mut v = Vec::new();
        let mut idx = Vec::new();
        for k in 0..=segs {
            let a = k as f32 / segs as f32 * std::f32::consts::FRAC_PI_2;
            let c = Vec3::new(r * a.cos(), 0.0, r * a.sin());
            let radial = Vec3::new(a.cos(), 0.0, a.sin());
            for (dr, dy) in [(-w, -w), (w, -w), (w, w), (-w, w)] {
                v.push(vertex((c + radial * dr + Vec3::Y * dy).to_array()));
            }
        }
        for k in 0..segs {
            for s in 0..4u32 {
                let (a0, a1) = (4 * k + s, 4 * k + (s + 1) % 4);
                let (b0, b1) = (a0 + 4, a1 + 4);
                idx.extend_from_slice(&[a0, a1, b1, a0, b1, b0]);
            }
        }
        let lights = fit_mesh(&v, &idx, &emissive_material(true), &FitParams::default());
        assert!(lights.len() >= 2 && lights.len() <= 8, "{lights:?}");
        assert!(
            lights
                .iter()
                .all(|l| matches!(l.shape, EmitterShape::Tube { .. })),
            "{lights:?}"
        );
        let area = segs as f32 * 4.0 * 2.0 * w * (r * std::f32::consts::FRAC_PI_2 / segs as f32);
        let expect = std::f32::consts::PI * 1000.0 * area;
        let got = total_flux(&lights);
        assert!((got - expect).abs() < 0.05 * expect, "{got} vs {expect}");
    }

    #[test]
    fn non_emitters_and_blended_or_unlit_materials_fit_nothing() {
        let (v, i) = box_mesh(Vec3::splat(-0.5), Vec3::splat(0.5));
        assert!(fit_mesh(&v, &i, &LabMaterial::EMPTY, &FitParams::default()).is_empty());
        let mut m = emissive_material(true);
        m.unlit = true;
        assert!(fit_mesh(&v, &i, &m, &FitParams::default()).is_empty());
        let mut m = emissive_material(true);
        m.alpha_mode = ALPHA_ADDITIVE;
        assert!(fit_mesh(&v, &i, &m, &FitParams::default()).is_empty());
    }

    /// Review w11 #2: a sorted hair card is a blended layer whatever mode the host wrote, so an
    /// emissive one left `ALPHA_OPAQUE` fits no lights; hashed hair (alpha-tested) still does.
    #[test]
    fn sorted_hair_left_opaque_fits_no_lights() {
        let (v, i) = box_mesh(Vec3::splat(-0.5), Vec3::splat(0.5));
        let mut m = emissive_material(true);
        m.hair = crate::HairFactors {
            alpha: crate::HairAlpha::Sorted,
            ..crate::HairFactors::strands()
        };
        assert_eq!(m.alpha_mode, crate::ALPHA_OPAQUE);
        assert!(!material_emits(&m));
        assert!(fit_mesh(&v, &i, &m, &FitParams::default()).is_empty());
        m.hair.alpha = crate::HairAlpha::Hashed;
        assert!(material_emits(&m));
    }

    #[test]
    fn an_emissive_map_scales_and_masks_the_triangles() {
        // Two quads side by side; the map is black on the left half of UV space, white right.
        let mut v = Vec::new();
        for (x0, u) in [(-1.0f32, 0.25f32), (0.0, 0.75)] {
            for p in [
                [x0, 0.0, -0.5],
                [x0 + 1.0, 0.0, -0.5],
                [x0 + 1.0, 0.0, 0.5],
                [x0, 0.0, 0.5],
            ] {
                v.push(LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], [u, 0.5]));
            }
        }
        let i = vec![0, 2, 1, 0, 3, 2, 4, 6, 5, 4, 7, 6];
        let mut m = emissive_material(false);
        m.emissive_map = Some(crate::lab_frame::LabTexture {
            width: 2,
            height: 1,
            rgba: vec![0, 0, 0, 255, 255, 255, 255, 255],
        });
        let lights = fit_mesh(&v, &i, &m, &FitParams::default());
        assert_eq!(lights.len(), 1, "{lights:?}");
        assert!(Vec3::from_array(lights[0].centre).x > 0.4, "{lights:?}");
    }

    #[test]
    fn transform_moves_scales_and_keeps_radiance() {
        let l = EmissiveLight {
            centre: [0.0, 0.0, 0.0],
            axis_x: [0.5, 0.0, 0.0],
            axis_y: [0.0, 0.0, 0.1],
            radiance: [10.0; 3],
            anchor: [0.0, 0.0],
            shape: EmitterShape::Quad {
                two_sided: false,
                taper: 0.0,
            },
        };
        let m = Mat4::from_scale_rotation_translation(
            Vec3::splat(2.0),
            regia_core::glam::Quat::IDENTITY,
            Vec3::new(1.0, 2.0, 3.0),
        );
        let t = l.transformed(&m);
        assert_eq!(t.centre, [1.0, 2.0, 3.0]);
        assert!((t.area() - 4.0 * l.area()).abs() < 1e-5);
        assert_eq!(t.radiance, l.radiance);
        assert_eq!(
            probe_origins(&l).len(),
            10,
            "a quad probes from the anchor plus a 3x3 grid"
        );
        let tube = EmissiveLight {
            shape: EmitterShape::Tube { radius: 0.01 },
            ..l
        };
        let tt = tube.transformed(&m);
        assert!(matches!(tt.shape, EmitterShape::Tube { radius } if (radius - 0.02).abs() < 1e-6));
        assert_eq!(
            probe_origins(&tube).len(),
            5,
            "a tube probes from the anchor plus four axis stations"
        );
    }

    #[test]
    fn irradiance_falls_off_with_the_inverse_square_far_away_and_saturates_close() {
        // One-sided 0.2 × 0.2 quad facing down at height h over an upward receiver.
        let l = EmissiveLight {
            centre: [0.0, 0.0, 0.0],
            axis_x: [0.1, 0.0, 0.0],
            axis_y: [0.0, 0.0, 0.1],
            radiance: [1000.0; 3],
            // x × z = −y: faces down.
            anchor: [0.0, 0.0],
            shape: EmitterShape::Quad {
                two_sided: false,
                taper: 0.0,
            },
        };
        let at = |h: f32| irradiance_at(&l, Vec3::new(0.0, -h, 0.0), Vec3::Y);
        // Far: E ≈ L · A / h².
        let far = at(5.0);
        let expect = 1000.0 * 0.04 / 25.0;
        assert!((far - expect).abs() < 0.01 * expect, "{far} vs {expect}");
        // Touching: E → π L (the quad fills the hemisphere's centre).
        assert!(at(0.001) > 0.95 * std::f32::consts::PI * 1000.0);
        // Above the quad (behind its emitting face): nothing.
        assert_eq!(irradiance_at(&l, Vec3::new(0.0, 1.0, 0.0), -Vec3::Y), 0.0);
        // The range ends where the peak illuminance falls to the cut.
        let r = l.range(0.4);
        assert!(r >= 10.0 && r < 10.0 + 2.0 * l.half_diagonal(), "{r}");
    }

    #[test]
    fn a_tube_lights_like_its_silhouette_from_every_side() {
        let l = EmissiveLight {
            centre: [0.0; 3],
            axis_x: [0.5, 0.0, 0.0],
            axis_y: [0.0; 3],
            radiance: [1000.0; 3],
            anchor: [0.0, 0.0],
            shape: EmitterShape::Tube { radius: 0.01 },
        };
        let d = 3.0;
        let expect = 1000.0 * (1.0 * 0.02) / (d * d);
        for dir in [Vec3::Y, -Vec3::Y, Vec3::Z, Vec3::new(0.0, 0.6, 0.8)] {
            let e = irradiance_at(&l, dir * d, -dir);
            assert!((e - expect).abs() < 0.03 * expect, "{dir}: {e} vs {expect}");
        }
    }

    #[test]
    fn octahedral_directions_round_trip() {
        for k in 0..200 {
            let a = k as f32 * 2.399_963;
            let z = 1.0 - 2.0 * (k as f32 + 0.5) / 200.0;
            let r = (1.0 - z * z).sqrt();
            let d = Vec3::new(r * a.cos(), z, r * a.sin());
            let back = oct_decode(oct_encode(d));
            assert!(back.dot(d) > 0.9999, "{d} -> {back}");
        }
    }

    #[test]
    fn a_probe_sees_the_plate_between_a_light_and_the_ground() {
        // A light 1 m up, a 2 × 2 plate 0.5 m up (a hull), the ground below at y = 0.
        let light = EmissiveLight {
            centre: [0.0, 1.0, 0.0],
            axis_x: [0.1, 0.0, 0.0],
            axis_y: [0.0, 0.0, 0.1],
            radiance: [1000.0; 3],
            anchor: [0.0, 0.0],
            shape: EmitterShape::Quad {
                two_sided: true,
                taper: 0.0,
            },
        };
        let quad = |y: f32, h: f32| {
            let [a, b, c, d] = [[-h, y, -h], [h, y, -h], [h, y, h], [-h, y, h]].map(Vec3::from_array);
            [[a, b, c], [a, c, d]]
        };
        let mut tris = quad(0.5, 1.0).to_vec();
        tris.extend(quad(0.0, 50.0));
        let bvh = Bvh::from_tris(tris);
        let (probe, open) = light_probe(&light, &bvh, 12.0).expect("not buried");
        assert!(open > 0.95, "{open}");
        let at = |d: Vec3| {
            let uv = oct_encode(d.normalize());
            let res = PROBE_RES as usize;
            let (x, y) = ((uv[0] * res as f32) as usize, (uv[1] * res as f32) as usize);
            probe[y.min(res - 1) * res + x.min(res - 1)]
        };
        // Straight down: the plate at 0.5 m, not the ground at 1 m (texel centres are a few
        // degrees off the axis).
        assert!((at(-Vec3::Y) - 0.5).abs() < 0.05, "{}", at(-Vec3::Y));
        // Down and far out (past the plate's edge): the ground.
        let out = Vec3::new(3.0, -1.0, 0.0);
        assert!((at(out) - out.length()).abs() < 0.4, "{}", at(out));
        // Up: nothing within reach.
        assert!(at(Vec3::Y) >= 12.0 - 1e-3);
        // A light pressed onto the plate's top face, emitting down into it, is buried.
        let buried = EmissiveLight {
            centre: [0.0, 0.505, 0.0],
            axis_x: [0.1, 0.0, 0.0],
            axis_y: [0.0, 0.0, 0.1],
            shape: EmitterShape::Quad {
                two_sided: false,
                taper: 0.0,
            },
            ..light
        };
        assert!(light_probe(&buried, &bvh, 12.0).is_none());
        // The atlas tile layout: tile 17 starts one probe row down, one probe right.
        let mut atlas = vec![0.0; probe_atlas_len(18)];
        write_probe_tile(&mut atlas, 17, &probe);
        let width = (PROBE_ATLAS_COLUMNS * PROBE_RES) as usize;
        let res = PROBE_RES as usize;
        assert_eq!(atlas[res * width + res], probe[0]);
        assert_eq!(
            atlas[(2 * res - 1) * width + 2 * res - 1],
            probe[PROBE_TEXELS - 1]
        );
    }

    #[test]
    fn a_half_covered_emitter_keeps_its_open_share() {
        // A 1 × 1 m pad facing down 0.15 m over the ground, a 0.5 m wide bar 5 mm under one half
        // of it (the frame under DO27's hover pads): not buried, about half its light escapes,
        // and the probe still sees the ground beside the bar.
        let light = EmissiveLight {
            centre: [0.0, 0.15, 0.0],
            axis_x: [0.5, 0.0, 0.0],
            axis_y: [0.0, 0.0, 0.5],
            radiance: [1000.0; 3],
            anchor: [0.0, 0.0],
            shape: EmitterShape::Quad {
                two_sided: false,
                taper: 0.0,
            },
        };
        let quad = |lo: [f32; 2], hi: [f32; 2], y: f32| {
            let [a, b, c, d] = [
                [lo[0], y, lo[1]],
                [hi[0], y, lo[1]],
                [hi[0], y, hi[1]],
                [lo[0], y, hi[1]],
            ]
            .map(Vec3::from_array);
            [[a, b, c], [a, c, d]]
        };
        let mut tris = quad([-0.6, -0.6], [0.0, 0.6], 0.145).to_vec();
        tris.extend(quad([-50.0, -50.0], [50.0, 50.0], 0.0));
        let bvh = Bvh::from_tris(tris);
        let (probe, open) = light_probe(&light, &bvh, 12.0).expect("half open is not buried");
        assert!(open > 0.3 && open < 0.75, "{open}");
        // Straight down from the anchor (over the bar's edge): the far hit is the ground.
        let uv = oct_encode(-Vec3::Y);
        let res = PROBE_RES as usize;
        let (x, y) = ((uv[0] * res as f32) as usize, (uv[1] * res as f32) as usize);
        assert!(probe[y.min(res - 1) * res + x.min(res - 1)] > 0.12, "{probe:?}");
    }

    #[test]
    fn the_frame_collection_caches_fits_and_honours_the_budget() {
        let mat = emissive_material(true);
        let ground = LabMaterial::EMPTY;
        let (bar_v, bar_i) = box_mesh(Vec3::new(-0.5, -0.01, -0.01), Vec3::new(0.5, 0.01, 0.01));
        let (floor_v, floor_i) = box_mesh(Vec3::new(-20.0, -0.1, -20.0), Vec3::new(20.0, 0.0, 20.0));
        let mesh = |key: u64, v: &'static [LabVertex], i: &'static [u32], m: &'static LabMaterial, x: f32| {
            crate::FrameMesh {
                is_static: false,
                mesh_key: key,
                material_key: key,
                object_key: key,
                vertices: v,
                indices: i,
                material: m,
                model: Mat4::from_translation(Vec3::new(x, 1.0, 0.0)).to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: crate::ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        };
        let leak = |v: Vec<LabVertex>, i: Vec<u32>| -> (&'static [LabVertex], &'static [u32]) {
            (Box::leak(v.into_boxed_slice()), Box::leak(i.into_boxed_slice()))
        };
        let (bv, bi) = leak(bar_v, bar_i);
        let (fv, fi) = leak(floor_v, floor_i);
        let mat: &'static LabMaterial = Box::leak(Box::new(mat));
        let ground: &'static LabMaterial = Box::leak(Box::new(ground));
        let mut meshes = vec![
            mesh(1, bv, bi, mat, -2.0),
            mesh(2, bv, bi, mat, 0.0),
            mesh(3, bv, bi, mat, 2.0),
        ];
        meshes.push(crate::FrameMesh {
            model: Mat4::IDENTITY.to_cols_array_2d(),
            ..mesh(4, fv, fi, ground, 0.0)
        });
        let view = crate::light_budget::LightView::new(
            [0.0, 2.0, 8.0],
            [0.0, 1.0, 0.0],
            1.0,
            16.0 / 9.0,
            0.1,
            100.0,
        );
        let mut cache = EmissiveCache::default();
        let (records, atlas, report) = collect_frame_lights(&mut cache, &meshes, &view, 2, 5.0);
        assert_eq!(
            (report.meshes, report.fitted, report.in_view),
            (3, 3, 3),
            "{report:?}"
        );
        assert_eq!(
            (
                report.uploaded,
                report.dropped,
                report.refits,
                report.probes_traced
            ),
            (2, 1, 3, 3)
        );
        assert_eq!(records.len(), 2);
        assert_eq!(atlas.len(), probe_atlas_len(2));
        // Each uploaded light points at its own probe tile.
        assert_eq!(records[0][3][3], 1.0);
        assert_eq!(records[1][3][3], 2.0);
        // Second frame: nothing refitted or retraced.
        let (_, _, again) = collect_frame_lights(&mut cache, &meshes, &view, 2, 5.0);
        assert_eq!((again.refits, again.probes_traced), (0, 0));
        // The probes' hook: per mesh, the range of its uploaded light; 0 for the bar over the
        // budget and the floor (not an emitter).
        let ranges = cache.mesh_ranges().to_vec();
        assert_eq!(ranges.len(), 4);
        assert_eq!(ranges.iter().filter(|&&r| r > 0.0).count(), 2, "{ranges:?}");
        assert_eq!(ranges[3], 0.0);
        let mut uploaded: Vec<f32> = records.iter().map(|r| r[2][3]).collect();
        let mut nonzero: Vec<f32> = ranges.iter().copied().filter(|&r| r > 0.0).collect();
        uploaded.sort_by(f32::total_cmp);
        nonzero.sort_by(f32::total_cmp);
        assert_eq!(uploaded, nonzero);
        // A light behind the camera is culled before the budget; budget 0 is off.
        let behind = crate::light_budget::LightView::new(
            [0.0, 2.0, 8.0],
            [0.0, 2.0, 20.0],
            1.0,
            16.0 / 9.0,
            0.1,
            100.0,
        );
        let (_, _, culled) = collect_frame_lights(&mut cache, &meshes, &behind, 8, 5.0);
        assert!(culled.in_view < 3, "{culled:?}");
        let (off, _, off_report) = collect_frame_lights(&mut cache, &meshes, &view, 0, 5.0);
        assert!(off.is_empty() && off_report.cap == 0);
        assert!(cache.mesh_ranges().iter().all(|&r| r == 0.0));
    }

    /// `n × n` cells (two triangles each) over `[-half, half]²` at y = 0, facing up.
    fn grid(n: u32, half: f32) -> (Vec<LabVertex>, Vec<u32>) {
        let mut vertices = Vec::new();
        for z in 0..=n {
            for x in 0..=n {
                let (u, v) = (x as f32 / n as f32, z as f32 / n as f32);
                vertices.push(LabVertex::from_pos_normal_uv(
                    [(u * 2.0 - 1.0) * half, 0.0, (v * 2.0 - 1.0) * half],
                    [0.0, 1.0, 0.0],
                    [u, v],
                ));
            }
        }
        let mut indices = Vec::new();
        for z in 0..n {
            for x in 0..n {
                let i = z * (n + 1) + x;
                indices.extend_from_slice(&[i, i + n + 1, i + 1, i + 1, i + n + 1, i + n + 2]);
            }
        }
        (vertices, indices)
    }

    fn frame_mesh(
        key: u64,
        mesh: &(Vec<LabVertex>, Vec<u32>),
        material: &'static LabMaterial,
        at: Vec3,
    ) -> crate::FrameMesh<'static> {
        let vertices: &'static [LabVertex] = Box::leak(mesh.0.clone().into_boxed_slice());
        let indices: &'static [u32] = Box::leak(mesh.1.clone().into_boxed_slice());
        crate::FrameMesh {
            is_static: false,
            mesh_key: key,
            material_key: key,
            object_key: key,
            vertices,
            indices,
            material,
            model: Mat4::from_translation(at).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: crate::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }
    }

    /// v0.1.7: the probe BVH keeps only the triangles a probe ray can reach. Against every
    /// triangle of the reached meshes (the v0.1.6 gather) each probe is bit for bit the same,
    /// from a fraction of the triangles.
    #[test]
    fn probe_triangles_out_of_reach_are_dropped_without_changing_a_probe() {
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(false)));
        let ground: &'static LabMaterial = Box::leak(Box::new(LabMaterial::EMPTY));
        let bar = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.5, 0.05, 0.05));
        // 200 m of ground in 0.5 m cells (320 000 triangles), a wall beside the bar.
        let floor = grid(400, 100.0);
        let wall = box_mesh(Vec3::new(-2.0, 0.0, -0.1), Vec3::new(2.0, 3.0, 0.1));
        let meshes = vec![
            frame_mesh(1, &bar, mat, Vec3::new(0.0, 0.6, 0.0)),
            frame_mesh(2, &floor, ground, Vec3::ZERO),
            frame_mesh(3, &wall, ground, Vec3::new(0.0, 0.0, -1.0)),
        ];
        let model = Mat4::from_cols_array_2d(&meshes[0].model);
        let lights: Vec<EmissiveLight> =
            fit_mesh(bar.0.as_slice(), bar.1.as_slice(), mat, &FitParams::default())
                .iter()
                .map(|l| l.transformed(&model))
                .collect();
        assert!(!lights.is_empty());
        let spheres: Vec<(Vec3, f32, f32)> = lights.iter().map(|l| probe_sphere(l, 5.0)).collect();
        let culled = Bvh::from_meshes(&meshes, &spheres, &mut HashMap::new());
        let mut all = Vec::new();
        for mesh in &meshes {
            let m = Mat4::from_cols_array_2d(&mesh.model);
            let p = |i: u32| m.transform_point3(Vec3::from_array(mesh.vertices[i as usize].position));
            for t in mesh.indices.as_chunks::<3>().0 {
                all.push([p(t[0]), p(t[1]), p(t[2])]);
            }
        }
        let reference = Bvh::from_tris(all);
        eprintln!(
            "probe triangles: {} of {}",
            culled.tris.len(),
            reference.tris.len()
        );
        assert!(
            culled.tris.len() * 20 < reference.tris.len(),
            "{} of {}",
            culled.tris.len(),
            reference.tris.len()
        );
        for (light, &(_, reach, _)) in lights.iter().zip(&spheres) {
            assert_eq!(
                light_probe(light, &culled, reach),
                light_probe(light, &reference, reach)
            );
        }
    }

    /// v0.1.7 CPU cost of the emissive paths (tester logs: emissive_lights / emissive_probes
    /// up to 1.8 s in a frame as units came into view). Counters, not a timing gate: past the
    /// load frame a burst of new emitters traces at most PROBE_LIGHTS_PER_FRAME probes a frame
    /// until all are traced, and a unit back after 1000 frames away (the old cache kept 300)
    /// is neither fitted nor traced again. The frame times print for the report.
    #[test]
    fn a_burst_of_emitters_traces_a_bounded_number_of_probes_per_frame() {
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(false)));
        let ground: &'static LabMaterial = Box::leak(Box::new(LabMaterial::EMPTY));
        let bar = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.5, 0.05, 0.05));
        let floor = grid(200, 100.0);
        let base = vec![
            frame_mesh(1, &floor, ground, Vec3::ZERO),
            frame_mesh(2, &bar, mat, Vec3::new(0.0, 0.6, 0.0)),
        ];
        // 150 units, each its own bar key, spread over 60 m.
        let units: Vec<crate::FrameMesh<'static>> = (0..150u64)
            .map(|i| {
                let at = Vec3::new((i % 15) as f32 * 4.0 - 30.0, 0.6, (i / 15) as f32 * 4.0 - 20.0);
                frame_mesh(100 + i, &bar, mat, at)
            })
            .collect();
        let view = crate::light_budget::LightView::new(
            [0.0, 8.0, 30.0],
            [0.0, 0.0, 0.0],
            1.0,
            16.0 / 9.0,
            0.1,
            500.0,
        );
        let per = fit_mesh(bar.0.as_slice(), bar.1.as_slice(), mat, &FitParams::default()).len() as u32;
        assert!(per > 0);
        let mut cache = EmissiveCache::default();
        let (_, _, load) = collect_frame_lights(&mut cache, &base, &view, 256, 5.0);
        assert_eq!(load.probes_traced, per, "{load:?}");
        let all: Vec<crate::FrameMesh<'static>> = base.iter().chain(&units).copied().collect();
        let mut traced = Vec::new();
        let mut ms = Vec::new();
        for _ in 0..32 {
            let (_, _, r) = collect_frame_lights(&mut cache, &all, &view, 256, 5.0);
            traced.push(r.probes_traced);
            ms.push(r.cpu_ms);
            if r.probes_deferred == 0 {
                break;
            }
        }
        eprintln!("burst of 150 emitters: probes traced per frame {traced:?}, cpu ms {ms:?}");
        assert!(
            traced.iter().all(|&n| n as usize <= PROBE_LIGHTS_PER_FRAME),
            "{traced:?}"
        );
        assert_eq!(traced.iter().sum::<u32>(), 150 * per, "{traced:?}");
        // The units leave the view for 1000 frames and come back.
        for _ in 0..1000 {
            collect_frame_lights(&mut cache, &base, &view, 256, 5.0);
        }
        let (_, _, back) = collect_frame_lights(&mut cache, &all, &view, 256, 5.0);
        eprintln!("back after 1000 frames: {back:?}");
        assert_eq!(
            (back.refits, back.probes_traced, back.probes_deferred),
            (0, 0, 0),
            "{back:?}"
        );
    }

    /// A quad's probe leaves from ten points; a tube's from five (centre plus four along the axis).
    #[test]
    fn probe_origins_are_ten_on_a_quad_and_five_on_a_tube() {
        let quad = EmissiveLight {
            centre: [0.0, 1.0, 0.0],
            axis_x: [0.4, 0.0, 0.0],
            axis_y: [0.0, 0.0, 0.2],
            radiance: [1.0, 1.0, 1.0],
            anchor: [0.0, 0.0],
            shape: EmitterShape::Quad {
                two_sided: false,
                taper: 0.0,
            },
        };
        let tube = EmissiveLight {
            centre: [0.0, 1.0, 0.0],
            axis_x: [0.5, 0.0, 0.0],
            axis_y: [0.0, 0.0, 0.0],
            radiance: [1.0, 1.0, 1.0],
            anchor: [0.0, 0.0],
            shape: EmitterShape::Tube { radius: 0.02 },
        };
        assert_eq!(probe_origins(&quad).len(), 10);
        assert_eq!(probe_origins(&tube).len(), 5);
    }

    /// Past the load frame one new mesh is admitted whole, even past [`PROBE_LIGHTS_PER_FRAME`],
    /// and a key of 0 is traced on the same frame. A farther keyed mesh waits.
    #[test]
    fn one_new_mesh_is_traced_whole_past_the_per_frame_budget() {
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(false)));
        let bar = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.5, 0.05, 0.05));
        let mut many_v = Vec::new();
        let mut many_i = Vec::new();
        for i in 0..20 {
            let x = i as f32 * 5.0;
            let (v, ix) = box_mesh(Vec3::new(x - 0.5, -0.05, -0.05), Vec3::new(x + 0.5, 0.05, 0.05));
            let base = many_v.len() as u32;
            many_v.extend(v);
            many_i.extend(ix.into_iter().map(|k| k + base));
        }
        let many = (many_v, many_i);
        let fitted = fit_mesh(many.0.as_slice(), many.1.as_slice(), mat, &FitParams::default());
        assert!(
            fitted.len() > PROBE_LIGHTS_PER_FRAME,
            "the mesh must hold more than one frame's budget, got {}",
            fitted.len()
        );
        assert!(fitted.len() <= FitParams::default().max_lights_per_mesh);
        let view = crate::light_budget::LightView::new(
            [0.0, 8.0, 30.0],
            [0.0, 0.0, 0.0],
            1.0,
            16.0 / 9.0,
            0.1,
            500.0,
        );
        let mut cache = EmissiveCache::default();
        let primer = [frame_mesh(2, &bar, mat, Vec3::new(0.0, 0.6, 0.0))];
        let (_, _, load) = collect_frame_lights(&mut cache, &primer, &view, 256, 5.0);
        assert!(
            load.probes_traced > 0,
            "the load frame traces the primer: {load:?}"
        );
        assert!(cache.primed);
        cache.queue.set_background(false);
        let frame = [
            frame_mesh(50, &many, mat, Vec3::ZERO),
            frame_mesh(51, &bar, mat, Vec3::new(0.0, 0.6, 200.0)),
            frame_mesh(0, &bar, mat, Vec3::new(0.0, 0.6, 400.0)),
        ];
        let (_, _, report) = collect_frame_lights(&mut cache, &frame, &view, 256, 5.0);
        assert_eq!(
            report.probes_traced as usize,
            fitted.len() + 1,
            "the near mesh and the key-0 mesh trace whole: {report:?}"
        );
        assert_eq!(
            report.probes_deferred, 1,
            "the farther keyed mesh waits: {report:?}"
        );
    }

    #[test]
    fn gpu_record_round_trips_the_shape_code() {
        let l = EmissiveLight {
            centre: [1.0, 2.0, 3.0],
            axis_x: [0.5, 0.0, 0.0],
            axis_y: [0.0, 0.0, 0.1],
            radiance: [5.0, 6.0, 7.0],
            anchor: [0.0, 0.0],
            shape: EmitterShape::Quad {
                two_sided: true,
                taper: -0.5,
            },
        };
        let r = gpu_record(&l, 4.0);
        let code = r[0][3];
        assert!((5.0..6.0).contains(&code));
        let taper = (code - 5.0) / 0.96 * 2.0 - 1.0;
        assert!((taper + 0.5).abs() < 1e-5);
        assert_eq!(r[2][3], 4.0);
        for anchor in [[0.0, 0.0], [-1.0, 1.0], [0.5, -0.25]] {
            let back = decode_anchor(encode_anchor(anchor));
            assert!(
                (back[0] - anchor[0]).abs() < 0.005 && (back[1] - anchor[1]).abs() < 0.005,
                "{back:?}"
            );
        }
        let tube = gpu_record(
            &EmissiveLight {
                shape: EmitterShape::Tube { radius: 0.02 },
                ..l
            },
            1.0,
        );
        assert!(tube[0][3] >= 6.0 && tube[0][3] < 7.0 && tube[3][0] == 0.02);
    }

    fn hitch_view() -> crate::light_budget::LightView {
        crate::light_budget::LightView::new([0.0, 2.0, 6.0], [0.0, 0.6, 0.0], 1.0, 16.0 / 9.0, 0.1, 100.0)
    }

    /// v0.1.18 (DO27 combat hitches, Regia Engine v0.1.16): the same mesh content landing under 100
    /// fresh keys (new `Arc` addresses: a damage step that lands the intact mesh again) on one
    /// object fits once and traces its probes once, and every frame draws the same lights.
    /// v0.1.17 fitted and traced it 100 times. With and without the worker.
    #[test]
    fn the_same_content_under_100_fresh_keys_fits_once_and_traces_once() {
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(false)));
        let ground: &'static LabMaterial = Box::leak(Box::new(LabMaterial::EMPTY));
        let bar = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.5, 0.05, 0.05));
        let floor = grid(20, 10.0);
        let per = fit_mesh(bar.0.as_slice(), bar.1.as_slice(), mat, &FitParams::default()).len() as u32;
        assert!(per > 0);
        for background in [false, true] {
            let mut cache = EmissiveCache::default();
            cache.set_background(background);
            let floor_mesh = frame_mesh(1, &floor, ground, Vec3::ZERO);
            let mut traced = 0;
            let mut first: Option<Vec<[[f32; 4]; 4]>> = None;
            for i in 0..100u64 {
                // A fresh copy (new buffers) under a fresh key, on object 7.
                let unit = crate::FrameMesh {
                    object_key: 7,
                    ..frame_mesh(1000 + i, &bar, mat, Vec3::new(0.0, 0.6, 0.0))
                };
                let (records, _, report) =
                    collect_frame_lights(&mut cache, &[floor_mesh, unit], &hitch_view(), 16, 5.0);
                traced += report.probes_traced;
                assert!(
                    !records.is_empty(),
                    "background {background}, frame {i}: {report:?}"
                );
                let first = first.get_or_insert_with(|| records.clone());
                assert_eq!(&records, first, "background {background}, frame {i}");
            }
            let stats = cache.stats();
            eprintln!("background {background}: {stats:?}");
            assert_eq!(stats.fits_done, 1, "{stats:?}");
            assert_eq!(traced, per, "{stats:?}");
            assert_eq!(
                (stats.cache_hits, stats.probe_hits, stats.hashes),
                (99, 99, 100),
                "{stats:?}"
            );
        }
    }

    /// v0.1.18: a key that names other content (an address reused by another mesh with the same
    /// counts) is fitted again; v0.1.17 compared only the counts and kept the old fit.
    #[test]
    fn a_reused_key_with_other_content_is_fitted_again() {
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(false)));
        let short = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.5, 0.05, 0.05));
        let long = box_mesh(Vec3::new(-0.9, -0.05, -0.05), Vec3::new(0.9, 0.05, 0.05));
        assert_eq!((short.0.len(), short.1.len()), (long.0.len(), long.1.len()));
        let mut cache = EmissiveCache::default();
        let at = Vec3::new(0.0, 0.6, 0.0);
        let (a, _, _) = collect_frame_lights(
            &mut cache,
            &[frame_mesh(5, &short, mat, at)],
            &hitch_view(),
            16,
            5.0,
        );
        let (b, _, report) = collect_frame_lights(
            &mut cache,
            &[frame_mesh(5, &long, mat, at)],
            &hitch_view(),
            16,
            5.0,
        );
        assert_eq!((report.refits, cache.stats().fits_done), (1, 2), "{report:?}");
        assert_ne!(a, b);
        // Same content again under that key: nothing fitted.
        let (c, _, report) = collect_frame_lights(
            &mut cache,
            &[frame_mesh(5, &short, mat, at)],
            &hitch_view(),
            16,
            5.0,
        );
        assert_eq!((report.refits, cache.stats().fits_done), (1, 2), "{report:?}");
        assert_eq!(a, c);
    }

    /// Runs collections of `meshes` until `done` holds for the records or five seconds pass
    /// (the worker), returning every frame's records and CPU time.
    /// Frame-thread work of one collection: (fits, distance probes traced).
    type FrameWork = (u64, u64);

    /// Collects frames until `done` holds for the records (at most 16), each frame landing
    /// the worker's results once they are ready (`land_waits`: no clock, no sleep). Returns each
    /// frame's records with the frame thread's own work in it.
    fn until(
        cache: &mut EmissiveCache,
        meshes: &[crate::FrameMesh<'_>],
        done: impl Fn(&[[[f32; 4]; 4]]) -> bool,
    ) -> Vec<(Vec<[[f32; 4]; 4]>, FrameWork)> {
        cache.queue.land_waits = true;
        let mut frames = Vec::new();
        for _ in 0..16 {
            let (records, work) = collect_counted(cache, meshes);
            let finished = done(&records);
            frames.push((records, work));
            if finished {
                break;
            }
        }
        frames
    }

    /// One collection and the frame thread's own fits and probe traces in it.
    fn collect_counted(
        cache: &mut EmissiveCache,
        meshes: &[crate::FrameMesh<'_>],
    ) -> (Vec<[[f32; 4]; 4]>, FrameWork) {
        let before = (cache.queue.frame_thread_fits, cache.queue.frame_thread_probes);
        let (records, _, _) = collect_frame_lights(cache, meshes, &hitch_view(), 64, 5.0);
        let after = (cache.queue.frame_thread_fits, cache.queue.frame_thread_probes);
        (records, (after.0 - before.0, after.1 - before.1))
    }

    /// A rigid emissive mesh submitted with `mesh_key` 0 and the same vertices every frame is
    /// fingerprinted like a keyed mesh: the second collection fits nothing and traces no probe
    /// on the frame thread. The first collection still does both (there is no cache yet).
    #[test]
    fn an_unchanged_key_0_mesh_does_not_refit_or_reprobe() {
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(false)));
        let bar = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.5, 0.05, 0.05));
        let mesh = frame_mesh(0, &bar, mat, Vec3::new(0.0, 0.6, 0.0));
        let mut cache = EmissiveCache::default();
        let (first_lights, first_work) = collect_counted(&mut cache, &[mesh]);
        let hashes_after_first = cache.stats().hashes;
        let (second_lights, second_work) = collect_counted(&mut cache, &[mesh]);
        assert!(!first_lights.is_empty(), "the bar must fit at least one light");
        assert!(
            first_work.0 >= 1 && first_work.1 >= 1,
            "the first collection fits and traces on the frame thread: {first_work:?}"
        );
        assert_eq!(
            second_work,
            (0, 0),
            "an unchanged key-0 mesh fitted or traced again: first {first_work:?}, \
             hashes {hashes_after_first} -> {}",
            cache.stats().hashes
        );
        assert_eq!(
            second_lights, first_lights,
            "the cached fit must draw the same lights"
        );
        assert_eq!(
            cache.stats().hashes,
            hashes_after_first,
            "the second collection hashed the mesh again"
        );
    }

    /// A key-0 occluder edited in place (same buffer, same length) is measured again: it now
    /// reaches the probe sphere it was far from, so its triangles are gathered.
    #[test]
    fn a_key_0_occluder_edited_in_place_reaches_the_probe() {
        let ground: &'static LabMaterial = Box::leak(Box::new(LabMaterial::EMPTY));
        let template = box_mesh(Vec3::new(10.0, 0.0, 0.0), Vec3::new(11.0, 1.0, 1.0));
        let (mut vertices, indices) = box_mesh(Vec3::new(10.0, 0.0, 0.0), Vec3::new(11.0, 1.0, 1.0));
        let probes = [(Vec3::ZERO, 1.0f32, 0.0f32)];
        let mut bounds = HashMap::new();
        let far = crate::FrameMesh {
            vertices: &vertices,
            indices: &indices,
            ..frame_mesh(0, &template, ground, Vec3::ZERO)
        };
        assert_eq!(Gathered::collect(&[far], &probes, &mut bounds).candidates(), 0);
        for v in &mut vertices {
            v.position = (Vec3::from_array(v.position) - Vec3::new(10.5, 0.5, 0.5)).to_array();
        }
        let near = crate::FrameMesh {
            vertices: &vertices,
            indices: &indices,
            ..frame_mesh(0, &template, ground, Vec3::ZERO)
        };
        assert!(
            Gathered::collect(&[near], &probes, &mut bounds).candidates() > 0,
            "the in-place edit kept the far box"
        );
    }

    /// With the worker on (the runtime's mode), a key-0 mesh that moves every frame keeps its
    /// lights on every frame: its probes trace on the frame thread at the current pose. Sent to
    /// the worker, each trace landed after the next move had dropped it, and the emitter was
    /// deferred (dark) on every frame past the load frame.
    #[test]
    fn a_key_0_mesh_moving_every_frame_stays_lit_with_the_worker_on() {
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(false)));
        let bar = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.5, 0.05, 0.05));
        let mesh = frame_mesh(0, &bar, mat, Vec3::new(0.0, 0.6, 0.0));
        let mut cache = EmissiveCache::default();
        cache.set_background(true);
        cache.queue.land_waits = true;
        let (first, _) = collect_counted(&mut cache, &[mesh]);
        assert!(!first.is_empty(), "the bar must fit at least one light");
        let _ = collect_counted(&mut cache, &[mesh]);
        for step in 1..=5 {
            let mut moved = mesh;
            moved.model = Mat4::from_translation(Vec3::new(0.1 * step as f32, 0.6, 0.0)).to_cols_array_2d();
            let (records, work) = collect_counted(&mut cache, &[moved]);
            assert!(
                !records.is_empty(),
                "move {step}: the moving key-0 emitter went dark ({work:?})"
            );
            assert!(
                work.1 >= 1,
                "move {step}: its probes trace on its frame ({work:?})"
            );
        }
        // Then it stops: the lights of the last pose stay, and nothing traces again.
        let mut still = mesh;
        still.model = Mat4::from_translation(Vec3::new(0.5, 0.6, 0.0)).to_cols_array_2d();
        let (a, _) = collect_counted(&mut cache, &[still]);
        let (b, work) = collect_counted(&mut cache, &[still]);
        assert!(!a.is_empty());
        assert_eq!(a, b);
        assert_eq!(work, (0, 0), "a still key-0 mesh does not retrace");
    }

    /// Moving an unchanged key-0 mesh keeps the object-space fit and traces probes again.
    /// The probes are world-space, so the previous trace does not apply at the new pose.
    #[test]
    fn a_moved_key_0_mesh_keeps_its_fit_and_retraces_probes() {
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(false)));
        let bar = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.5, 0.05, 0.05));
        let mesh = frame_mesh(0, &bar, mat, Vec3::new(0.0, 0.6, 0.0));
        let mut cache = EmissiveCache::default();
        let (first_lights, _) = collect_counted(&mut cache, &[mesh]);
        let _ = collect_counted(&mut cache, &[mesh]);
        let hashes = cache.stats().hashes;
        let mut moved = mesh;
        moved.model = Mat4::from_translation(Vec3::new(3.0, 0.6, 0.0)).to_cols_array_2d();
        let (moved_lights, moved_work) = collect_counted(&mut cache, &[moved]);
        assert_eq!(
            moved_work.0, 0,
            "the object-space fit stays cached: {moved_work:?}"
        );
        assert!(
            moved_work.1 >= 1,
            "a moved key-0 mesh must retrace its probes: {moved_work:?}"
        );
        assert_ne!(moved_lights, first_lights, "world lights follow the model");
        assert!(!first_lights.is_empty(), "the bar must fit at least one light");
        assert_eq!(cache.stats().hashes, hashes, "the vertices did not change");
    }

    /// v0.1.18: past the load frame a new mesh's fit and probe trace run on the worker. A mesh
    /// that replaces another on the same object draws with the replaced mesh's lights until its
    /// own land, and those are exactly what the frame thread makes.
    #[test]
    fn a_replacing_mesh_keeps_the_old_lights_until_its_own_land_from_the_worker() {
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(false)));
        let ground: &'static LabMaterial = Box::leak(Box::new(LabMaterial::EMPTY));
        let intact = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.5, 0.05, 0.05));
        let damaged = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.2, 0.05, 0.05));
        let floor = grid(20, 10.0);
        let at = Vec3::new(0.0, 0.6, 0.0);
        let floor_mesh = frame_mesh(1, &floor, ground, Vec3::ZERO);
        let a = crate::FrameMesh {
            object_key: 7,
            ..frame_mesh(10, &intact, mat, at)
        };
        let b = crate::FrameMesh {
            object_key: 7,
            ..frame_mesh(11, &damaged, mat, at)
        };
        let run = |background: bool| {
            let mut cache = EmissiveCache::default();
            cache.set_background(background);
            let (old, _, _) = collect_frame_lights(&mut cache, &[floor_mesh, a], &hitch_view(), 64, 5.0);
            collect_frame_lights(&mut cache, &[floor_mesh, a], &hitch_view(), 64, 5.0);
            (cache, old)
        };
        // The frame thread (v0.1.17): the damaged mesh's lights in the frame it lands.
        let (mut cache, old) = run(false);
        let (new, _, _) = collect_frame_lights(&mut cache, &[floor_mesh, b], &hitch_view(), 64, 5.0);
        assert!(!old.is_empty() && !new.is_empty() && old != new);
        // The worker: the intact lights until the damaged ones land, then those.
        let (mut cache, _) = run(true);
        let frames = until(&mut cache, &[floor_mesh, b], |r| r == new.as_slice());
        let stats = cache.stats();
        eprintln!(
            "{} frames until the damaged lights landed; {stats:?}",
            frames.len()
        );
        assert_eq!(frames.last().map(|f| &f.0), Some(&new), "{stats:?}");
        assert!(
            frames.len() >= 2,
            "the first frame cannot have the worker's lights"
        );
        for (records, _) in &frames[..frames.len() - 1] {
            assert_eq!(records, &old);
        }
        assert!(
            stats.background && stats.fits_deferred == 1 && stats.stand_ins >= 1,
            "{stats:?}"
        );
    }

    /// v0.1.18: a large new emissive mesh past the load frame leaves its fit and probe trace to
    /// the worker: the frame thread runs no fit and traces no probe in the frame it lands or in
    /// any frame until its lights land (so its cost stays within [`EMISSIVE_FRAME_BUDGET_MS`]
    /// whatever the machine); on the frame thread (v0.1.17) the landing frame pays the whole
    /// fit and trace. The lights that land are the same. Work is counted, not timed (the
    /// v0.1.29 gate saw 1.93 ms for the landing frame's content hash on a loaded host).
    #[test]
    fn a_large_new_emissive_mesh_lands_within_the_frame_budget() {
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(false)));
        let ground: &'static LabMaterial = Box::leak(Box::new(LabMaterial::EMPTY));
        let bar = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.5, 0.05, 0.05));
        // A lamp of 16 384 triangles: curved, so the fit splits it down to many lights.
        let lamp = crate::uv_sphere(0.6, 128, 64);
        let floor = grid(20, 10.0);
        let base = [
            frame_mesh(1, &floor, ground, Vec3::ZERO),
            frame_mesh(2, &bar, mat, Vec3::new(-2.0, 0.6, 0.0)),
        ];
        let lamp_mesh = frame_mesh(3, &lamp, mat, Vec3::new(1.0, 1.2, 0.0));
        let all = [base[0], base[1], lamp_mesh];
        let landing = |background: bool| {
            let mut cache = EmissiveCache::default();
            cache.set_background(background);
            collect_frame_lights(&mut cache, &base, &hitch_view(), 64, 5.0);
            cache
        };
        // The lamp's own lights: what each side must fit and trace once.
        let lamp_lights =
            fit_mesh(lamp.0.as_slice(), lamp.1.as_slice(), mat, &FitParams::default()).len() as u64;
        let mut cache = landing(false);
        let (reference, sync) = collect_counted(&mut cache, &all);
        let mut cache = landing(true);
        let primed = cache.stats();
        let frames = until(&mut cache, &all, |r| r == reference.as_slice());
        let work: Vec<FrameWork> = frames.iter().map(|f| f.1).collect();
        let stats = cache.stats().since(&primed);
        eprintln!(
            "lamp of {} triangles and {lamp_lights} lights: frame thread (fits, probes) {sync:?} \
             in its landing frame ({} lights drawn); worker frames {work:?}; {stats:?}",
            lamp.1.len() / 3,
            reference.len()
        );
        assert_eq!(frames.last().map(|f| &f.0), Some(&reference), "{stats:?}");
        assert!(
            lamp_lights >= 8,
            "the lamp is too small to test the budget ({lamp_lights} lights)"
        );
        assert_eq!(
            sync,
            (1, lamp_lights),
            "the frame thread fits and traces the lamp in its landing frame"
        );
        assert!(
            work.iter().all(|&w| w == (0, 0)),
            "the frame thread fitted or traced the lamp: {work:?}"
        );
        // The worker did both: one deferred fit, landed, and every light's probe.
        assert_eq!(
            (stats.fits_deferred, stats.fits_done, stats.probes_traced),
            (1, 1, lamp_lights),
            "{stats:?}"
        );
        assert_eq!(stats.budget_holds, 0, "{stats:?}");
    }

    /// v0.2.23: a background probe batch gathers its occluder triangles on the worker. The frame
    /// thread only copies the reached meshes' positions and indices; it world-transforms and
    /// filters no triangle in the frame the batch starts or in any frame until the lights land
    /// (the emissive_hitch frame of 0.8-1.7 ms on cards and lavapipe alike was that gather of a
    /// 16 384-triangle lamp). The frame-thread path still gathers there, and the lights and probes
    /// that land from the worker are bit for bit the frame thread's. Work is counted, not timed.
    #[test]
    fn a_background_probe_batch_gathers_its_triangles_on_the_worker() {
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(false)));
        let ground: &'static LabMaterial = Box::leak(Box::new(LabMaterial::EMPTY));
        let bar = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.5, 0.05, 0.05));
        let lamp = crate::uv_sphere(0.6, 128, 64);
        let floor = grid(20, 10.0);
        let base = [
            frame_mesh(1, &floor, ground, Vec3::ZERO),
            frame_mesh(2, &bar, mat, Vec3::new(-2.0, 0.6, 0.0)),
        ];
        let all = [
            base[0],
            base[1],
            frame_mesh(3, &lamp, mat, Vec3::new(1.0, 1.2, 0.0)),
        ];
        let landing = |background: bool| {
            let mut cache = EmissiveCache::default();
            cache.set_background(background);
            collect_frame_lights(&mut cache, &base, &hitch_view(), 64, 5.0);
            cache
        };
        let mut cache = landing(false);
        let before = cache.queue.frame_thread_gathered;
        let (reference, _) = collect_counted(&mut cache, &all);
        let sync_gathered = cache.queue.frame_thread_gathered - before;
        let mut cache = landing(true);
        cache.queue.land_waits = true;
        let primed = cache.stats();
        let mut gathered = Vec::new();
        let mut last = Vec::new();
        for _ in 0..16 {
            let before = cache.queue.frame_thread_gathered;
            let (records, _) = collect_counted(&mut cache, &all);
            gathered.push(cache.queue.frame_thread_gathered - before);
            let done = records == reference;
            last = records;
            if done {
                break;
            }
        }
        let stats = cache.stats().since(&primed);
        eprintln!(
            "frame-thread gather: {sync_gathered} triangles on the frame-thread path; per worker frame {gathered:?}; \
             {stats:?}"
        );
        assert!(
            sync_gathered >= (lamp.1.len() / 3) as u64,
            "the frame-thread path gathers the lamp's triangles ({sync_gathered})"
        );
        assert_eq!(
            last, reference,
            "the worker's lights and probes differ: {stats:?}"
        );
        assert!(stats.probes_traced > 0, "{stats:?}");
        assert!(
            gathered.iter().all(|&n| n == 0),
            "the frame thread gathered triangles of a background batch: {gathered:?}"
        );
    }

    /// The worker's gather from copied positions and indices is the frame thread's gather from
    /// the borrowed meshes, triangle for triangle and group for group, including a mesh no
    /// sphere reaches, a blended one, and a moved and scaled one.
    #[test]
    fn a_copied_gather_input_gathers_what_the_meshes_give() {
        let opaque: &'static LabMaterial = Box::leak(Box::new(LabMaterial::EMPTY));
        let blended: &'static LabMaterial = Box::leak(Box::new(LabMaterial {
            alpha_mode: ALPHA_BLEND,
            ..LabMaterial::EMPTY
        }));
        let lamp = crate::uv_sphere(0.5, 32, 16);
        let floor = grid(12, 10.0);
        let far = box_mesh(Vec3::splat(-0.5), Vec3::splat(0.5));
        let mut scaled = frame_mesh(4, &lamp, opaque, Vec3::ZERO);
        scaled.model = (Mat4::from_translation(Vec3::new(0.3, 1.0, -0.2))
            * Mat4::from_scale(Vec3::new(1.5, 0.7, 1.0)))
        .to_cols_array_2d();
        let meshes = [
            frame_mesh(1, &floor, opaque, Vec3::ZERO),
            frame_mesh(2, &far, opaque, Vec3::new(80.0, 0.0, 0.0)),
            frame_mesh(3, &lamp, blended, Vec3::new(0.0, 1.0, 0.0)),
            scaled,
        ];
        let probes = [
            (Vec3::new(0.0, 1.0, 0.0), 3.0, 0.6),
            (Vec3::new(2.0, 0.5, 1.0), 1.0, 0.1),
        ];
        let direct = Gathered::collect(&meshes, &probes, &mut HashMap::new());
        let copied = GatherInput::collect(&meshes, &probes, &mut HashMap::new()).gather();
        assert!(direct.candidates() > 0);
        assert_eq!(
            direct.groups.len(),
            2,
            "the floor and the scaled lamp reach the probes"
        );
        assert_eq!(direct.groups, copied.groups);
        let bits = |g: &Gathered| -> Vec<u32> {
            g.tris
                .iter()
                .flat_map(|t| t.iter().flat_map(|v| v.to_array().map(f32::to_bits)))
                .collect()
        };
        assert_eq!(bits(&direct), bits(&copied));
    }

    /// ADR-1386: under a baked volume that names its emitters, the lights of exactly those
    /// emitters are tagged (the rest of each record unchanged); an emitter added after the bake,
    /// one moved since and a dynamic one stay untagged; a volume that names none (format v1/v2)
    /// tags nothing; over the budget the untagged lights go first.
    #[test]
    fn lights_of_baked_emitters_are_tagged_and_go_last_over_the_budget() {
        use crate::baked_gi::BakedEmitterSet;
        let leak = |v: Vec<LabVertex>, i: Vec<u32>| -> (&'static [LabVertex], &'static [u32]) {
            (Box::leak(v.into_boxed_slice()), Box::leak(i.into_boxed_slice()))
        };
        let (bar_v, bar_i) = box_mesh(Vec3::new(-0.5, -0.01, -0.01), Vec3::new(0.5, 0.01, 0.01));
        let (bv, bi) = leak(bar_v, bar_i);
        let (floor_v, floor_i) = box_mesh(Vec3::new(-20.0, -0.1, -20.0), Vec3::new(20.0, 0.0, 20.0));
        let (fv, fi) = leak(floor_v, floor_i);
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(true)));
        let ground: &'static LabMaterial = Box::leak(Box::new(LabMaterial::EMPTY));
        let mesh =
            |key: u64, v: &'static [LabVertex], i: &'static [u32], m: &'static LabMaterial, at: Vec3| {
                crate::FrameMesh {
                    is_static: false,
                    mesh_key: key,
                    material_key: key,
                    object_key: key,
                    vertices: v,
                    indices: i,
                    material: m,
                    model: Mat4::from_translation(at).to_cols_array_2d(),
                    joints: &[],
                    receiver_layer: 0,
                    shadow: crate::ShadowCast::All,
                    uv1: &[],
                    lod_dither: 0.0,
                }
            };
        // Bars 1 and 2 were baked; bar 3 was added after the bake; the bake also traced a bar at
        // x = 4 that has since moved to (4, 1, 3); key 6 is a unit's light (dynamic).
        let meshes = vec![
            mesh(1, bv, bi, mat, Vec3::new(-2.0, 1.0, 0.0)),
            mesh(2, bv, bi, mat, Vec3::new(0.0, 1.0, 0.0)),
            mesh(3, bv, bi, mat, Vec3::new(2.0, 1.0, 0.0)),
            mesh(4, fv, fi, ground, Vec3::ZERO),
            mesh(5, bv, bi, mat, Vec3::new(4.0, 1.0, 3.0)),
            mesh(6, bv, bi, mat, Vec3::new(-4.0, 1.5, 1.0)),
        ];
        let object = BakedEmitter::from_points(bv.iter().map(|v| v.position)).expect("bar bounds");
        let baked_at = |at: Vec3| {
            BakedEmitter::from_object_bounds(
                object.min,
                object.max,
                &Mat4::from_translation(at).to_cols_array_2d(),
            )
            .expect("baked bounds")
        };
        let set = std::sync::Arc::new(BakedEmitterSet::new(&[
            baked_at(Vec3::new(4.0, 1.0, 0.0)),
            baked_at(Vec3::new(-2.0, 1.0, 0.0)),
            baked_at(Vec3::new(0.0, 1.0, 0.0)),
        ]));
        let view = crate::light_budget::LightView::new(
            [0.0, 2.0, 10.0],
            [0.0, 1.0, 0.0],
            1.2,
            16.0 / 9.0,
            0.1,
            100.0,
        );
        let mut plain = EmissiveCache::default();
        let (untagged, _, plain_report) = collect_frame_lights(&mut plain, &meshes, &view, 8, 5.0);
        assert_eq!(
            (plain_report.uploaded, plain_report.baked),
            (5, 0),
            "{plain_report:?}"
        );
        assert!(untagged.iter().all(|r| r[3][3] >= 0.0));

        let mut cache = EmissiveCache::default();
        cache.set_baked_emitters(Some(set.clone()));
        let (records, _, report) = collect_frame_lights(&mut cache, &meshes, &view, 8, 5.0);
        assert_eq!((report.uploaded, report.baked), (5, 2), "{report:?}");
        // One light per bar, in mesh order (the floor emits nothing): bars 1 and 2 are tagged,
        // their probe codes kept under the tag; bar 3, the moved bar and the unit are not.
        let tags: Vec<f32> = records.iter().map(|r| r[3][3]).collect();
        assert_eq!(
            tags,
            vec![BAKED_EMITTER_TAG - 1.0, BAKED_EMITTER_TAG - 2.0, 3.0, 4.0, 5.0]
        );
        // Apart from the tag, every record is what an untagged collection uploads.
        for (r, u) in records.iter().zip(&untagged) {
            let code = if r[3][3] <= BAKED_EMITTER_TAG {
                BAKED_EMITTER_TAG - r[3][3]
            } else {
                r[3][3]
            };
            assert_eq!(code, u[3][3]);
            assert_eq!(r[..3], u[..3]);
            assert_eq!(r[3][..3], u[3][..3]);
        }
        // Later frames keep the tags (the bounds come from the per-mesh cache).
        let (again, _, again_report) = collect_frame_lights(&mut cache, &meshes, &view, 8, 5.0);
        assert_eq!(again_report.baked, 2);
        assert_eq!(again.iter().map(|r| r[3][3]).collect::<Vec<_>>(), tags);
        // The unit drives onto bar 2's spot one metre higher: still its own light.
        let mut moved = meshes.clone();
        moved[5] = mesh(6, bv, bi, mat, Vec3::new(0.0, 2.0, 0.0));
        let (_, _, moved_report) = collect_frame_lights(&mut cache, &moved, &view, 8, 5.0);
        assert_eq!(moved_report.baked, 2);

        // A volume that does not name its emitters (format v1/v2) tags nothing.
        let mut old = EmissiveCache::default();
        old.set_baked_emitters(None);
        let (old_records, _, old_report) = collect_frame_lights(&mut old, &meshes, &view, 8, 5.0);
        assert_eq!(old_report.baked, 0);
        assert_eq!(old_records, untagged);

        // Over the budget the untagged lights go first: with room for three, the baked bars
        // (bar 2 is the nearest to the eye) are the ones dropped.
        let mut tight = EmissiveCache::default();
        tight.set_baked_emitters(Some(set));
        let (kept, _, tight_report) = collect_frame_lights(&mut tight, &meshes, &view, 3, 5.0);
        assert_eq!(
            (tight_report.uploaded, tight_report.dropped, tight_report.baked),
            (3, 2, 0)
        );
        assert!(kept.iter().all(|r| r[3][3] >= 0.0), "{kept:?}");
        let mut xs: Vec<f32> = kept.iter().map(|r| r[0][0]).collect();
        xs.sort_by(f32::total_cmp);
        assert!(
            xs.len() == 3 && xs.iter().zip([-4.0, 2.0, 4.0]).all(|(a, b)| (a - b).abs() < 1e-3),
            "{xs:?}"
        );
    }

    /// ADR-2592: authored rect and tube lights join the frame's emitters after the fitted ones, a shadowed one
    /// with its distance probe (traced once per placement), a shadowless one with none.
    #[test]
    fn authored_area_lights_upload_after_the_fitted_emitters() {
        let (floor_v, floor_i) = box_mesh(Vec3::new(-20.0, -0.1, -20.0), Vec3::new(20.0, 0.0, 20.0));
        let fv: &'static [LabVertex] = Box::leak(floor_v.into_boxed_slice());
        let fi: &'static [u32] = Box::leak(floor_i.into_boxed_slice());
        let ground: &'static LabMaterial = Box::leak(Box::new(LabMaterial::EMPTY));
        let meshes = vec![crate::FrameMesh {
            is_static: false,
            mesh_key: 9,
            material_key: 9,
            object_key: 9,
            vertices: fv,
            indices: fi,
            material: ground,
            model: Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: crate::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }];
        let rect = crate::RectLight::default();
        let at = Mat4::from_rotation_translation(
            regia_core::glam::Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
            Vec3::new(0.0, 2.0, 0.0),
        );
        let lights = [
            crate::light_profiles::AreaLightInput {
                light: crate::light_profiles::rect_emitter(&rect, &at),
                range: 0.0,
                cast_shadows: true,
            },
            crate::light_profiles::AreaLightInput {
                light: crate::light_profiles::tube_emitter(
                    &crate::TubeLight::default(),
                    &Mat4::from_translation(Vec3::new(1.0, 1.0, 0.0)),
                ),
                range: 6.0,
                cast_shadows: false,
            },
        ];
        let mut cache = EmissiveCache::default();
        let view = hitch_view();
        let (records, atlas, report) = collect_frame_lights(&mut cache, &meshes, &view, 8, 1.0);
        assert!(
            records.is_empty() && atlas.is_empty(),
            "no authored lights, no emitters"
        );
        assert_eq!(report.uploaded, 0);
        cache.set_authored_area_lights(&lights);
        let (records, atlas, report) = collect_frame_lights(&mut cache, &meshes, &view, 8, 1.0);
        assert_eq!(records.len(), 2);
        assert_eq!(report.probes_traced, 1, "only the shadowed one traces");
        // A downward rect at 2 m: a one-sided quad (kind 4 + taper code), its probe in tile 0.
        assert!(
            (records[0][0][3] - (4.0 + 0.48)).abs() < 1e-6,
            "{}",
            records[0][0][3]
        );
        assert_eq!(&records[0][0][..3], &[0.0, 2.0, 0.0]);
        assert_eq!(records[0][3][3], 1.0, "probe code: tile 0");
        assert!(atlas[..PROBE_RES as usize].iter().any(|&d| d > 0.0));
        // The tube: kind 6, no probe, its authored range.
        assert!((records[1][0][3] - (6.0 + 0.48)).abs() < 1e-6);
        assert_eq!(records[1][3][3], 0.0);
        assert_eq!(records[1][2][3], 6.0);
        // The same placement again traces nothing; moving the rect traces it once more.
        let (_, _, again) = collect_frame_lights(&mut cache, &meshes, &view, 8, 1.0);
        assert_eq!(again.probes_traced, 0);
        let mut moved = lights;
        moved[0].light.centre[1] = 2.5;
        cache.set_authored_area_lights(&moved);
        let (_, _, third) = collect_frame_lights(&mut cache, &meshes, &view, 8, 1.0);
        assert_eq!(third.probes_traced, 1);
        // The emitter budget applies to them as to fitted lights.
        let (capped, _, _) = collect_frame_lights(&mut cache, &meshes, &view, 1, 1.0);
        assert_eq!(capped.len(), 1);
    }

    /// The fit must follow the emissive map's pixels, not its allocation: an in-place texel edit
    /// re-fits the mesh, and the same bytes at a new address answer the content cache instead
    /// of re-fitting (render8 determinism audit, finding 1).
    #[test]
    fn an_in_place_emissive_map_edit_refits_and_a_relocation_hits_the_cache() {
        // One 1x1 m quad, its uvs spanning [0,1]², and a 2x2 map: both triangles sample all four
        // texels among their four probes (centre + edge midpoints).
        let v = |x: f32, z: f32, u: f32, w: f32| {
            LabVertex::from_pos_normal_uv([x, 1.0, z], [0.0, 1.0, 0.0], [u, w])
        };
        let quad = vec![
            v(-0.5, -0.5, 0.0, 0.0),
            v(0.5, -0.5, 1.0, 0.0),
            v(0.5, 0.5, 1.0, 1.0),
            v(-0.5, 0.5, 0.0, 1.0),
        ];
        let (qv, qi): (&'static [LabVertex], &'static [u32]) = (
            Box::leak(quad.into_boxed_slice()),
            Box::leak(vec![0, 1, 2, 0, 2, 3].into_boxed_slice()),
        );
        let mat: &'static mut LabMaterial = Box::leak(Box::new(LabMaterial {
            emissive: [1.0, 1.0, 1.0],
            emissive_intensity: 1000.0,
            double_sided: true,
            emissive_map: Some(LabTexture {
                width: 2,
                height: 2,
                rgba: vec![255; 16],
            }),
            ..LabMaterial::EMPTY
        }));
        fn quad_frame_mesh<'a>(
            m: &'a LabMaterial,
            qv: &'a [LabVertex],
            qi: &'a [u32],
        ) -> crate::FrameMesh<'a> {
            crate::FrameMesh {
                is_static: false,
                mesh_key: 0x5EED,
                material_key: 0x5EED,
                object_key: 0,
                vertices: qv,
                indices: qi,
                material: m,
                model: Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: crate::ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        }
        let view = crate::light_budget::LightView::new(
            [0.0, 2.0, 8.0],
            [0.0, 1.0, 0.0],
            1.0,
            16.0 / 9.0,
            0.1,
            100.0,
        );
        let mut cache = EmissiveCache::default();
        // Complete frames keep the fits and probe traces on the frame thread (ADR-2582), so the
        // test steps by frame count, never the worker or the clock.
        cache.set_complete(true);
        let (r1, _, s1) = collect_frame_lights(&mut cache, &[quad_frame_mesh(mat, qv, qi)], &view, 8, 5.0);
        assert!(s1.fitted >= 1, "{s1:?}");
        let lit = r1[0][1][0];
        assert!(lit > 100.0, "{lit}");
        // In-place edit: black out texel (0,0)'s red and texel (1,1)'s RGB (bytes 12..15; the
        // sampler reads RGB only, so byte 15 is alpha and changes no light). Every buffer
        // address stays the same.
        {
            let map = mat.emissive_map.as_mut().expect("the map is set");
            map.rgba[0] = 0;
            map.rgba[12..15].fill(0);
        }
        let (r2, _, s2) = collect_frame_lights(&mut cache, &[quad_frame_mesh(mat, qv, qi)], &view, 8, 5.0);
        assert_eq!(s2.refits, 1, "the pixel edit is a new content: {s2:?}");
        assert!(
            r2[0][1][0] < lit,
            "radiance must follow the pixels: {} vs {lit}",
            r2[0][1][0]
        );
        // The same bytes in a new allocation: the content cache answers, nothing re-fits.
        {
            let map = mat.emissive_map.as_mut().expect("the map is set");
            let moved = map.rgba.clone();
            map.rgba = moved;
        }
        let (r3, _, s3) = collect_frame_lights(&mut cache, &[quad_frame_mesh(mat, qv, qi)], &view, 8, 5.0);
        assert_eq!(
            s3.refits, 0,
            "same bytes at a new address are one content: {s3:?}"
        );
        assert_eq!(r3[0][1][0], r2[0][1][0], "unchanged content, unchanged lights");
    }

    /// A mesh with `mesh_key` 0 fits on the first collection and not again. A second allocation
    /// of the same vertices misses the address memo, hashes once, and still does not fit.
    #[test]
    fn key_zero_emitter_fits_once() {
        let mat: &'static LabMaterial = Box::leak(Box::new(emissive_material(false)));
        let mesh = box_mesh(Vec3::new(-0.5, -0.05, -0.05), Vec3::new(0.5, 0.05, 0.05));
        let drawn = frame_mesh(0, &mesh, mat, Vec3::new(0.0, 0.6, 0.0));
        let mut cache = EmissiveCache::default();
        let (first, work1) = collect_counted(&mut cache, std::slice::from_ref(&drawn));
        let (second, work2) = collect_counted(&mut cache, std::slice::from_ref(&drawn));
        assert_eq!(work1.0, 1, "the first collection fits the key-0 emitter");
        assert_eq!(work2.0, 0, "the second frame refit a key-0 emitter");
        assert_eq!(first, second);
        let again = frame_mesh(0, &mesh, mat, Vec3::new(0.0, 0.6, 0.0));
        let (third, work3) = collect_counted(&mut cache, std::slice::from_ref(&again));
        assert_eq!(work3.0, 0, "a new allocation of the same vertices refit");
        assert_eq!(third, first);
    }

    /// Counts bytes, not elapsed time: immutable runtime maps hash once, and the write serial
    /// that invalidates a dynamic material's resident frame invalidates its signature once.
    #[test]
    fn tracked_emissive_maps_hash_no_bytes_in_a_steady_frame() {
        use std::sync::Arc;

        const MAP_BYTES: usize = 1 << 20;
        let dynamic = crate::DynamicTexture::new(1, 1, true);
        let mut material = Arc::new(LabMaterial {
            emissive: [1.0; 3],
            emissive_intensity: 1000.0,
            emissive_map: Some(LabTexture {
                width: 512,
                height: 512,
                rgba: vec![255; MAP_BYTES],
            }),
            // A dynamic albedo changes the material epoch without changing the fitter's map.
            images: crate::MaterialImages {
                albedo: Some(crate::MaterialImage::Dynamic(dynamic.clone())),
                ..crate::MaterialImages::EMPTY
            },
            ..LabMaterial::EMPTY
        });
        let geometry = box_mesh(Vec3::splat(-0.5), Vec3::splat(0.5));
        let template = frame_mesh(71, &geometry, Box::leak(Box::new(LabMaterial::EMPTY)), Vec3::ZERO);
        let mut cache = EmissiveCache::default();
        cache.set_complete(true);
        let mut frame = |material: &Arc<LabMaterial>| {
            let mesh = crate::FrameMesh {
                material_key: crate::runtime::material_key(material),
                material,
                ..template
            };
            let before = SIGNATURE_BYTES_HASHED.with(std::cell::Cell::get);
            // Sharing a material between draws must not walk the same pixels twice.
            collect_frame_lights(&mut cache, &[mesh, mesh], &hitch_view(), 16, 5.0);
            SIGNATURE_BYTES_HASHED.with(std::cell::Cell::get) - before
        };
        assert_eq!(frame(&material), MAP_BYTES as u64, "initial content");
        for _ in 0..4 {
            assert_eq!(frame(&material), 0, "unchanged frame must hash no map bytes");
        }
        dynamic.write(&[10, 20, 30, 255]).expect("dynamic write");
        assert_eq!(frame(&material), MAP_BYTES as u64, "one new write epoch");
        assert_eq!(frame(&material), 0, "write was already seen");
        assert!(dynamic.write(&[1]).is_err());
        assert_eq!(frame(&material), 0, "rejected writes do not move the epoch");
        Arc::make_mut(&mut material)
            .emissive_map
            .as_mut()
            .expect("map")
            .rgba[17] ^= 1;
        assert_eq!(
            frame(&material),
            MAP_BYTES as u64,
            "new runtime material generation"
        );
        assert_eq!(frame(&material), 0);
    }

    /// Capacity pressure cannot turn a steady working set into repeated map scans. Idle and
    /// dead generations still leave the memo without keeping pixel buffers alive.
    #[test]
    fn signature_memos_keep_the_working_set_and_release_idle_or_dead_generations() {
        let materials: Vec<_> = (0..=CACHE_MAX_ENTRIES)
            .map(|_| {
                let material = std::sync::Arc::new(LabMaterial {
                    emissive_map: Some(LabTexture {
                        width: 1,
                        height: 1,
                        rgba: vec![255; 4],
                    }),
                    ..LabMaterial::EMPTY
                });
                (crate::runtime::material_key(&material), material)
            })
            .collect();
        let mut cache = EmissiveCache::default();
        for frame in 1..=2 {
            cache.frame = frame;
            let before = SIGNATURE_BYTES_HASHED.with(std::cell::Cell::get);
            for (key, material) in &materials {
                cache.cached_material_signature(*key, material);
            }
            let bytes = SIGNATURE_BYTES_HASHED.with(std::cell::Cell::get) - before;
            assert_eq!(bytes, if frame == 1 { materials.len() as u64 * 4 } else { 0 });
            cache.prune_material_signatures();
            assert_eq!(cache.material_signatures.len(), materials.len());
        }
        cache.frame = 3;
        cache.cached_material_signature(materials[0].0, &materials[0].1);
        cache.prune_material_signatures();
        assert_eq!(cache.material_signatures.len(), CACHE_MAX_ENTRIES);
        cache.frame += CACHE_KEEP_FRAMES;
        cache.cached_material_signature(materials[0].0, &materials[0].1);
        cache.prune_material_signatures();
        assert_eq!(cache.material_signatures.len(), 1);
        drop(materials);
        cache.prune_material_signatures();
        assert!(cache.material_signatures.is_empty());
    }

    /// A host borrowing a mutable material cannot opt into the immutable cache by copying a
    /// registered key: the owning allocation must match too, or an in-place edit goes stale.
    #[test]
    fn a_borrowed_material_cannot_reuse_another_materials_signature_memo() {
        use std::sync::Arc;

        let owned = Arc::new(LabMaterial {
            emissive: [1.0; 3],
            emissive_intensity: 1000.0,
            emissive_map: Some(LabTexture {
                width: 2,
                height: 2,
                rgba: vec![255; 16],
            }),
            ..LabMaterial::EMPTY
        });
        let key = crate::runtime::material_key(&owned);
        let geometry = box_mesh(Vec3::splat(-0.5), Vec3::splat(0.5));
        let template = frame_mesh(72, &geometry, Box::leak(Box::new(LabMaterial::EMPTY)), Vec3::ZERO);
        let mut cache = EmissiveCache::default();
        cache.set_complete(true);
        let mut frame = |material: &LabMaterial| {
            let mesh = crate::FrameMesh {
                material_key: key,
                material,
                ..template
            };
            let before = SIGNATURE_BYTES_HASHED.with(std::cell::Cell::get);
            let (_, _, report) = collect_frame_lights(&mut cache, &[mesh], &hitch_view(), 16, 5.0);
            (
                SIGNATURE_BYTES_HASHED.with(std::cell::Cell::get) - before,
                report.refits,
            )
        };
        assert_eq!(frame(&owned).0, 16);
        let mut borrowed = (*owned).clone();
        assert_eq!(frame(&borrowed).0, 16, "a matching key is not ownership proof");
        borrowed.emissive_map.as_mut().expect("map").rgba[15] ^= 1;
        assert_eq!(
            frame(&borrowed),
            (16, 1),
            "even an alpha-only in-place edit rekeys"
        );
    }

    /// Review of fixrev-21-k, findings 1 and 5: the emissive-map signature must mix the map
    /// word-wise (`lab_frame::mix_bytes_wide`), not byte at a time. The wall-clock landing
    /// test in `tests/emissive_hitch.rs` only catches a byte-wise revert on a slow enough
    /// host; this known answer (computed by an independent model of the documented
    /// construction) catches it on any host. A 3×3 map is 36 bytes: four chunks and a tail.
    /// The byte-wise FNV-1a chain gives 0x79967aa2e30ae5ff for the same material.
    #[test]
    fn material_signature_hashes_the_map_word_wise() {
        let map = crate::lab_frame::LabTexture {
            width: 3,
            height: 3,
            rgba: (0..36u32).map(|i| (i * 31 + 7) as u8).collect(),
        };
        let mut material = LabMaterial {
            emissive: [1.0, 0.5, 0.25],
            emissive_intensity: 2.0,
            double_sided: false,
            ..LabMaterial::EMPTY
        };
        assert_eq!(material_signature(&material), 0xda28_06b2_5a2b_e94f, "no map");
        material.emissive_map = Some(map);
        let signature = material_signature(&material);
        assert_eq!(
            signature, 0x7620_d294_4962_b7f3,
            "the map mixes word-wise: {signature:#018x}"
        );
        // A one-byte edit in the map's tail changes the signature.
        if let Some(map) = material.emissive_map.as_mut() {
            map.rgba[35] ^= 1;
        }
        assert_ne!(material_signature(&material), signature);
    }
}
