// SPDX-License-Identifier: MPL-2.0
//! Surface debug views (Tincal engine request F3, `specs/13` § 11): the UV checker, texel density
//! and material ID modes of [`DebugViewMode`].
//!
//! The three modes replace what the frame draws instead of overlaying it, so they work on every
//! path and platform with no shader change: [`debug_surface_meshes`] swaps each extracted mesh
//! for a debug twin (same geometry, same skin weights, same model matrix) drawn with a shared
//! unlit material. Vertex colours are the sRGB-decoded legend colours, so the pixels show the legend colours (up to 8-bit rounding).
//!
//! - **UV checker** ([`DebugViewMode::UvChecker`]): a checker texture on the mesh's UVs, scaled
//!   per mesh so one checker cell spans `tile_m` meters of surface (equal cells across meshes
//!   with different UV scale; stretch and shear show as non-square cells). `tile_m <= 0`
//!   lays the checker out in plain UV space (four cells per UV unit).
//! - **Texel density** ([`DebugViewMode::TexelDensity`]): each triangle is coloured by the
//!   pixels per meter its base-colour texture gives it, against a target: blue under, green
//!   within the tolerance, red over ([`DensityClass`]). A material without a base-colour texture
//!   counts as [`ASSUMED_TEXTURE_PX`] square.
//! - **Material ID** ([`DebugViewMode::MaterialId`]): one false colour per material, derived
//!   from the material's name so it is stable across frames, runs and selections. Names come
//!   from the [`MaterialLabel`] component; the legend ([`LegendEntry`]) lists every colour in
//!   the frame.
//!
//! Debug twins are cached per source mesh and rebuilt only when their inputs change, so a still
//! frame uploads nothing. Triangle classification and UV scale are Regia Engine-original arithmetic
//! (`docs/provenance.md`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
thread_local! {
    static WORLD_POSITION_BUILDS: Cell<u64> = const { Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    static LEGEND_PROBES: Cell<u64> = const { Cell::new(0) };
}

/// World-position buffers built on this thread (tests). A cache hit must not increment it.
#[cfg(test)]
pub(crate) fn world_position_builds() -> u64 {
    WORLD_POSITION_BUILDS.with(Cell::get)
}

/// Meshes visited while building the material-id legend on this thread (tests). One per mesh.
#[cfg(test)]
pub(crate) fn legend_probes() -> u64 {
    LEGEND_PROBES.with(Cell::get)
}

use regia_app::prelude::*;

use crate::debug::DebugViewMode;
use crate::lab_frame::{LabMaterial, LabTexture, LabVertex};
use crate::runtime::{CpuMesh, ExtractedMesh};
use crate::skin::{JointMatrix, skin_position};

/// Side, in pixels, assumed for a material with no base-colour texture (texel density mode).
pub const ASSUMED_TEXTURE_PX: f32 = 1024.0;

/// Colour of triangles below the target density (display-referred RGB).
pub const DENSITY_UNDER_COLOR: [f32; 3] = [0.10, 0.30, 1.00];
/// Colour of triangles within the tolerance.
pub const DENSITY_WITHIN_COLOR: [f32; 3] = [0.10, 0.85, 0.20];
/// Colour of triangles above the target density.
pub const DENSITY_OVER_COLOR: [f32; 3] = [1.00, 0.15, 0.10];

/// Names a mesh entity's material in the material ID legend. Put it next to the
/// [`crate::MeshRenderer`] (the glTF hierarchy spawner and the level builder do).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct MaterialLabel(pub String);

/// One row of the material ID legend: a material name and the colour it draws with.
#[derive(Debug, Clone, PartialEq)]
pub struct LegendEntry {
    /// Material name ([`MaterialLabel`], else `unnamed #rrggbb` from the base colour).
    pub name: String,
    /// The false colour (display-referred RGB, 0..1).
    pub color: [f32; 3],
}

/// Receives the material ID legend every time a frame is drawn in that mode (the
/// `(material name, color)` callback of Tincal F3). Insert as a resource for game windows;
/// editor panes also return the legend in `PaneInfo::debug_legend`.
#[derive(Resource, Clone)]
pub struct DebugLegendSink(pub LegendCallback);

/// The legend callback type of [`DebugLegendSink`].
pub type LegendCallback = Arc<dyn Fn(&[LegendEntry]) + Send + Sync>;

impl std::fmt::Debug for DebugLegendSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DebugLegendSink")
    }
}

/// Texel density class of a triangle against a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DensityClass {
    /// Fewer pixels per meter than the target allows.
    Under,
    /// Within the tolerance of the target.
    Within,
    /// More pixels per meter than the target.
    Over,
}

impl DensityClass {
    /// The display colour of the class.
    #[must_use]
    pub fn color(self) -> [f32; 3] {
        match self {
            Self::Under => DENSITY_UNDER_COLOR,
            Self::Within => DENSITY_WITHIN_COLOR,
            Self::Over => DENSITY_OVER_COLOR,
        }
    }
}

/// Classifies a density (pixels per meter) against `target`: within when
/// `|density / target − 1| <= tolerance` (0.1 = ±10 %), under below, over above. A non-positive
/// or non-finite target classifies everything as within (nothing to compare with).
#[must_use]
pub fn texel_density_class(density_px_per_m: f32, target_px_per_m: f32, tolerance: f32) -> DensityClass {
    if !(target_px_per_m > 0.0 && target_px_per_m.is_finite()) {
        return DensityClass::Within;
    }
    let ratio = density_px_per_m / target_px_per_m;
    let tol = tolerance.max(0.0);
    if ratio < 1.0 - tol {
        DensityClass::Under
    } else if ratio > 1.0 + tol {
        DensityClass::Over
    } else {
        DensityClass::Within
    }
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// Area of the triangle `a b c`.
#[must_use]
pub fn triangle_area(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> f32 {
    let (u, v) = (sub(b, a), sub(c, a));
    let cross = [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ];
    0.5 * (cross[0] * cross[0] + cross[1] * cross[1] + cross[2] * cross[2]).sqrt()
}

fn uv_area(a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> f32 {
    0.5 * ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])).abs()
}

/// Texel density of one triangle, in pixels per meter: a texture of `texture_px` pixels per UV
/// side gives `texture_px * sqrt(uv area / world area)`. `None` for a triangle with no world
/// area or a non-finite density (a NaN UV). A triangle with no UV area has density 0.
#[must_use]
pub fn triangle_texel_density(positions: [[f32; 3]; 3], uvs: [[f32; 2]; 3], texture_px: f32) -> Option<f32> {
    let world = triangle_area(positions[0], positions[1], positions[2]);
    if world.is_nan() || world <= 1e-12 {
        return None;
    }
    let uv = uv_area(uvs[0], uvs[1], uvs[2]);
    Some(texture_px * (uv / world).sqrt()).filter(|d| d.is_finite())
}

/// FNV-1a over bytes: the stable hash behind material colours (not `DefaultHasher`, whose output
/// may change between Rust releases).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h6 = h.rem_euclid(1.0) * 6.0;
    let (i, f) = (h6.floor(), h6.fract());
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    match i as u32 % 6 {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

/// The stable false colour of a material name: the hue is its FNV-1a hash after a splitmix64 finalizer,
/// fixed saturation and value so every colour is bright and distinct from the greys of a scene.
#[must_use]
pub fn material_color(name: &str) -> [f32; 3] {
    // FNV's high bits barely move between similar names; a splitmix64 finalizer spreads them.
    let mut h = fnv1a(name.as_bytes());
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    let hue = (h >> 11) as f32 / (1u64 << 53) as f32;
    hsv_to_rgb(hue, 0.70, 0.95)
}

/// sRGB-encoded channel to linear: the unlit path writes linear values into an sRGB target, so
/// a vertex colour must be the decoded legend colour for the screen to show the legend colour.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn fallback_name(material: &LabMaterial) -> String {
    let [r, g, b, _] = material
        .base_color
        .map(|c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
    format!("unnamed #{r:02x}{g:02x}{b:02x}")
}

/// The debug frame: the replacement meshes and the material ID legend.
#[derive(Clone)]
pub struct DebugSurfaceFrame {
    /// The frame's meshes, replaced.
    pub meshes: Vec<ExtractedMesh>,
    /// The material ID legend (empty in the other modes), sorted by name.
    pub legend: Vec<LegendEntry>,
}

impl DebugSurfaceFrame {
    /// Whether the mode is one [`debug_surface_meshes`] implements.
    #[must_use]
    pub fn handles(mode: DebugViewMode) -> bool {
        matches!(
            mode,
            DebugViewMode::UvChecker { .. } | DebugViewMode::TexelDensity { .. } | DebugViewMode::MaterialId
        )
    }
}

/// Replaces `meshes` with their debug twins for `mode`. `label` names a mesh entity's material
/// ([`MaterialLabel`]). `None` when `mode` is not a surface mode (the frame draws as is).
#[must_use]
pub fn debug_surface_meshes(
    meshes: &[ExtractedMesh],
    mode: DebugViewMode,
    label: &dyn Fn(Entity) -> Option<String>,
) -> Option<DebugSurfaceFrame> {
    let _span = tracing::info_span!("debug_surface_meshes", meshes = meshes.len()).entered();
    let mut legend: Vec<LegendEntry> = Vec::new();
    let out: Vec<ExtractedMesh> = match mode {
        DebugViewMode::UvChecker { tile_m } => meshes.iter().map(|m| uv_checker_twin(m, tile_m)).collect(),
        DebugViewMode::TexelDensity {
            target_px_per_m,
            tolerance,
        } => meshes
            .iter()
            .map(|m| texel_density_twin(m, target_px_per_m, tolerance))
            .collect(),
        DebugViewMode::MaterialId => {
            let mut colors: HashMap<String, [f32; 3]> = HashMap::with_capacity(meshes.len());
            let twins = meshes
                .iter()
                .map(|m| {
                    #[cfg(test)]
                    LEGEND_PROBES.with(|probes| probes.set(probes.get().saturating_add(1)));
                    let name = label(m.entity).unwrap_or_else(|| fallback_name(&m.material));
                    let color = material_color(&name);
                    let stored = *colors.entry(name).or_insert(color);
                    material_id_twin(m, stored)
                })
                .collect();
            legend.extend(
                colors
                    .into_iter()
                    .map(|(name, color)| LegendEntry { name, color }),
            );
            legend.sort_by(|a, b| a.name.cmp(&b.name));
            twins
        }
        _ => return None,
    };
    Some(DebugSurfaceFrame { meshes: out, legend })
}

// ---------------------------------------------------------------------------------------------
// Shared materials and the twin cache.

pub(crate) fn unlit_material(albedo: Option<LabTexture>, double_sided: bool) -> Arc<LabMaterial> {
    Arc::new(LabMaterial {
        base_color: [1.0; 4],
        unlit: true,
        albedo,
        double_sided,
        ..LabMaterial::default()
    })
}

/// The 4 × 4-cell UV checker: two greys, tinted red along U and green along V so mirrored or
/// rotated UVs read at a glance, with a darker line between cells.
fn checker_texture() -> LabTexture {
    const SIZE: u32 = 128;
    const CELL: u32 = SIZE / 4;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (cx, cy) = (x / CELL, y / CELL);
            let base = if (cx + cy) % 2 == 0 { 0.86 } else { 0.42 };
            let (tu, tv) = (x as f32 / SIZE as f32, y as f32 / SIZE as f32);
            let line = x % CELL == 0 || y % CELL == 0;
            let k = if line { 0.55 } else { 1.0 };
            let r = (base * (0.75 + 0.25 * tu) * k).clamp(0.0, 1.0);
            let g = (base * (0.75 + 0.25 * tv) * k).clamp(0.0, 1.0);
            let b = (base * 0.80 * k).clamp(0.0, 1.0);
            rgba.extend_from_slice(&[(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8, 255]);
        }
    }
    LabTexture {
        width: SIZE,
        height: SIZE,
        rgba,
    }
}

/// Cells per UV unit when `tile_m <= 0` (the checker texture repeats once per UV unit).
const CELLS_PER_REPEAT: f32 = 4.0;

struct SharedMaterials {
    checker: [Arc<LabMaterial>; 2],
    flat: [Arc<LabMaterial>; 2],
}

fn shared_materials() -> &'static SharedMaterials {
    static MATERIALS: OnceLock<SharedMaterials> = OnceLock::new();
    MATERIALS.get_or_init(|| {
        let tex = checker_texture();
        SharedMaterials {
            checker: [
                unlit_material(Some(tex.clone()), false),
                unlit_material(Some(tex), true),
            ],
            flat: [unlit_material(None, false), unlit_material(None, true)],
        }
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Checker,
    Density,
    MaterialId,
    /// A lightmap view's atlas UV (`crate::lightmap_debug`).
    LightmapAtlas,
}

struct Twin {
    source: Weak<CpuMesh>,
    /// Hash of every input the twin was built from; a different value rebuilds it.
    param: u64,
    /// Joint palette (or the model matrix, when the mesh is rigid) mixed with the mode's
    /// scalars. `0` means the caller does not skip the skin (material id, lightmap atlas).
    pose: u64,
    mesh: Arc<CpuMesh>,
}

type TwinKey = (usize, usize, Kind);
static TWINS: Mutex<Option<HashMap<TwinKey, Twin>>> = Mutex::new(None);

/// Drops the twins whose source mesh is gone, at most once per [`TWIN_PURGE_SECS`] (a hit
/// would otherwise keep closed models' twin meshes alive while one mesh is framed).
fn purge_dead_twins(cache: &mut HashMap<TwinKey, Twin>) {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = std::time::Instant::now();
    if last.is_some_and(|t| now.duration_since(t).as_secs_f32() < TWIN_PURGE_SECS) {
        return;
    }
    *last = Some(now);
    drop_dead_twins(cache);
}

fn drop_dead_twins(cache: &mut HashMap<TwinKey, Twin>) {
    cache.retain(|_, twin| twin.source.strong_count() > 0);
}

/// Seconds between sweeps of dead debug twins.
const TWIN_PURGE_SECS: f32 = 2.0;

/// The cached twin of `extracted`'s mesh for `kind`, rebuilt by `build` when `param` changed.
pub(crate) fn cached_twin(
    extracted: &ExtractedMesh,
    kind: Kind,
    param: u64,
    build: impl FnOnce() -> CpuMesh,
) -> Arc<CpuMesh> {
    let key: TwinKey = (
        Arc::as_ptr(&extracted.mesh) as usize,
        extracted
            .draw_indices
            .as_ref()
            .map_or(0, |i| Arc::as_ptr(i) as usize),
        kind,
    );
    let mut guard = TWINS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = guard.get_or_insert_with(HashMap::new);
    purge_dead_twins(cache);
    if let Some(twin) = cache.get(&key)
        && twin.param == param
        && twin
            .source
            .upgrade()
            .is_some_and(|m| Arc::ptr_eq(&m, &extracted.mesh))
    {
        return Arc::clone(&twin.mesh);
    }
    let mesh = Arc::new(build());
    cache.insert(
        key,
        Twin {
            source: Arc::downgrade(&extracted.mesh),
            param,
            pose: 0,
            mesh: Arc::clone(&mesh),
        },
    );
    mesh
}

fn twin_key(extracted: &ExtractedMesh, kind: Kind) -> TwinKey {
    (
        Arc::as_ptr(&extracted.mesh) as usize,
        extracted
            .draw_indices
            .as_ref()
            .map_or(0, |indices| Arc::as_ptr(indices) as usize),
        kind,
    )
}

/// Pose of `extracted` mixed with `extra` (the mode's tile size or density target).
///
/// Never zero, so a stored `0` means "this kind does not skip the skin". A skinned mesh
/// hashes the joint palette (what `world_positions` reads); a rigid mesh hashes the model.
fn pose_fingerprint(extracted: &ExtractedMesh, extra: u64) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = crate::key_hash::KeyHasher::default();
    hasher.write_u64(extra);
    if extracted.joints.is_empty() {
        hasher.write(bytemuck::bytes_of(&extracted.model));
    } else {
        hasher.write(bytemuck::cast_slice(&extracted.joints));
    }
    hasher.finish() | 1
}

/// The cached twin when `pose` still matches, without building world positions.
fn cached_for_pose(extracted: &ExtractedMesh, kind: Kind, pose: u64) -> Option<Arc<CpuMesh>> {
    let key = twin_key(extracted, kind);
    let mut guard = TWINS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = guard.as_mut()?;
    purge_dead_twins(cache);
    let twin = cache.get(&key)?;
    if twin.pose == pose
        && twin
            .source
            .upgrade()
            .is_some_and(|mesh| Arc::ptr_eq(&mesh, &extracted.mesh))
    {
        Some(Arc::clone(&twin.mesh))
    } else {
        None
    }
}

fn remember_pose(extracted: &ExtractedMesh, kind: Kind, pose: u64) {
    let key = twin_key(extracted, kind);
    let mut guard = TWINS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cache) = guard.as_mut()
        && let Some(twin) = cache.get_mut(&key)
    {
        twin.pose = pose;
    }
}

pub(crate) fn hash_u32s(values: impl IntoIterator<Item = u32>) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for v in values {
        for b in v.to_le_bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

fn draw_indices(extracted: &ExtractedMesh) -> &[u32] {
    extracted
        .draw_indices
        .as_ref()
        .map_or(&extracted.mesh.indices, |i| i.as_ref())
}

/// World-space position of every vertex: skinned with the frame's palette, or through the model
/// matrix for a rigid mesh.
fn world_positions(extracted: &ExtractedMesh) -> Vec<[f32; 3]> {
    #[cfg(test)]
    WORLD_POSITION_BUILDS.with(|builds| builds.set(builds.get().saturating_add(1)));
    let skinned = !extracted.joints.is_empty();
    let m = &extracted.model;
    extracted
        .mesh
        .vertices
        .iter()
        .map(|v| {
            if skinned {
                let pick = |lane: usize| {
                    extracted
                        .joints
                        .get(v.joints[lane] as usize)
                        .copied()
                        .unwrap_or(JointMatrix::identity())
                };
                skin_position(v.position, &[pick(0), pick(1), pick(2), pick(3)], v.weights)
            } else {
                let p = v.position;
                std::array::from_fn(|i| m[0][i] * p[0] + m[1][i] * p[1] + m[2][i] * p[2] + m[3][i])
            }
        })
        .collect()
}

fn shared_for(extracted: &ExtractedMesh, flat: bool) -> Arc<LabMaterial> {
    let set = shared_materials();
    let i = usize::from(extracted.material.double_sided);
    Arc::clone(if flat { &set.flat[i] } else { &set.checker[i] })
}

pub(crate) fn twin_of(
    extracted: &ExtractedMesh,
    mesh: Arc<CpuMesh>,
    material: Arc<LabMaterial>,
    keep_indices: bool,
) -> ExtractedMesh {
    let mut twin = extracted.clone();
    twin.mesh = mesh;
    twin.material = material;
    if !keep_indices {
        twin.draw_indices = None;
    }
    // The twin's UV1 (lightmap) set is irrelevant to an unlit draw.
    twin.uv1 = None;
    twin
}

// ---------------------------------------------------------------------------------------------
// UV checker.

/// Meters of surface per UV unit of a mesh, area weighted over its triangles (`None` when no
/// triangle has both a world and a UV area).
fn meters_per_uv(extracted: &ExtractedMesh, world: &[[f32; 3]]) -> Option<f32> {
    let (mut world_area, mut uv_total) = (0.0_f64, 0.0_f64);
    let verts = &extracted.mesh.vertices;
    for tri in draw_indices(extracted).as_chunks::<3>().0 {
        if tri.iter().any(|&i| i as usize >= verts.len()) {
            continue;
        }
        let [a, b, c] = tri.map(|i| i as usize);
        let w = triangle_area(world[a], world[b], world[c]);
        let u = uv_area(verts[a].uv, verts[b].uv, verts[c].uv);
        if w > 1e-12 && u > 1e-12 {
            world_area += f64::from(w);
            uv_total += f64::from(u);
        }
    }
    (uv_total > 0.0).then(|| (world_area / uv_total).sqrt() as f32)
}

fn uv_checker_twin(extracted: &ExtractedMesh, tile_m: f32) -> ExtractedMesh {
    let pose = pose_fingerprint(extracted, u64::from(tile_m.to_bits()));
    if let Some(mesh) = cached_for_pose(extracted, Kind::Checker, pose) {
        return twin_of(extracted, mesh, shared_for(extracted, false), true);
    }
    // UV scale: one cell is `tile_m` meters, a texture repeat is four cells.
    let scale = if tile_m > 0.0 && tile_m.is_finite() {
        meters_per_uv(extracted, &world_positions(extracted)).map(|m| m / (CELLS_PER_REPEAT * tile_m))
    } else {
        None
    }
    .unwrap_or(1.0);
    // Quantized (1/16 octave), so a skinned mesh's breathing area does not rebuild the twin
    // every frame.
    let quantized = (scale.max(1e-6).log2() * 16.0).round() as i32;
    let scale_applied = (quantized as f32 / 16.0).exp2();
    let mesh = cached_twin(extracted, Kind::Checker, u64::from(quantized as u32), || {
        let mut vertices: Vec<LabVertex> = extracted.mesh.vertices.clone();
        for v in &mut vertices {
            v.uv = [v.uv[0] * scale_applied, v.uv[1] * scale_applied];
            v.color = [1.0; 4];
        }
        CpuMesh {
            vertices,
            indices: extracted.mesh.indices.clone(),
        }
    });
    remember_pose(extracted, Kind::Checker, pose);
    twin_of(extracted, mesh, shared_for(extracted, false), true)
}

// ---------------------------------------------------------------------------------------------
// Texel density.

fn texture_px(material: &LabMaterial) -> f32 {
    material
        .albedo
        .as_ref()
        .filter(|t| t.width > 0 && t.height > 0)
        .map_or(ASSUMED_TEXTURE_PX, |t| (t.width as f32 * t.height as f32).sqrt())
}

/// The class of every drawn triangle of `extracted` (`None` for a triangle with no world area
/// or an out-of-range index; those draw in the within colour's neutral grey).
fn triangle_classes(
    extracted: &ExtractedMesh,
    target_px_per_m: f32,
    tolerance: f32,
) -> Vec<Option<DensityClass>> {
    let world = world_positions(extracted);
    let px = texture_px(&extracted.material);
    let verts = &extracted.mesh.vertices;
    draw_indices(extracted)
        .as_chunks::<3>()
        .0
        .iter()
        .map(|tri| {
            if tri.iter().any(|&i| i as usize >= verts.len()) {
                return None;
            }
            let [a, b, c] = tri.map(|i| i as usize);
            triangle_texel_density(
                [world[a], world[b], world[c]],
                [verts[a].uv, verts[b].uv, verts[c].uv],
                px,
            )
            .map(|d| texel_density_class(d, target_px_per_m, tolerance))
        })
        .collect()
}

/// Colour of a triangle with no usable density.
const NO_DENSITY_COLOR: [f32; 3] = [0.35, 0.35, 0.35];

fn texel_density_twin(extracted: &ExtractedMesh, target_px_per_m: f32, tolerance: f32) -> ExtractedMesh {
    // The classes also read the albedo resolution: a material swap or a streamed albedo size
    // on the same mesh and pose must not reuse the old colours.
    let pose = pose_fingerprint(
        extracted,
        hash_u32s([
            target_px_per_m.to_bits(),
            tolerance.to_bits(),
            texture_px(&extracted.material).to_bits(),
        ]),
    );
    if let Some(mesh) = cached_for_pose(extracted, Kind::Density, pose) {
        return twin_of(extracted, mesh, shared_for(extracted, true), false);
    }
    let classes = triangle_classes(extracted, target_px_per_m, tolerance);
    let param = hash_u32s(classes.iter().map(|c| match c {
        None => 0,
        Some(DensityClass::Under) => 1,
        Some(DensityClass::Within) => 2,
        Some(DensityClass::Over) => 3,
    }));
    let mesh = cached_twin(extracted, Kind::Density, param, || {
        // Flat colour per triangle needs unshared vertices.
        let mut vertices = Vec::with_capacity(classes.len() * 3);
        let source = &extracted.mesh.vertices;
        for (tri, class) in draw_indices(extracted).as_chunks::<3>().0.iter().zip(&classes) {
            if tri.iter().any(|&i| i as usize >= source.len()) {
                continue;
            }
            let [r, g, b] = class.map_or(NO_DENSITY_COLOR, DensityClass::color);
            for &i in tri {
                let mut v = source[i as usize];
                v.color = [srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b), 1.0];
                vertices.push(v);
            }
        }
        let indices = (0..vertices.len() as u32).collect();
        CpuMesh { vertices, indices }
    });
    remember_pose(extracted, Kind::Density, pose);
    twin_of(extracted, mesh, shared_for(extracted, true), false)
}

// ---------------------------------------------------------------------------------------------
// Material ID.

pub(crate) fn material_id_twin(extracted: &ExtractedMesh, color: [f32; 3]) -> ExtractedMesh {
    let param = hash_u32s(color.map(f32::to_bits));
    let mesh = cached_twin(extracted, Kind::MaterialId, param, || {
        let mut vertices = extracted.mesh.vertices.clone();
        for v in &mut vertices {
            v.color = [
                srgb_to_linear(color[0]),
                srgb_to_linear(color[1]),
                srgb_to_linear(color[2]),
                1.0,
            ];
        }
        CpuMesh {
            vertices,
            indices: extracted.mesh.indices.clone(),
        }
    });
    twin_of(extracted, mesh, shared_for(extracted, true), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn density_classes_follow_the_tolerance_band() {
        let class = |d| texel_density_class(d, 512.0, 0.1);
        assert_eq!(class(400.0), DensityClass::Under);
        assert_eq!(class(512.0 * 0.91), DensityClass::Within);
        assert_eq!(class(512.0 * 1.09), DensityClass::Within);
        assert_eq!(class(600.0), DensityClass::Over);
        assert_eq!(class(0.0), DensityClass::Under);
        assert_eq!(texel_density_class(5.0, 0.0, 0.1), DensityClass::Within);
    }

    #[test]
    fn triangle_density_follows_the_uv_to_world_ratio() {
        // A 1 m right triangle whose UVs span the full 0..1 square: a 1024 px texture gives
        // 1024 px/m; half the UV extent gives half the density.
        let pos = [[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let full = triangle_texel_density(pos, [[0.0; 2], [1.0, 0.0], [0.0, 1.0]], 1024.0).expect("area");
        assert!((full - 1024.0).abs() < 0.01, "{full}");
        let half = triangle_texel_density(pos, [[0.0; 2], [0.5, 0.0], [0.0, 0.5]], 1024.0).expect("area");
        assert!((half - 512.0).abs() < 0.01, "{half}");
        assert_eq!(triangle_texel_density([[0.0; 3]; 3], [[0.0; 2]; 3], 1024.0), None);
        assert_eq!(triangle_texel_density(pos, [[0.0; 2]; 3], 1024.0), Some(0.0));
        // A NaN UV is "unknown" (the grey class), not within tolerance.
        assert_eq!(
            triangle_texel_density(pos, [[0.0; 2], [f32::NAN, 0.0], [0.0, 1.0]], 1024.0),
            None
        );
    }

    #[test]
    fn dead_twins_are_dropped() {
        let empty = || {
            Arc::new(CpuMesh {
                vertices: Vec::new(),
                indices: Vec::new(),
            })
        };
        let live = empty();
        let dead = empty();
        let mut cache: HashMap<TwinKey, Twin> = HashMap::new();
        for (i, source) in [&live, &dead].into_iter().enumerate() {
            cache.insert(
                (i, 0, Kind::Density),
                Twin {
                    source: Arc::downgrade(source),
                    param: 0,
                    pose: 0,
                    mesh: empty(),
                },
            );
        }
        drop(dead);
        drop_dead_twins(&mut cache);
        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key(&(0, 0, Kind::Density)));
    }

    #[test]
    fn material_colors_are_stable_and_distinct() {
        assert_eq!(material_color("skin"), material_color("skin"));
        let names = ["skin", "hair", "eyes", "cloth", "metal"];
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                let (ca, cb) = (material_color(a), material_color(b));
                let d: f32 = ca.iter().zip(&cb).map(|(x, y)| (x - y).abs()).sum();
                assert!(d > 0.15, "{a} and {b} too close: {ca:?} {cb:?}");
            }
        }
        assert!(material_color("skin").iter().all(|c| (0.0..=1.0).contains(c)));
    }

    fn skinned_triangle() -> ExtractedMesh {
        let vertex = |position: [f32; 3], uv: [f32; 2]| {
            let mut v = LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], uv);
            v.weights = [1.0, 0.0, 0.0, 0.0];
            v
        };
        ExtractedMesh {
            is_static: false,
            entity: regia_app::prelude::Entity::from_bits(1),
            mesh: Arc::new(CpuMesh {
                vertices: vec![
                    vertex([0.0, 0.0, 0.0], [0.0, 0.0]),
                    vertex([1.0, 0.0, 0.0], [1.0, 0.0]),
                    vertex([0.0, 0.0, 1.0], [0.0, 1.0]),
                ],
                indices: vec![0, 1, 2],
            }),
            material: Arc::new(LabMaterial::default()),
            layer: 0,
            model: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            joints: vec![JointMatrix::identity()],
            decal_layer: 0,
            uv1: None,
            shadow: crate::ShadowCast::All,
            draw_indices: None,
            lod_key: 0,
            object_key_xor: 0,
            gi_proxy: false,
            lod_dither: 0.0,
        }
    }

    /// The second UV-checker or texel-density call on one skinned mesh with the same palette
    /// must not build another world-position buffer. The twin is already cached.
    #[test]
    fn a_cached_debug_twin_does_not_reskin() {
        let mesh = skinned_triangle();
        let modes = [
            DebugViewMode::UvChecker { tile_m: 0.25 },
            DebugViewMode::TexelDensity {
                target_px_per_m: 512.0,
                tolerance: 0.1,
            },
        ];
        for mode in modes {
            let before = world_position_builds();
            let first =
                debug_surface_meshes(std::slice::from_ref(&mesh), mode, &|_| None).expect("surface mode");
            let mid = world_position_builds();
            let second =
                debug_surface_meshes(std::slice::from_ref(&mesh), mode, &|_| None).expect("surface mode");
            let after = world_position_builds();
            assert_eq!(first.meshes.len(), 1);
            assert_eq!(second.meshes.len(), 1);
            assert_eq!(mid - before, 1, "the first call skins once ({mode:?})");
            assert_eq!(
                after - mid,
                0,
                "the second call built another world-position buffer ({mode:?})"
            );
        }
    }

    /// The texel-density pose cache must not hand a mesh the classes of another albedo size:
    /// the same mesh and pose with a 64 px and then a 4096 px albedo colour differently.
    #[test]
    fn texel_density_twin_follows_the_albedo_size() {
        let with_albedo = |size: u32| {
            let mut mesh = skinned_triangle();
            mesh.material = Arc::new(LabMaterial {
                albedo: Some(LabTexture {
                    width: size,
                    height: size,
                    rgba: Vec::new(),
                }),
                ..LabMaterial::default()
            });
            mesh
        };
        let small = with_albedo(64);
        let mut large = with_albedo(4096);
        large.mesh = Arc::clone(&small.mesh);
        let mode = DebugViewMode::TexelDensity {
            target_px_per_m: 512.0,
            tolerance: 0.1,
        };
        let color = |m: &ExtractedMesh| {
            let out = debug_surface_meshes(std::slice::from_ref(m), mode, &|_| None).expect("surface mode");
            out.meshes[0].mesh.vertices[0].color
        };
        let under = color(&small);
        let over = color(&large);
        assert_ne!(under, over, "the 4096 px albedo reused the 64 px classes");
        let [r, g, b] = DensityClass::Over.color();
        assert_eq!(
            over,
            [srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b), 1.0]
        );
    }

    /// Distinct material names build the legend in one pass over the meshes.
    #[test]
    fn material_id_legend_stays_linear_in_the_mesh_count() {
        let count = 48u64;
        let meshes: Vec<ExtractedMesh> = (0..count)
            .map(|i| {
                let mut mesh = skinned_triangle();
                mesh.entity = regia_app::prelude::Entity::from_bits(i + 1);
                mesh
            })
            .collect();
        let before = legend_probes();
        let frame = debug_surface_meshes(&meshes, DebugViewMode::MaterialId, &|entity| {
            Some(format!("material-{entity:?}"))
        })
        .expect("material id");
        let probes = legend_probes() - before;
        assert_eq!(frame.legend.len(), count as usize);
        assert_eq!(
            probes, count,
            "legend scan was quadratic: {probes} comparisons for {count} meshes"
        );
    }
}
