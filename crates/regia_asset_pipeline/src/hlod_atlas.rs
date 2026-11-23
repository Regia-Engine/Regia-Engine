// SPDX-License-Identifier: MPL-2.0
//! Baked HLOD proxy atlases (ADR-2483, `docs/specs/78-world-streaming.md` § 11.1): a proxy is
//! unwrapped with the lightmap UV generator, and every atlas texel inside a proxy triangle takes the
//! base color, normal and occlusion-roughness-metallic of the source surface found by a short ray
//! pair along the proxy normal. Gutters and misses are filled by edge dilation.
//!
//! Method (original code; sources in `docs/provenance.md`): texture-space baking of a detailed
//! surface onto a simplified one by casting along the low-resolution normal (Cignoni, Montani,
//! Rocchini, Scopigno, "A general method for preserving attribute values on simplified meshes",
//! IEEE Visualization 1998; Krishnamurthy & Levoy, "Fitting smooth surfaces to dense polygon
//! meshes", SIGGRAPH 1996, for the ray-along-normal correspondence), with tangent frames from UV
//! derivatives (Lengyel, "Computing tangent space basis vectors for an arbitrary mesh", 2001) and
//! gutter dilation by neighbor averaging, as lightmap bakers do. Single-threaded, fixed iteration
//! order, no clock: the same input bakes the same bytes.
//!
//! ADR-2713 adds three things. A ray that lands in a cutout hole goes on through it (at most
//! [`MAX_CUTOUT_STEPS`] times), so a card in front of a wall bakes the wall through its holes. The
//! base color's alpha holds coverage: zero where the nearest source surface is a hole, and the
//! proxy material is alpha-masked when any texel is. And the sources are searched through a
//! two-level index (`index.rs`), so a parent over many cells costs about what a leaf does per ray.

use std::sync::Arc;

use regia_core::math::{Aabb, Affine3A, Ray, Vec2, Vec3, Vec3A};
use regia_core::mesh_query::{TriBvh, TriMesh};

use crate::hlod_cook::{MeshSoup, UNKNOWN_PROXY_COLOR};
use crate::lightmap_uv::{LightmapUvSettings, UnwrapInput, build_lightmap_layout};

mod graph;
mod index;

pub use graph::{GraphBake, bake_material_from_graph, read_graph_bake_material};
pub use index::BakeCounts;
use index::{IndexHit, Lookup, SourceIndex};

/// How many times one ray goes on through cutout holes before it gives up (ADR-2713).
pub const MAX_CUTOUT_STEPS: u32 = 8;
/// How far past a hole a ray starts again, meters.
const CUTOUT_STEP_M: f32 = 1e-3;
/// The alpha cutoff of a proxy material whose atlas has holes.
pub const PROXY_ALPHA_CUTOFF: f32 = 0.5;

/// Gutter texels around every chart (the lightmap generator's padding).
pub const ATLAS_PADDING: u32 = 2;
/// Dilation passes over unbaked texels before the rest take the atlas mean.
pub const DILATION_PASSES: u32 = 4;
/// The smallest atlas cap a level may set.
pub const ATLAS_MIN_PX: u32 = 16;
/// The largest atlas cap a level may set.
pub const ATLAS_MAX_PX: u32 = 4096;

/// A decoded texture a source material samples: row-major RGBA8 as authored (sRGB-encoded for the
/// base color, linear for the normal and ORM maps).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BakeImage {
    /// Width, texels.
    pub width: u32,
    /// Height, texels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl BakeImage {
    /// An image, or `None` when the size and the bytes disagree or it is empty.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        (width > 0 && height > 0 && rgba.len() as u64 == u64::from(width) * u64::from(height) * 4)
            .then_some(Self { width, height, rgba })
    }

    /// The texel at `(x, y)` wrapped, each channel through `decode`.
    fn texel(&self, x: i64, y: i64, decode: &dyn Fn(usize, u8) -> f32) -> [f32; 4] {
        let x = x.rem_euclid(i64::from(self.width)) as usize;
        let y = y.rem_euclid(i64::from(self.height)) as usize;
        let at = (y * self.width as usize + x) * 4;
        std::array::from_fn(|c| decode(c, self.rgba[at + c]))
    }

    /// Bilinear sample at `uv` with wrap; each channel decoded before filtering.
    fn sample(&self, uv: [f32; 2], decode: &dyn Fn(usize, u8) -> f32) -> [f32; 4] {
        let fx = f64::from(uv[0]) * f64::from(self.width) - 0.5;
        let fy = f64::from(uv[1]) * f64::from(self.height) - 0.5;
        if !(fx.is_finite() && fy.is_finite()) {
            return self.texel(0, 0, decode);
        }
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = ((fx - x0) as f32, (fy - y0) as f32);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let a = self.texel(x0, y0, decode);
        let b = self.texel(x0 + 1, y0, decode);
        let c = self.texel(x0, y0 + 1, decode);
        let d = self.texel(x0 + 1, y0 + 1, decode);
        std::array::from_fn(|i| {
            let top = a[i] + (b[i] - a[i]) * tx;
            let bottom = c[i] + (d[i] - c[i]) * tx;
            top + (bottom - top) * ty
        })
    }
}

/// What the bake reads of a source mesh's material: factors and the decoded maps.
#[derive(Debug, Clone, PartialEq)]
pub struct BakeMaterial {
    /// Linear RGBA base color factor.
    pub base_color: [f32; 4],
    /// Metallic factor.
    pub metallic: f32,
    /// Perceptual roughness factor.
    pub roughness: f32,
    /// Base color map (sRGB-encoded).
    pub base_color_map: Option<Arc<BakeImage>>,
    /// Tangent-space normal map (linear, XYZ in RGB).
    pub normal_map: Option<Arc<BakeImage>>,
    /// Occlusion (R), and with [`Self::orm_packed`] roughness (G) and metallic (B).
    pub orm_map: Option<Arc<BakeImage>>,
    /// The ORM map's G and B scale roughness and metallic.
    pub orm_packed: bool,
    /// An alpha-masked material's cutoff: a hit below it is a hole, not surface. The ray goes on
    /// through it to the surface behind (ADR-2713) and the proxy's coverage there is zero. `None`
    /// for a material drawn whole.
    pub alpha_cutoff: Option<f32>,
    /// The mesh's vertex colors scale the base color. A standard material always takes them; a
    /// material graph only when it reads `VertexColor` (ADR-2713).
    pub vertex_color: bool,
}

impl Default for BakeMaterial {
    fn default() -> Self {
        Self {
            base_color: [1.0; 4],
            metallic: 0.0,
            roughness: 0.5,
            base_color_map: None,
            normal_map: None,
            orm_map: None,
            orm_packed: false,
            alpha_cutoff: None,
            vertex_color: true,
        }
    }
}

/// The atlas a node asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtlasSettings {
    /// Texels per meter of proxy surface.
    pub texels_per_m: f32,
    /// The largest atlas side, texels.
    pub max_px: u32,
}

impl AtlasSettings {
    /// The settings of tree level `level` for a level asking `px` texels per meter at level 0 with a
    /// cap of `max_px`: the density halves per level, the cap is clamped to `16..=4096`.
    pub fn for_level(px: u32, max_px: u32, level: u32) -> Self {
        let scale = 0.5f32.powi(level.min(30) as i32);
        Self {
            texels_per_m: px as f32 * scale,
            max_px: max_px.clamp(ATLAS_MIN_PX, ATLAS_MAX_PX),
        }
    }
}

/// One source triangle's attributes, in the order of its source tree.
#[derive(Debug, Clone)]
struct SourceTriangle {
    normals: [Vec3; 3],
    uvs: [Vec2; 3],
    colors: Option<[[f32; 4]; 3]>,
    material: Option<u32>,
    /// Tangent and bitangent from the UV derivatives (zero when the UVs are degenerate).
    tangent: Vec3,
    bitangent: Vec3,
}

/// The source surface of one cell: its static meshes in world space, as a ray tree plus each
/// triangle's attributes.
#[derive(Debug, Clone)]
pub struct BakeSource {
    tree: TriBvh,
    triangles: Vec<SourceTriangle>,
    materials: Vec<Arc<BakeMaterial>>,
    bounds: Aabb,
}

/// Tangent and bitangent of a triangle from its UV derivatives (Lengyel 2001), in the runtime's
/// tangent space: the tangent along +U and the bitangent up the image, along -V. A normal map's +Y
/// points up the image while V runs down it (`regia_render::generate_vertex_tangents` flips V for
/// that, and the shaders take `cross(n, t) * w`), so the bake must read and write Y the same way.
fn uv_frame(p: [Vec3; 3], uv: [Vec2; 3]) -> (Vec3, Vec3) {
    let (e1, e2) = (p[1] - p[0], p[2] - p[0]);
    let (d1, d2) = (uv[1] - uv[0], uv[2] - uv[0]);
    let det = d1.x * d2.y - d2.x * d1.y;
    if det.abs() < 1e-12 || !det.is_finite() {
        return (Vec3::ZERO, Vec3::ZERO);
    }
    let r = 1.0 / det;
    ((e1 * d2.y - e2 * d1.y) * r, (e1 * d2.x - e2 * d1.x) * r)
}

/// An orthonormal tangent frame around `n` from a UV tangent and bitangent: Gram-Schmidt for the
/// tangent, the bitangent's side kept as the handedness. `None` when the tangent is parallel to `n`.
fn frame(n: Vec3, tangent: Vec3, bitangent: Vec3) -> Option<(Vec3, Vec3)> {
    let t = (tangent - n * n.dot(tangent)).normalize_or_zero();
    if t == Vec3::ZERO {
        return None;
    }
    let sign = if n.cross(t).dot(bitangent) < 0.0 {
        -1.0
    } else {
        1.0
    };
    Some((t, n.cross(t) * sign))
}

impl BakeSource {
    /// The source of `meshes` (each with its world pose), or `None` with no triangle.
    pub fn new(meshes: &[(Affine3A, Arc<MeshSoup>)]) -> Option<Self> {
        let mut positions: Vec<Vec3> = Vec::new();
        let mut indices: Vec<[u32; 3]> = Vec::new();
        let mut triangles = Vec::new();
        let mut materials: Vec<Arc<BakeMaterial>> = Vec::new();
        for (world, soup) in meshes {
            let material = soup.material.as_ref().map(|m| {
                match materials
                    .iter()
                    .position(|known| Arc::ptr_eq(known, m) || **known == **m)
                {
                    Some(i) => i as u32,
                    None => {
                        materials.push(Arc::clone(m));
                        materials.len() as u32 - 1
                    }
                }
            });
            let rotate = world.matrix3.inverse().transpose();
            let base = positions.len() as u32;
            positions.extend(
                soup.positions
                    .iter()
                    .map(|p| Vec3::from(world.transform_point3a(Vec3A::from(Vec3::from_array(*p))))),
            );
            let normal = |i: usize| {
                soup.normals
                    .get(i)
                    .map(|n| Vec3::from(rotate * Vec3A::from(Vec3::from_array(*n))).normalize_or_zero())
                    .unwrap_or(Vec3::ZERO)
            };
            let has_uvs = soup.uvs.len() == soup.positions.len();
            let has_colors = soup.colors.len() == soup.positions.len() && !soup.colors.is_empty();
            for tri in soup.indices.as_chunks::<3>().0 {
                let [a, b, c] = tri.map(|i| i as usize);
                if [a, b, c].iter().any(|i| *i >= soup.positions.len()) {
                    continue;
                }
                let p = [a, b, c].map(|i| positions[base as usize + i]);
                let uvs = if has_uvs {
                    [a, b, c].map(|i| Vec2::from_array(soup.uvs[i]))
                } else {
                    [Vec2::ZERO; 3]
                };
                let (tangent, bitangent) = if has_uvs {
                    uv_frame(p, uvs)
                } else {
                    (Vec3::ZERO, Vec3::ZERO)
                };
                triangles.push(SourceTriangle {
                    normals: [a, b, c].map(normal),
                    uvs,
                    colors: has_colors.then(|| [a, b, c].map(|i| soup.colors[i])),
                    material,
                    tangent,
                    bitangent,
                });
                indices.push(tri.map(|i| base + i));
            }
        }
        if indices.is_empty() {
            return None;
        }
        let bounds = Aabb::from_points(positions.iter().copied())?;
        let tree = TriBvh::build(TriMesh::new(positions, indices).ok()?).ok()?;
        Some(Self {
            tree,
            triangles,
            materials,
            bounds,
        })
    }

    /// The world bounds of the source.
    pub fn bounds(&self) -> Aabb {
        self.bounds
    }

    /// Triangles in the source.
    pub fn triangle_count(&self) -> usize {
        self.triangles.len()
    }

    /// The surface sample at barycentric `bary` on triangle `t`: linear base color, world normal
    /// and (occlusion, roughness, metallic), or a cutout hole.
    fn sample(&self, t: u32, bary: Vec3) -> Option<Sampled> {
        let tri = self.triangles.get(t as usize)?;
        let w = [bary.x, bary.y, bary.z];
        let lerp3 = |v: [Vec3; 3]| v[0] * w[0] + v[1] * w[1] + v[2] * w[2];
        let uv = tri.uvs[0] * w[0] + tri.uvs[1] * w[1] + tri.uvs[2] * w[2];
        let uv = uv.to_array();
        let mut n = lerp3(tri.normals).normalize_or_zero();
        if n == Vec3::ZERO {
            let p = self.tree.mesh().triangle(t);
            n = (p[1] - p[0]).cross(p[2] - p[0]).normalize_or_zero();
        }
        let vertex_color: [f32; 4] = tri.colors.map_or([1.0; 4], |c| {
            std::array::from_fn(|i| c[0][i] * w[0] + c[1][i] * w[1] + c[2][i] * w[2])
        });
        let material = tri.material.and_then(|m| self.materials.get(m as usize));
        let Some(material) = material else {
            let color = if tri.colors.is_some() {
                vertex_color
            } else {
                UNKNOWN_PROXY_COLOR
            };
            return Some(Sampled::Surface(SurfaceSample {
                color,
                normal: n,
                orm: [1.0, 0.5, 0.0],
            }));
        };
        let vertex_color = if material.vertex_color {
            vertex_color
        } else {
            [1.0; 4]
        };
        let texture = material
            .base_color_map
            .as_ref()
            .map_or([1.0; 4], |map| map.sample(uv, &decode_srgb));
        let color: [f32; 4] = std::array::from_fn(|i| vertex_color[i] * material.base_color[i] * texture[i]);
        // A hole of a cutout material (a foliage card) is not surface: its texels hold no meaningful
        // color, and they would darken the proxy.
        if material.alpha_cutoff.is_some_and(|cutoff| color[3] < cutoff) {
            return Some(Sampled::Hole);
        }
        let mut normal = n;
        if let Some(map) = &material.normal_map
            && let Some((t_axis, b_axis)) = frame(n, tri.tangent, tri.bitangent)
        {
            let s = map.sample(uv, &decode_unorm);
            let local = Vec3::new(s[0] * 2.0 - 1.0, s[1] * 2.0 - 1.0, s[2] * 2.0 - 1.0);
            let world = (t_axis * local.x + b_axis * local.y + n * local.z).normalize_or_zero();
            if world != Vec3::ZERO {
                normal = world;
            }
        }
        let mut orm = [1.0, material.roughness, material.metallic];
        if let Some(map) = &material.orm_map {
            let s = map.sample(uv, &decode_unorm);
            orm[0] = s[0];
            if material.orm_packed {
                orm[1] *= s[1];
                orm[2] *= s[2];
            }
        }
        Some(Sampled::Surface(SurfaceSample { color, normal, orm }))
    }
}

/// What a source hit holds.
#[derive(Debug, Clone, Copy)]
enum Sampled {
    /// Surface to bake.
    Surface(SurfaceSample),
    /// A hole of a cutout material.
    Hole,
}

/// What a texel bakes.
#[derive(Debug, Clone, Copy)]
struct SurfaceSample {
    color: [f32; 4],
    normal: Vec3,
    orm: [f32; 3],
}

/// The sRGB transfer function's decode, per channel (alpha linear).
fn decode_srgb(channel: usize, value: u8) -> f32 {
    let v = f32::from(value) / 255.0;
    if channel == 3 {
        return v;
    }
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Unsigned normalized bytes.
fn decode_unorm(_channel: usize, value: u8) -> f32 {
    f32::from(value) / 255.0
}

/// Linear to sRGB-encoded byte.
fn encode_srgb(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let e = if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (e * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Linear to unsigned normalized byte.
fn encode_unorm(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// A baked proxy: the unwrapped mesh and its three atlas images.
#[derive(Debug, Clone, PartialEq)]
pub struct BakedAtlas {
    /// The proxy with seam vertices split and atlas UVs in `uvs`, in the input's space.
    pub mesh: MeshSoup,
    /// Atlas width, texels (a multiple of 4).
    pub width: u32,
    /// Atlas height, texels (a multiple of 4).
    pub height: u32,
    /// Base color, sRGB-encoded RGB with coverage in alpha (ADR-2713): 255 on surface, 0 where
    /// the nearest source surface is a cutout hole, dilated into gutters and misses.
    pub base_color: Vec<u8>,
    /// Tangent-space normal in the proxy's UV frame, RGB = `n · 0.5 + 0.5`, alpha 255.
    pub normal: Vec<u8>,
    /// Occlusion, roughness, metallic in RGB, linear, alpha 255.
    pub orm: Vec<u8>,
    /// Texels per meter the atlas reached.
    pub texels_per_m: f32,
    /// Texels per meter asked for.
    pub asked_texels_per_m: f32,
    /// The cap truncated the density.
    pub capped: bool,
    /// Root mean square source distance over baked texels, meters.
    pub bake_error_m: f32,
    /// Texels inside proxy triangles that found no source surface (none in reach, or only cutout
    /// holes with nothing behind them), over texels inside proxy triangles.
    pub miss_ratio: f32,
    /// Texels inside proxy triangles whose nearest source surface is a cutout hole (coverage 0),
    /// over texels inside proxy triangles. Above 0 the proxy material is alpha-masked.
    pub hole_ratio: f32,
    /// The work the source searches did (ADR-2713).
    pub counts: BakeCounts,
}

/// Why a proxy did not bake (the node draws flat).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct AtlasError {
    /// `hlod_atlas_settings`, `hlod_atlas_empty` or the unwrap's `lightmap_uv_*` code.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

/// How far a texel looks for its source: 2 % of the proxy's bounds diagonal, at least 0.5 m.
pub fn bake_reach(proxy: &MeshSoup) -> f32 {
    proxy.bounds().map_or(0.5, |[lo, hi]| {
        let diagonal = (Vec3::from_array(hi) - Vec3::from_array(lo)).length();
        (diagonal * 0.02).max(0.5)
    })
}

/// What `proxy`'s three atlas textures will weigh in memory, for the residency budget: the surface
/// at the asked density with a packing allowance (a chart layout never fills its rectangle), the side
/// held at the cap, one byte per texel (BC7 and ASTC 4x4) for each of the three textures, a third more
/// for the mip chain. An estimate: the tree that holds it is built before the proxies bake.
pub fn estimated_texture_bytes(proxy: &MeshSoup, settings: AtlasSettings) -> u64 {
    /// Atlas area over surface area of a typical chart packing.
    const PACKING: f64 = 1.5;
    let mut area = 0.0f64;
    for tri in proxy.indices.as_chunks::<3>().0 {
        let corner = |i: u32| proxy.positions.get(i as usize).map(|p| Vec3::from_array(*p));
        if let (Some(a), Some(b), Some(c)) = (corner(tri[0]), corner(tri[1]), corner(tri[2])) {
            area += f64::from((b - a).cross(c - a).length()) * 0.5;
        }
    }
    let texels = area * PACKING * f64::from(settings.texels_per_m) * f64::from(settings.texels_per_m);
    let cap = f64::from(settings.max_px.clamp(ATLAS_MIN_PX, ATLAS_MAX_PX));
    let side = texels.sqrt().clamp(0.0, cap);
    (side * side * 4.0).ceil() as u64
}

/// Unwraps `proxy` (world space) and bakes its atlas from `sources`.
///
/// # Errors
///
/// [`AtlasError`] when the settings are unusable, the proxy has no triangle, or the unwrap fails.
pub fn bake_proxy_atlas(
    proxy: &MeshSoup,
    sources: &[&BakeSource],
    settings: AtlasSettings,
) -> Result<BakedAtlas, AtlasError> {
    #[cfg(test)]
    let started = std::time::Instant::now();
    #[cfg(test)]
    let lookup = if TEST_LINEAR.get() {
        Some(Lookup::Linear)
    } else {
        SourceIndex::build(sources).map(Lookup::Indexed)
    };
    #[cfg(not(test))]
    let lookup = SourceIndex::build(sources).map(Lookup::Indexed);
    let baked = bake_with(proxy, sources, settings, lookup);
    #[cfg(test)]
    if let Ok(b) = &baked {
        TEST_COUNTS.set(Some(TEST_COUNTS.get().unwrap_or_default().plus(b.counts)));
        TEST_BAKE_NANOS.set(TEST_BAKE_NANOS.get() + started.elapsed().as_nanos() as u64);
    }
    baked
}

#[cfg(test)]
thread_local! {
    /// Tests: bake through the search before the index (ADR-2713), to compare.
    pub(crate) static TEST_LINEAR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Tests: the counts of every bake on this thread, summed.
    pub(crate) static TEST_COUNTS: std::cell::Cell<Option<BakeCounts>> = const { std::cell::Cell::new(None) };
    /// Tests: wall time spent in bakes on this thread (the cook-time probe prints it; no test asserts it).
    pub(crate) static TEST_BAKE_NANOS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// What one texel found: the source surface it bakes, and its coverage.
struct TexelFind {
    /// `(distance, sample)` of the nearest source surface, through cutout holes.
    surface: Option<(f32, SurfaceSample)>,
    /// 1 when the nearest thing found is surface, 0 when it is a cutout hole; `None` when nothing
    /// is in reach.
    coverage: Option<f32>,
}

/// A walk's first hit: distance, source, whether it is a cutout hole.
type FirstHit = (f32, usize, bool);
/// A walk's first surface: distance, source, sample.
type SurfaceHit = (f32, usize, SurfaceSample);

/// The source searches of one bake.
struct Finder<'a, 'b> {
    sources: &'a [&'b BakeSource],
    lookup: Option<&'a Lookup>,
    counts: BakeCounts,
}

impl Finder<'_, '_> {
    /// The nearest source hit along `ray` within `reach`; `near` lists the sources near the proxy
    /// triangle. A short list is searched in turn, a longer one through the index (both find the
    /// same hit; the index only pays off past a few cells).
    fn ray(&mut self, ray: &Ray, reach: f32, near: &[usize]) -> Option<IndexHit> {
        self.counts.rays += 1;
        match self.lookup? {
            Lookup::Indexed(_) if near.len() <= index::FEW_CELLS => {
                index::linear_ray(self.sources, near, ray, reach, &mut self.counts)
            }
            Lookup::Indexed(index) => index.ray(self.sources, ray, reach, &mut self.counts),
            #[cfg(test)]
            Lookup::Linear => index::linear_ray(self.sources, near, ray, reach, &mut self.counts),
        }
    }

    /// The closest source point within `reach`.
    fn closest(&mut self, p: Vec3, reach: f32, near: &[usize]) -> Option<IndexHit> {
        self.counts.closest += 1;
        match self.lookup? {
            Lookup::Indexed(_) if near.len() <= index::FEW_CELLS => {
                index::linear_closest(self.sources, near, p, reach, &mut self.counts)
            }
            Lookup::Indexed(index) => index.closest(self.sources, p, reach, &mut self.counts),
            #[cfg(test)]
            Lookup::Linear => index::linear_closest(self.sources, near, p, reach, &mut self.counts),
        }
    }

    /// The sample of a hit.
    fn sample(&self, hit: IndexHit) -> Option<Sampled> {
        self.sources.get(hit.1)?.sample(hit.2, hit.3)
    }

    /// Walks from `p` along `direction` up to `reach`, on through cutout holes: the first hit
    /// and the first surface.
    fn walk(
        &mut self,
        p: Vec3,
        direction: Vec3,
        reach: f32,
        near: &[usize],
    ) -> (Option<FirstHit>, Option<SurfaceHit>) {
        let mut first = None;
        let mut travelled = 0.0f32;
        for step in 0..=MAX_CUTOUT_STEPS {
            let left = reach - travelled;
            if left < 0.0 {
                break;
            }
            let Some(hit) = self.ray(&Ray::new(p + direction * travelled, direction), left, near) else {
                break;
            };
            let at = travelled + hit.0;
            match self.sample(hit) {
                Some(Sampled::Surface(sample)) => {
                    first.get_or_insert((at, hit.1, false));
                    return (first, Some((at, hit.1, sample)));
                }
                Some(Sampled::Hole) => {
                    first.get_or_insert((at, hit.1, true));
                    if step < MAX_CUTOUT_STEPS {
                        self.counts.cutout_steps += 1;
                    }
                    travelled = at + CUTOUT_STEP_M;
                }
                None => break,
            }
        }
        (first, None)
    }

    /// What the texel at `p` with normal `n` bakes: both ray walks along `±n`, the nearer result
    /// winning (ties: the earlier source, then `+n`), else the closest source point.
    fn texel(&mut self, p: Vec3, n: Vec3, reach: f32, near: &[usize]) -> TexelFind {
        let (first_up, surface_up) = self.walk(p, n, reach, near);
        let (first_down, surface_down) = self.walk(p, -n, reach, near);
        let pick = |a: Option<(f32, usize)>, b: Option<(f32, usize)>| match (a, b) {
            (Some(a), Some(b)) => b.0 < a.0 || (b.0 == a.0 && b.1 < a.1),
            (None, Some(_)) => true,
            _ => false,
        };
        let first = if pick(first_up.map(|f| (f.0, f.1)), first_down.map(|f| (f.0, f.1))) {
            first_down
        } else {
            first_up
        };
        let surface = if pick(surface_up.map(|s| (s.0, s.1)), surface_down.map(|s| (s.0, s.1))) {
            surface_down
        } else {
            surface_up
        };
        if let Some((_, _, hole)) = first {
            return TexelFind {
                surface: surface.map(|(d, _, sample)| (d, sample)),
                coverage: Some(if hole { 0.0 } else { 1.0 }),
            };
        }
        // No ray found anything: the closest point (it cannot see through a hole).
        match self
            .closest(p, reach, near)
            .and_then(|hit| Some((hit.0, self.sample(hit)?)))
        {
            Some((d, Sampled::Surface(sample))) => TexelFind {
                surface: Some((d, sample)),
                coverage: Some(1.0),
            },
            Some((_, Sampled::Hole)) => TexelFind {
                surface: None,
                coverage: Some(0.0),
            },
            None => TexelFind {
                surface: None,
                coverage: None,
            },
        }
    }
}

/// [`bake_proxy_atlas`] with the source search given (`None`: there is no source).
fn bake_with(
    proxy: &MeshSoup,
    sources: &[&BakeSource],
    settings: AtlasSettings,
    lookup: Option<Lookup>,
) -> Result<BakedAtlas, AtlasError> {
    let _span = tracing::info_span!("hlod_atlas_bake", triangles = proxy.triangles()).entered();
    if !(settings.texels_per_m.is_finite() && settings.texels_per_m > 0.0) {
        return Err(AtlasError {
            code: "hlod_atlas_settings",
            message: format!("texels per meter {} must be above 0", settings.texels_per_m),
        });
    }
    if proxy.is_empty() || proxy.normals.len() != proxy.positions.len() {
        return Err(AtlasError {
            code: "hlod_atlas_empty",
            message: "the proxy has no triangle or no normals".into(),
        });
    }
    let uv_settings = LightmapUvSettings {
        texels_per_metre: settings.texels_per_m,
        padding: ATLAS_PADDING,
        max_atlas: settings.max_px.clamp(ATLAS_MIN_PX, ATLAS_MAX_PX),
        ..LightmapUvSettings::default()
    };
    let input = UnwrapInput {
        name: "proxy",
        positions: &proxy.positions,
        indices: &proxy.indices,
        density_scale: 1.0,
    };
    let (layout, report) = build_lightmap_layout(&[input], &uv_settings).map_err(|e| AtlasError {
        code: e.code,
        message: e.message,
    })?;
    let (Some(entry), Some(unwrap)) = (layout.entries.first(), layout.unwraps.first()) else {
        return Err(AtlasError {
            code: "hlod_atlas_empty",
            message: "the unwrap returned no entry".into(),
        });
    };
    let width = layout.width.max(4).div_ceil(4) * 4;
    let height = layout.height.max(4).div_ceil(4) * 4;
    let [sx, sy, ox, oy] = entry.scale_offset;
    let (fit_x, fit_y) = (
        layout.width as f32 / width as f32,
        layout.height as f32 / height as f32,
    );
    let mut mesh = MeshSoup {
        positions: unwrap.xref.iter().map(|&v| proxy.positions[v as usize]).collect(),
        normals: unwrap.xref.iter().map(|&v| proxy.normals[v as usize]).collect(),
        colors: Vec::new(),
        indices: unwrap.indices.clone(),
        uvs: unwrap
            .uv
            .iter()
            .map(|uv| [(uv[0] * sx + ox) * fit_x, (uv[1] * sy + oy) * fit_y])
            .collect(),
        material: None,
    };
    if proxy.colors.len() == proxy.positions.len() && !proxy.colors.is_empty() {
        mesh.colors = unwrap.xref.iter().map(|&v| proxy.colors[v as usize]).collect();
    }

    let texels = width as usize * height as usize;
    let mut finder = Finder {
        sources,
        lookup: lookup.as_ref(),
        counts: BakeCounts::default(),
    };
    let mut coverage: Vec<Option<[f32; 4]>> = vec![None; texels];
    let mut holes = 0u64;
    let mut baked: Vec<Option<SurfaceSample>> = vec![None; texels];
    let mut covered = vec![false; texels];
    let mut proxy_frame: Vec<(Vec3, Vec3, Vec3)> = vec![(Vec3::ZERO, Vec3::ZERO, Vec3::ZERO); texels];
    let reach = bake_reach(proxy);
    let (mut inside, mut missed) = (0u64, 0u64);
    let mut error_sum = 0.0f64;
    let (wf, hf) = (width as f32, height as f32);
    for tri in mesh.indices.as_chunks::<3>().0 {
        let [a, b, c] = tri.map(|i| i as usize);
        let p = [a, b, c].map(|i| Vec3::from_array(mesh.positions[i]));
        let n = [a, b, c].map(|i| Vec3::from_array(mesh.normals[i]));
        let uv = [a, b, c].map(|i| Vec2::from_array(mesh.uvs[i]));
        let face = (p[1] - p[0]).cross(p[2] - p[0]).normalize_or_zero();
        let (tangent, bitangent) = uv_frame(p, uv);
        // The sources this triangle can reach.
        let Some(tri_box) = Aabb::from_points(p) else {
            continue;
        };
        let grown = Aabb {
            min: tri_box.min - Vec3::splat(reach),
            max: tri_box.max + Vec3::splat(reach),
        };
        // The sources near the triangle: searched in turn when there are few, else through the index.
        let near: Vec<usize> = (0..sources.len())
            .filter(|&i| {
                let b = sources[i].bounds;
                b.min.cmple(grown.max).all() && b.max.cmpge(grown.min).all()
            })
            .collect();
        let px = uv.map(|t| Vec2::new(t.x * wf, t.y * hf));
        let area = (px[1] - px[0]).perp_dot(px[2] - px[0]);
        if area.abs() < 1e-12 {
            continue;
        }
        let lo = px[0].min(px[1]).min(px[2]).floor().max(Vec2::ZERO);
        let hi = px[0].max(px[1]).max(px[2]).ceil().min(Vec2::new(wf, hf));
        for y in lo.y as u32..hi.y as u32 {
            for x in lo.x as u32..hi.x as u32 {
                let at = y as usize * width as usize + x as usize;
                if covered[at] {
                    continue;
                }
                let q = Vec2::new(x as f32 + 0.5, y as f32 + 0.5);
                let w1 = (q - px[0]).perp_dot(px[2] - px[0]) / area;
                let w2 = (px[1] - px[0]).perp_dot(q - px[0]) / area;
                let w0 = 1.0 - w1 - w2;
                const EPS: f32 = -1e-4;
                if w0 < EPS || w1 < EPS || w2 < EPS {
                    continue;
                }
                covered[at] = true;
                inside += 1;
                let point = p[0] * w0 + p[1] * w1 + p[2] * w2;
                let mut normal = (n[0] * w0 + n[1] * w1 + n[2] * w2).normalize_or_zero();
                if normal == Vec3::ZERO {
                    normal = face;
                }
                if let Some((t_axis, b_axis)) = frame(normal, tangent, bitangent) {
                    proxy_frame[at] = (t_axis, b_axis, normal);
                }
                let found = finder.texel(point, normal, reach, &near);
                if let Some(c) = found.coverage {
                    coverage[at] = Some([c, 0.0, 0.0, 0.0]);
                    if c < 1.0 {
                        holes += 1;
                    }
                }
                match found.surface {
                    Some((distance, sample)) => {
                        error_sum += f64::from(distance) * f64::from(distance);
                        baked[at] = Some(sample);
                    }
                    None => missed += 1,
                }
            }
        }
    }
    let baked_count = inside - missed;
    let bake_error_m = if baked_count > 0 {
        (error_sum / baked_count as f64).sqrt() as f32
    } else {
        0.0
    };
    let miss_ratio = if inside > 0 {
        missed as f32 / inside as f32
    } else {
        0.0
    };
    let hole_ratio = if inside > 0 {
        holes as f32 / inside as f32
    } else {
        0.0
    };

    // Encode the baked texels, then dilate into gutters and misses.
    let mut base: Vec<Option<[f32; 4]>> = Vec::with_capacity(texels);
    let mut normal: Vec<Option<[f32; 4]>> = Vec::with_capacity(texels);
    let mut orm: Vec<Option<[f32; 4]>> = Vec::with_capacity(texels);
    for (sample, &(t, b, n)) in baked.iter().zip(&proxy_frame) {
        let Some(sample) = sample else {
            base.push(None);
            normal.push(None);
            orm.push(None);
            continue;
        };
        base.push(Some(sample.color));
        // The source normal in the proxy's tangent frame; a frame-less texel keeps the flat normal.
        let local = if n == Vec3::ZERO {
            Vec3::Z
        } else {
            let v = Vec3::new(
                sample.normal.dot(t),
                sample.normal.dot(b),
                sample.normal.dot(n).max(0.05),
            );
            v.normalize_or_zero()
        };
        normal.push(Some([
            local.x * 0.5 + 0.5,
            local.y * 0.5 + 0.5,
            local.z * 0.5 + 0.5,
            1.0,
        ]));
        orm.push(Some([sample.orm[0], sample.orm[1], sample.orm[2], 1.0]));
    }
    let base = dilate(base, width, height, UNKNOWN_PROXY_COLOR);
    let normal = dilate(normal, width, height, [0.5, 0.5, 1.0, 1.0]);
    let orm = dilate(orm, width, height, [1.0, 0.5, 0.0, 1.0]);
    // Coverage only leaves 1 where a texel's nearest source surface was a hole.
    let alpha: Vec<u8> = if holes > 0 {
        dilate(coverage, width, height, [1.0; 4])
            .iter()
            .map(|c| encode_unorm(c[0]))
            .collect()
    } else {
        vec![255; texels]
    };
    let encode = |texels: &[[f32; 4]], f: fn(f32) -> u8| -> Vec<u8> {
        texels
            .iter()
            .flat_map(|t| [f(t[0]), f(t[1]), f(t[2]), 255])
            .collect()
    };
    let mut base_color = encode(&base, encode_srgb);
    for (texel, a) in base_color.as_chunks_mut::<4>().0.iter_mut().zip(&alpha) {
        texel[3] = *a;
    }
    Ok(BakedAtlas {
        mesh,
        width,
        height,
        base_color,
        normal: encode(&normal, encode_unorm),
        orm: encode(&orm, encode_unorm),
        texels_per_m: entry.texels_per_metre,
        asked_texels_per_m: settings.texels_per_m,
        capped: report.fit_scale < 1.0,
        bake_error_m,
        miss_ratio,
        hole_ratio,
        counts: finder.counts,
    })
}

/// Fills unbaked texels: [`DILATION_PASSES`] passes of the mean of baked 8-neighbors, then the mean
/// of every baked texel (or `fallback` with none).
fn dilate(mut texels: Vec<Option<[f32; 4]>>, width: u32, height: u32, fallback: [f32; 4]) -> Vec<[f32; 4]> {
    let (w, h) = (width as i64, height as i64);
    for _ in 0..DILATION_PASSES {
        let before = texels.clone();
        let mut changed = false;
        for y in 0..h {
            for x in 0..w {
                let at = (y * w + x) as usize;
                if before[at].is_some() {
                    continue;
                }
                let mut sum = [0.0f32; 4];
                let mut count = 0u32;
                for (dx, dy) in [
                    (-1, -1),
                    (0, -1),
                    (1, -1),
                    (-1, 0),
                    (1, 0),
                    (-1, 1),
                    (0, 1),
                    (1, 1),
                ] {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= w || ny >= h {
                        continue;
                    }
                    if let Some(v) = before[(ny * w + nx) as usize] {
                        for (s, v) in sum.iter_mut().zip(v) {
                            *s += v;
                        }
                        count += 1;
                    }
                }
                if count > 0 {
                    texels[at] = Some(sum.map(|s| s / count as f32));
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let (mut sum, mut count) = ([0.0f64; 4], 0u64);
    for v in texels.iter().flatten() {
        for (s, v) in sum.iter_mut().zip(v) {
            *s += f64::from(*v);
        }
        count += 1;
    }
    let mean = if count > 0 {
        sum.map(|s| (s / count as f64) as f32)
    } else {
        fallback
    };
    texels.into_iter().map(|v| v.unwrap_or(mean)).collect()
}

/// The asset ids of a proxy's atlas: base color, normal and ORM textures, then the material, as
/// sub-assets of the proxy mesh id.
pub fn atlas_ids(proxy: regia_asset::AssetId) -> [regia_asset::AssetId; 4] {
    ["atlas/base", "atlas/normal", "atlas/orm", "atlas/material"]
        .map(|name| regia_asset::AssetId::sub_asset(proxy, name))
}

/// What [`write_atlas`] produced.
#[derive(Debug, Clone, PartialEq)]
pub struct WrittenAtlas {
    /// The index record of the node.
    pub record: regia_world::format::HlodAtlas,
    /// Manifest rows of the three textures and the material.
    pub manifest: Vec<regia_asset::ManifestEntry>,
    /// Bytes of the four cooked files.
    pub bytes: u64,
    /// Bytes of the three cooked texture files: what the proxy's textures weigh (ADR-2713). The
    /// cook records it in the world index in place of the estimate.
    pub texture_bytes: u64,
}

/// The cooked files of `baked` for the proxy mesh `proxy`: three textures compressed for
/// `platform` (BC7 on desktop, ASTC on Android) and one standard material over them, written to
/// `cache_dir` when given. `rel_path` and `node` name the manifest rows (`<rel>#hlod/<node>/atlas/…`).
///
/// # Errors
///
/// A texture does not compress, a file does not encode, or a write fails.
pub fn write_atlas(
    proxy: regia_asset::AssetId,
    baked: &BakedAtlas,
    platform: &str,
    cache_dir: Option<&std::path::Path>,
    rel_path: &str,
    node: usize,
) -> Result<WrittenAtlas, String> {
    use regia_asset::{
        AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CookedStandardMaterial, ManifestEntry,
        MaterialAssetRon, TextureCookUsage, TextureSlot, cooked_file_name, encode_cooked_file,
        pack_importer_version,
    };
    let _span = tracing::info_span!("hlod_atlas_write", node, w = baked.width, h = baked.height).entered();
    let [base_id, normal_id, orm_id, material_id] = atlas_ids(proxy);
    let mut files: Vec<(regia_asset::AssetId, Vec<u8>, &'static str, &'static str)> = Vec::with_capacity(4);
    for (id, rgba, usage, name) in [
        (base_id, &baked.base_color, TextureCookUsage::ColorSrgb, "base"),
        (normal_id, &baked.normal, TextureCookUsage::NormalMap, "normal"),
        (orm_id, &baked.orm, TextureCookUsage::ColorLinear, "orm"),
    ] {
        // A proxy with holes draws with the alpha cutout: its base colour keeps mip 0's
        // coverage down the chain, so the far canopy does not thin out (ADR-3090).
        let texture = crate::texture_compress::cook_texture_rgba8_coverage(
            baked.width,
            baked.height,
            rgba,
            usage,
            platform,
            crate::texture_compress::TextureCookQuality::High,
            (usage == TextureCookUsage::ColorSrgb && baked.hole_ratio > 0.0).then_some(PROXY_ALPHA_CUTOFF),
        )
        .map_err(|e| format!("atlas {name}: {e}"))?;
        let payload = texture.to_postcard().map_err(|e| format!("atlas {name}: {e}"))?;
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::texture().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        let bytes = encode_cooked_file(&header, &payload).map_err(|e| format!("atlas {name}: {e}"))?;
        files.push((id, bytes, name, "regia_asset::CookedTexture"));
    }
    let mut authored = MaterialAssetRon::default_standard();
    authored.name = Some(format!("hlod proxy {node}"));
    authored.base.base_color = [1.0; 4];
    authored.base.metallic = 1.0;
    authored.base.roughness = 1.0;
    authored.base.base_color_map = Some(TextureSlot::Asset(base_id));
    authored.base.normal_map = Some(TextureSlot::Asset(normal_id));
    authored.base.orm_map = Some(TextureSlot::Asset(orm_id));
    authored.base.orm_packed = true;
    // Holes in the coverage: the standard material's alpha cutout draws them (ADR-2713).
    if baked.hole_ratio > 0.0 {
        authored.render_state.alpha_mode = regia_asset::AlphaMode::Mask;
        authored.render_state.alpha_cutoff = PROXY_ALPHA_CUTOFF;
    }
    let payload = CookedStandardMaterial::new(authored)
        .to_postcard()
        .map_err(|e| format!("atlas material: {e}"))?;
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: AssetTypeTag::standard_material().0,
        asset_id: material_id,
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: vec![base_id, normal_id, orm_id],
        payload_len: payload.len() as u64,
    };
    let bytes = encode_cooked_file(&header, &payload).map_err(|e| format!("atlas material: {e}"))?;
    files.push((
        material_id,
        bytes,
        "material",
        "regia_asset::CookedStandardMaterial",
    ));

    let mut written = WrittenAtlas {
        record: regia_world::format::HlodAtlas {
            material: material_id,
            textures: [base_id, normal_id, orm_id],
            size: [baked.width, baked.height],
            texels_per_m: baked.texels_per_m,
            asked_texels_per_m: baked.asked_texels_per_m,
            capped: baked.capped,
            bake_error_m: baked.bake_error_m,
            miss_ratio: baked.miss_ratio,
        },
        manifest: Vec::with_capacity(4),
        bytes: 0,
        texture_bytes: 0,
    };
    for (id, bytes, name, type_path) in files {
        if let Some(dir) = cache_dir {
            crate::stream_cook::write_if_changed(&dir.join(cooked_file_name(id)), &bytes)
                .map_err(|e| e.to_string())?;
        }
        written.bytes += bytes.len() as u64;
        if name != "material" {
            written.texture_bytes += bytes.len() as u64;
        }
        written.manifest.push(ManifestEntry {
            path: format!("{rel_path}#hlod/{node}/atlas/{name}"),
            id,
            type_path: type_path.to_owned(),
        });
    }
    Ok(written)
}

#[cfg(test)]
mod tests;
