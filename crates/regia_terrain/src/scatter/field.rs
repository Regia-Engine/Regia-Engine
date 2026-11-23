// SPDX-License-Identifier: MPL-2.0
//! Runtime scatter: instances placed per class tile around the camera (streamed: placement is
//! deterministic per cell, so a tile placed now, evicted and placed again is identical), culled
//! per tile on the CPU, LOD-selected and distance-faded per instance, and emitted as
//! [`FrameMesh`]es.
//!
//! The frame renderer does the rest on its one frame path: identical `(mesh, material)` runs are
//! instanced in the depth prepass and the sun cascades (object storage buffer), and the colour
//! pass culls every instance on the GPU — frustum plus Hi-Z occlusion against this frame's depth —
//! and draws the survivors with multi-draw-indirect (`specs/26` § 4, M15a). There is no second
//! culler here: the CPU only rejects whole tiles (`specs/26` § 1 "per-cell culling").
//!
//! Distance fade shrinks an instance about its (buried) pivot over `fade_start..fade_end`, so it
//! sinks into the ground instead of popping; opaque, no dithering, no alpha pass.
//!
//! LOD switches have hysteresis ([`LOD_HYSTERESIS`]: an instance keeps its LOD until its
//! projected size is that far past the switch size) and cross-fade over [`LOD_FADE_FRAMES`]
//! frames: both LODs are emitted with complementary screen-space dither
//! ([`FrameMesh::lod_dither`]), which TAA averages into a blend. The fade clock is
//! [`ScatterField::update`] (once per frame); [`ScatterField::collect`] is idempotent within a
//! frame.

use std::collections::HashMap;
use std::sync::Arc;

use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::{Mat4, Quat, Vec3};
use regia_render::view_math::look_at_view;
use regia_render::{
    CpuMesh, ExtractedMesh, FrameMesh, Frustum, LabMaterial, LabVertex, SHADOW_LOD_MESH_TAG, ShadowCast,
};

use super::ScatterError;
use super::caster::{ScatterCasterHint, reduce_cards};
use super::detail::ScatterDetail;
use super::impostor::{
    ImpostorAtlas, ImpostorInstance, ImpostorPart, ImpostorProto, ImpostorSettings, ImpostorSource,
};
use super::painted::{PAINT_TILE_SIZE_M, PaintedScatter};
use super::placement::{
    DensityMap, DensityMaps, Ground, PlacedInstance, PlacementGrid, PlacementInputs, PlacementOrigin,
    place_class_cell_on,
};
use super::season::{ScatterSeason, leaf_present, leaf_rank};
use super::set::{ScatterClass, ScatterSet};
use crate::holes::HolesMask;
use crate::layers::TerrainLayers;
use crate::sampler::TerrainMap;

/// Target side of a streaming / culling tile, metres (rounded to whole placement cells).
pub const TILE_SIZE_M: f32 = 64.0;
/// Brightness variants per (class, mesh) material.
pub const TINT_VARIANTS: usize = 4;
/// LOD hysteresis: an instance moves to a coarser LOD once its projected size is this fraction
/// below the switch size, and back once it is this fraction above it.
pub const LOD_HYSTERESIS: f32 = 0.15;
/// Frames a LOD switch cross-fades over.
pub const LOD_FADE_FRAMES: u64 = 8;
/// Mixed into the object key of an instance's outgoing-LOD copy while it cross-fades.
const LOD_FADE_OBJECT_TAG: u64 = 0x0FAD_E0B1_EC70_0000;
/// Mixed into the object key of an instance's shadow-only draw ([`ScatterMeshAsset::shadow_lod`]).
const SHADOW_LOD_OBJECT_TAG: u64 = 0x05AD_0E1D_0000_0000;
/// Most LODs a scatter mesh may carry.
pub const MAX_SCATTER_LODS: usize = 4;
/// Extra radius of the coarse per-instance cull (metres): an instance just off screen may still
/// throw a shadow into it.
const SHADOW_MARGIN_M: f32 = 1.5;

/// Sun elevation (radians above the horizon) below which the reach is clamped: a grazing sun
/// throws an effectively unbounded shadow, so the reach caps instead of growing to infinity.
const MIN_SUN_ELEVATION: f32 = 0.03;
/// Shortest reach (metres) a caster keeps even under an overhead sun, so a rock always pushes a
/// shadow at least this far past its own base.
const MIN_SHADOW_REACH_M: f32 = 1.0;
/// Longest reach (metres) past the caster's bounding sphere. Larger than one cascade's diameter;
/// a longer shadow simply reaches every cascade the caster is drawn into.
const MAX_SHADOW_REACH_M: f32 = 128.0;

/// Reach of a scatter caster's shadow beyond its bounding sphere, following the sun elevation.
///
/// The old fixed `3 r + 1 m` assumed a sun above ~20°; a low sun throws a much longer shadow
/// that the fixed reach cut where it crossed into a third cascade. `r / tan(elevation)` is the
/// shadow length of a rock of height `r` on flat ground; it is clamped so a near-horizontal sun
/// does not grow unbounded and an overhead sun keeps the one-metre safety margin.
#[must_use]
pub fn local_shadow_reach(radius: f32, sun_elevation: f32) -> f32 {
    let elevation = sun_elevation.clamp(MIN_SUN_ELEVATION, std::f32::consts::FRAC_PI_2);
    (radius.max(1.0e-3) / elevation.tan()).clamp(MIN_SHADOW_REACH_M, MAX_SHADOW_REACH_M)
}

/// Sun elevation (radians above the horizon) from a unit vector toward the sun, +Y up.
fn sun_elevation(to_sun: [f32; 3]) -> f32 {
    let len = (to_sun[0] * to_sun[0] + to_sun[1] * to_sun[1] + to_sun[2] * to_sun[2]).sqrt();
    let y = if len > 0.0 { to_sun[1] / len } else { 1.0 };
    y.clamp(-1.0, 1.0).asin()
}

/// One LOD of a scatter mesh.
#[derive(Clone, Debug)]
pub struct ScatterMeshLod {
    /// Vertices; pivot at the centre of the base, +Y up, metres.
    pub vertices: Vec<LabVertex>,
    /// Triangle list.
    pub indices: Vec<u32>,
    /// This LOD is used while the instance's projected radius is at least this fraction of the
    /// screen half-height (the last LOD should be 0).
    pub screen_size: f32,
}

/// A mesh the `.scatter.ron` names, resolved: its LOD chain and material.
#[derive(Clone, Debug)]
pub struct ScatterMeshAsset {
    /// LOD 0 (most detailed) first. LOD 0 must hold triangles; a coarser LOD may be empty (no
    /// vertices or no indices), and then draws nothing of this part (a tree whose bark ends where
    /// its billboard part begins).
    pub lods: Vec<ScatterMeshLod>,
    /// Material (the class tint and per-instance shade multiply its base colour).
    pub material: LabMaterial,
    /// More materials drawn with every instance of this mesh, at the same LOD, fade and
    /// transform (a tree's twig cards, its far billboard). Empty for a one-material mesh.
    pub parts: Vec<ScatterMeshPart>,
    /// While a finer LOD is drawn, shadows are cast from this LOD instead (a shadow-only draw of
    /// the coarser mesh; glTF extras `shadow_lod`). `0` casts from the drawn LOD; values past the
    /// last LOD are clamped to it.
    pub shadow_lod: u8,
}

/// One more material of a [`ScatterMeshAsset`]: its geometry per LOD of the asset.
#[derive(Clone, Debug)]
pub struct ScatterMeshPart {
    /// One entry per LOD of the asset, aligned with [`ScatterMeshAsset::lods`] (their
    /// `screen_size` is not read: the asset's own chain decides the LOD). An empty LOD draws
    /// nothing of this part at that LOD.
    pub lods: Vec<ScatterMeshLod>,
    /// The part's material (tinted like the asset's).
    pub material: LabMaterial,
}

/// True when a LOD holds no triangles (it draws nothing).
fn lod_is_empty(lod: &ScatterMeshLod) -> bool {
    lod.vertices.is_empty() || lod.indices.is_empty()
}

/// Checks one LOD's triangle list (an empty LOD passes).
fn lod_problem(lod: &ScatterMeshLod) -> Option<&'static str> {
    if lod_is_empty(lod) {
        return None;
    }
    if !lod.indices.len().is_multiple_of(3) {
        return Some("is not a triangle list");
    }
    if lod.indices.iter().any(|&i| i as usize >= lod.vertices.len()) {
        return Some("indexes past its vertices");
    }
    None
}

/// Default LOD switch sizes when a cooked mesh carries none.
const DEFAULT_SCREEN_SIZES: [f32; MAX_SCATTER_LODS] = [0.06, 0.018, 0.003, 0.0];

impl ScatterMeshAsset {
    /// Builds an asset from a cooked glTF mesh (`regia cook` output) and its material. Each
    /// [`regia_asset::CookedMeshLod`] becomes one LOD (authored `_LODn` nodes or cook-generated);
    /// a mesh without a chain is one LOD.
    ///
    /// # Errors
    ///
    /// [`ScatterError::Asset`] when the cooked mesh is malformed.
    pub fn from_cooked(
        path: &str,
        mesh: &regia_asset::CookedMesh,
        material: LabMaterial,
    ) -> Result<Self, ScatterError> {
        let bad = |reason: String| ScatterError::Asset {
            path: path.to_string(),
            reason,
        };
        let cpu = mesh.to_cpu_mesh().map_err(|e| bad(e.to_string()))?;
        let vertices: Vec<LabVertex> = (0..cpu.positions.len())
            .map(|i| {
                let mut v = LabVertex::from_pos_normal_uv(
                    cpu.positions[i],
                    cpu.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]),
                    cpu.uvs.get(i).copied().unwrap_or([0.0, 0.0]),
                );
                if let Some(t) = cpu.tangents.get(i) {
                    v.tangent = *t;
                }
                v
            })
            .collect();
        let width = match mesh.index_width {
            regia_asset::CookedIndexWidth::U16 => 2u32,
            regia_asset::CookedIndexWidth::U32 => 4,
        };
        let mut lods = Vec::new();
        if mesh.lods.is_empty() {
            lods.push(ScatterMeshLod {
                vertices: vertices.clone(),
                indices: cpu.indices.clone(),
                screen_size: 0.0,
            });
        } else {
            for (i, lod) in mesh.lods.iter().take(MAX_SCATTER_LODS).enumerate() {
                let start = (lod.index_byte_offset / width) as usize;
                let count = (lod.index_byte_length / width) as usize;
                let indices = cpu
                    .indices
                    .get(start..start + count)
                    .ok_or_else(|| bad(format!("LOD {i} index range is out of bounds")))?
                    .to_vec();
                let screen_size = if lod.screen_relative_size > 0.0 {
                    lod.screen_relative_size
                } else {
                    DEFAULT_SCREEN_SIZES[i]
                };
                lods.push(ScatterMeshLod {
                    vertices: vertices.clone(),
                    indices,
                    screen_size,
                });
            }
            if let Some(last) = lods.last_mut() {
                last.screen_size = 0.0;
            }
        }
        let asset = Self {
            lods,
            material,
            parts: Vec::new(),
            shadow_lod: 0,
        };
        asset.validate(path)?;
        Ok(asset)
    }

    /// Checks the LOD chain: 1..=4 LODs, LOD 0 a non-empty triangle list, coarser LODs empty or
    /// triangle lists in range; every part has one entry per LOD, valid or empty, and triangles
    /// in at least one of them.
    ///
    /// # Errors
    ///
    /// [`ScatterError::Asset`] naming `path`.
    pub fn validate(&self, path: &str) -> Result<(), ScatterError> {
        let bad = |reason: String| ScatterError::Asset {
            path: path.to_string(),
            reason,
        };
        if self.lods.is_empty() || self.lods.len() > MAX_SCATTER_LODS {
            return Err(bad(format!("{} LODs (1..=4 allowed)", self.lods.len())));
        }
        if lod_is_empty(&self.lods[0]) {
            return Err(bad("LOD 0 is not a non-empty triangle list".into()));
        }
        for (i, lod) in self.lods.iter().enumerate() {
            if let Some(problem) = lod_problem(lod) {
                return Err(bad(format!("LOD {i} {problem}")));
            }
        }
        for (p, part) in self.parts.iter().enumerate() {
            if part.lods.len() != self.lods.len() {
                return Err(bad(format!(
                    "part {} has {} LODs, the mesh {}",
                    p + 1,
                    part.lods.len(),
                    self.lods.len()
                )));
            }
            if part.lods.iter().all(lod_is_empty) {
                return Err(bad(format!("part {} has no triangles", p + 1)));
            }
            for (i, lod) in part.lods.iter().enumerate() {
                if let Some(problem) = lod_problem(lod) {
                    return Err(bad(format!("part {} LOD {i} {problem}", p + 1)));
                }
            }
        }
        Ok(())
    }
}

/// Resolved meshes by the asset path the `.scatter.ron` names.
pub type ScatterLibrary = HashMap<String, ScatterMeshAsset>;

/// Camera the field is culled and LOD-selected against.
#[derive(Clone, Copy, Debug)]
pub struct ScatterView {
    /// Eye, world metres.
    pub eye: [f32; 3],
    /// Column-major view-projection (0..1 depth), the frame's unjittered matrix.
    pub view_proj: [[f32; 4]; 4],
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Unit vector toward the sun, +Y up. Only the elevation (its Y component) matters: a low
    /// sun throws a much longer shadow, so the per-instance [`ShadowCast::Local`] reach follows
    /// it instead of assuming a sun above ~20°.
    pub sun_dir: [f32; 3],
}

/// Elevation of the default sun [`ScatterView::look_at`] assumes, radians (20°). The old fixed
/// `3 r + 1 m` reach corresponds to roughly this elevation, so callers that do not know the sun
/// keep the previous shadow length.
const DEFAULT_SUN_ELEVATION: f32 = 20.0_f32.to_radians();

impl ScatterView {
    /// The view of a frame camera: right-handed look-at, perspective with 0..1 depth (the frame
    /// renderer's convention). The sun defaults to 20° above the horizon, toward −Z.
    #[must_use]
    pub fn look_at(eye: [f32; 3], target: [f32; 3], fov_y: f32, aspect: f32, near: f32, far: f32) -> Self {
        let view = look_at_view(Vec3::from_array(eye), Vec3::from_array(target), Vec3::Y);
        // The renderer's clip planes (ADR-869): an infinite far plane would be NaN here.
        let (fov, near, far) = regia_render::view_math::usable_projection(fov_y, near, far);
        let proj = perspective(fov, aspect.max(1e-3), near, far);
        Self {
            eye,
            view_proj: (proj * view).to_cols_array_2d(),
            fov_y,
            sun_dir: [0.0, DEFAULT_SUN_ELEVATION.sin(), -DEFAULT_SUN_ELEVATION.cos()],
        }
    }

    /// Replaces the sun direction with `to_sun` (a unit vector toward the sun, +Y up).
    #[must_use]
    pub fn with_sun(mut self, to_sun: [f32; 3]) -> Self {
        self.sun_dir = to_sun;
        self
    }
}

/// What one [`ScatterField::collect`] emitted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ScatterFrameStats {
    /// Instances resident (placed in streamed tiles around the camera).
    pub resident: u32,
    /// Tiles inside the view frustum and fade range.
    pub tiles_visible: u32,
    /// Resident tiles.
    pub tiles: u32,
    /// Instances emitted to the frame (before the GPU's per-instance frustum + Hi-Z cull).
    pub emitted: u32,
    /// Emitted instances per LOD.
    pub per_lod: [u32; MAX_SCATTER_LODS],
    /// Emitted instances inside their fade band (shrinking).
    pub fading: u32,
    /// Instances cross-fading between two LODs (each emitted twice, dithered).
    pub lod_fading: u32,
    /// Emitted instances moved onto the drawn terrain surface ([`ScatterField::set_rendered_surface`]).
    pub seated: u32,
    /// Largest seating move this frame, centimetres.
    pub seat_max_cm: u32,
    /// Seats answered by the slack test alone (same selection, eye moved less than the
    /// distance to the nearest morph-band edge).
    pub seat_fast: u32,
    /// Seats re-checked against the selection whose drawn height was still cached.
    pub seat_checked: u32,
    /// Seats whose drawn height was recomputed.
    pub seat_computed: u32,
    /// Instances examined in visible tiles (emitted or not).
    pub considered: u32,
    /// Emitted meshes dithered out against their impostor (inside the class's fade band with
    /// impostors on, [`ScatterField::set_impostors`]).
    pub impostor_band: u32,
    /// Triangles emitted into shadow maps, counted once per draw whichever cascades take it: what a
    /// lower-detail caster ([`ScatterField::set_class_caster_hint`]) brings down.
    pub caster_triangles: u64,
    /// Emitted instances without their leaves this frame ([`ScatterField::set_class_season`]).
    pub leafless: u32,
    /// Instances in blocks the outdoor frusta left out: the view is inside a cell and they lie
    /// behind its walls ([`ScatterField::extract_meshes_through`], plan row W-22, ADR-8413).
    pub portal_culled: u32,
}

/// What one [`ScatterField::update`] streamed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ScatterStreamStats {
    /// Tiles placed this update.
    pub placed_tiles: u32,
    /// Instances placed this update.
    pub placed_instances: u32,
    /// Tiles evicted this update.
    pub evicted_tiles: u32,
    /// Tiles still waiting (the per-update tile budget ran out).
    pub pending_tiles: u32,
    /// Needed tiles taken from a background placement that had finished (no wait).
    pub waited_tiles: u32,
    /// Needed tiles whose background placement was still queued or running: placed on the
    /// calling thread instead (same result) and the task cancelled, so [`ScatterField::update`]
    /// never blocks on the async compute pool.
    pub replaced_tiles: u32,
    /// Background placements queued this update.
    pub queued_tiles: u32,
    /// Needed tiles placed on the calling thread (nothing was queued for them).
    pub sync_tiles: u32,
}

/// What one [`ScatterField::heights_changed`] did (ADR-7585): only the tiles under the edited
/// box are visited, and only the instances whose ground moved are re-sat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ScatterResitStats {
    /// Resident tiles whose area meets the edited box (the ones visited).
    pub tiles_touched: u32,
    /// Of those, tiles with at least one instance re-sat (bounds and seat cache rebuilt).
    pub tiles_rebuilt: u32,
    /// Instances moved onto the new ground.
    pub instances_resat: u32,
    /// Background placements under the box dropped (they read the old heights; placed again
    /// on demand from the new ones).
    pub pending_dropped: u32,
    /// Resident tiles in the field (the ones a whole-field refresh would have visited).
    pub tiles_resident: u32,
    /// Bytes of the whole height map copied to take the edit (ADR-7638): the size of the map
    /// on the first edit while the host still shares the map it built the field from, and 0
    /// after it, background placements in flight or not.
    pub heights_copied_bytes: u64,
}

impl ScatterResitStats {
    /// Adds `other`'s counters (`tiles_resident` takes the latest).
    pub fn accumulate(&mut self, other: &Self) {
        self.tiles_touched += other.tiles_touched;
        self.tiles_rebuilt += other.tiles_rebuilt;
        self.instances_resat += other.instances_resat;
        self.pending_dropped += other.pending_dropped;
        self.tiles_resident = other.tiles_resident;
        self.heights_copied_bytes += other.heights_copied_bytes;
    }
}

/// Everything placement reads: the source-of-truth heightfield and its dressing.
#[derive(Clone)]
pub struct ScatterSource {
    /// Heightfield (shared with the terrain draw by the caller).
    pub map: Arc<TerrainMap>,
    /// Holes (tunnel mouths).
    pub holes: Option<HolesMask>,
    /// Layer stack (layer masks).
    pub layers: Option<TerrainLayers>,
    /// Density maps by the path the `.scatter.ron` names.
    pub density_maps: DensityMaps,
}

impl ScatterSource {
    /// A source with only a heightfield.
    #[must_use]
    pub fn new(map: Arc<TerrainMap>) -> Self {
        Self {
            map,
            holes: None,
            layers: None,
            density_maps: DensityMaps::new(),
        }
    }

    fn inputs(&self) -> PlacementInputs<'_> {
        PlacementInputs {
            map: &self.map,
            holes: self.holes.as_ref(),
            layers: self.layers.as_ref(),
            density_maps: Some(&self.density_maps),
        }
    }
}

struct ProtoLod {
    /// Vertices with the base blend in their colour (no tint), and the triangle list: drawn
    /// without shared materials, and what impostors bake. Empty for a LOD the part does not
    /// draw. Shared so the ECS frame bridge ([`ScatterField::extract_meshes`]) hands the
    /// renderer the same mesh every frame.
    mesh: Arc<CpuMesh>,
    screen_size: f32,
    mesh_key: u64,
    /// With shared materials ([`ScatterBuildOptions::shared_materials`]): one vertex set per
    /// tint variant, the class tint × variant shade in the colour, and its mesh key. Empty
    /// otherwise.
    variants: Vec<(Arc<CpuMesh>, u64)>,
    /// Lower-detail shadow meshes of a leaf part ([`ScatterCasterHint::leaf_keep_one_in`], ADR-3146):
    /// `(keep one in, mesh, key)`, built when a class first asks and kept with the field (the renderer
    /// keys a runtime mesh by its address).
    reduced: Vec<(u8, Arc<CpuMesh>, u64)>,
}

impl ProtoLod {
    fn is_empty(&self) -> bool {
        self.mesh.indices.is_empty()
    }

    /// The vertex set and mesh key a tint variant draws with.
    fn variant(&self, variant: usize) -> (&Arc<CpuMesh>, u64) {
        self.variants
            .get(variant)
            .map_or((&self.mesh, self.mesh_key), |(mesh, key)| (mesh, *key))
    }

    /// The lower-detail shadow mesh keeping one card in `keep`, when this LOD has one.
    fn reduced(&self, keep: u8) -> Option<(&Arc<CpuMesh>, u64)> {
        self.reduced
            .iter()
            .find(|(k, _, _)| *k == keep)
            .map(|(_, mesh, key)| (mesh, *key))
    }
}

/// The geometry and materials of one material part of a prototype (part 0 is the asset's own).
struct ProtoPart {
    lods: Vec<ProtoLod>,
    materials: Vec<Arc<LabMaterial>>,
    material_keys: Vec<u64>,
    /// [`Self::materials`] at rest, for painted instances marked
    /// [`PAINTED_FLAG_STILL`](super::painted::PAINTED_FLAG_STILL); empty when the material does not
    /// sway (`specs/88` § 2.5).
    still: Vec<(Arc<LabMaterial>, u64)>,
}

/// One part's materials, keys and still copies as built ([`Prototype::built`]).
type PartMaterials = (Vec<Arc<LabMaterial>>, Vec<u64>, Vec<(Arc<LabMaterial>, u64)>);

/// A copy of a built material under subsurface factors, by the built material's address and the
/// factors' words ([`ScatterField::set_class_subsurface`]).
type SubsurfaceCopyKey = (usize, u64, u64, u32);

/// The copies a field has made, each kept with the built material it came from: the renderer keys
/// a runtime material by its address, so a copy that was freed could leave its address, and its
/// resident textures, to another class's new copy. Nothing here is freed before the field.
type SubsurfaceCopies = HashMap<SubsurfaceCopyKey, (Arc<LabMaterial>, Arc<LabMaterial>)>;

/// One class's copy of a leaf material under its season (ADR-3146): the material it starts from (kept,
/// so that its address stays taken), the copy and the key it draws under. The key and the address hold
/// for the life of the field, so a new tint rewrites the copy's parameters and the renderer keeps its
/// textures and bind group.
struct SeasonCopy {
    source: Arc<LabMaterial>,
    copy: Arc<LabMaterial>,
    key: u64,
}

/// [`SeasonCopy`]s by class and the address of the material each starts from.
type SeasonCopies = HashMap<(usize, usize), SeasonCopy>;

/// The material and key a built material draws with under `factors` (ADR-3170): itself with the
/// factors off; otherwise its copy carrying them (made once, in `copies`, so a material shared by
/// variants stays one material and switching back and forth uploads nothing new), under a key
/// mixed from the built key and the factors.
fn with_subsurface(
    material: &Arc<LabMaterial>,
    key: u64,
    factors: regia_render::SubsurfaceFactors,
    copies: &mut SubsurfaceCopies,
) -> (Arc<LabMaterial>, u64) {
    if !factors.is_on() {
        return (Arc::clone(material), key);
    }
    let words = (
        u64::from(factors.profile.word()) << 32 | u64::from(factors.radius_mm.to_bits()),
        u64::from(factors.tint[0].to_bits()) << 32 | u64::from(factors.tint[1].to_bits()),
    );
    let key = mix_key(
        mix_key(key, words.0, words.1),
        u64::from(factors.tint[2].to_bits()),
        0x5552,
    );
    let address = Arc::as_ptr(material) as usize;
    let (_, copy) = copies
        .entry((address, words.0, words.1, factors.tint[2].to_bits()))
        .or_insert_with(|| {
            let copy = LabMaterial {
                subsurface: factors,
                ..(**material).clone()
            };
            (Arc::clone(material), Arc::new(copy))
        });
    (Arc::clone(copy), key)
}

/// `base` with `tint` on its colour; alpha unchanged.
fn tinted(base: [f32; 4], tint: [f32; 3]) -> [f32; 4] {
    [base[0] * tint[0], base[1] * tint[1], base[2] * tint[2], base[3]]
}

/// The material and key `material` draws with under the season tint of class `class` (ADR-3146): its
/// copy for the class, made once and then rewritten in place ([`ScatterField::set_class_season`]).
fn with_season_tint(
    material: &Arc<LabMaterial>,
    key: u64,
    class: usize,
    tint: [f32; 3],
    copies: &mut SeasonCopies,
) -> (Arc<LabMaterial>, u64) {
    let address = Arc::as_ptr(material) as usize;
    let entry = copies.entry((class, address)).or_insert_with(|| SeasonCopy {
        source: Arc::clone(material),
        copy: Arc::new(LabMaterial {
            base_color: tinted(material.base_color, tint),
            ..(**material).clone()
        }),
        key: mix_key(key, class as u64, 0x5ea5),
    });
    (Arc::clone(&entry.copy), entry.key)
}

/// How a [`ScatterField`] builds its prototypes ([`ScatterField::new_with`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScatterBuildOptions {
    /// One material per mesh path, shared by every class and tint variant that places the
    /// mesh, with the class tint and the variant shade in the vertex colours instead of the
    /// material colour. The renderer uploads a material's textures once per material, so a
    /// scan rock's maps are resident once instead of once per (class, variant): four times
    /// less texture memory for one class, more when several classes share meshes. The image
    /// differs from the default only by float rounding (`albedo × base × vertex` is
    /// associated differently), so it is opt-in (ADR-041). Off by default.
    pub shared_materials: bool,
}

/// The size of a prototype (what placement needs; shared with background placement).
#[derive(Clone, Copy)]
struct ProtoShape {
    /// Bounding sphere about (0, centre_y, 0) at scale 1.
    radius: f32,
    centre_y: f32,
    /// Base footprint radius (XZ extent of the lowest 20 % of the mesh).
    footprint: f32,
    height: f32,
}

/// One (class, mesh) pair, ready to draw.
struct Prototype {
    lods: Vec<ProtoLod>,
    /// Material base colour × class tint (before the per-variant shade), linear.
    base_tint: [f32; 3],
    /// The class's `tint_jitter` (variant shades are `1 ± tint_jitter`).
    tint_jitter: f32,
    materials: Vec<Arc<LabMaterial>>,
    material_keys: Vec<u64>,
    /// [`Self::materials`] at rest ([`ProtoPart::still`]).
    still: Vec<(Arc<LabMaterial>, u64)>,
    /// The asset's further material parts ([`ScatterMeshAsset::parts`]), drawn as parts 1.. .
    parts: Vec<ProtoPart>,
    /// The materials as built, per part (0 = the asset's own): what
    /// [`ScatterField::set_class_subsurface`] starts from and restores with the factors off.
    built: Vec<PartMaterials>,
    /// [`ScatterMeshAsset::shadow_lod`], clamped to the chain.
    shadow_lod: usize,
    /// Bit per part (0 = the asset's own): the parts a season and a lower-detail caster treat as leaves.
    /// Its alpha-masked materials; every part when it has none (a mesh bush is all leaves).
    leaf_parts: u32,
    shape: ProtoShape,
}

impl Prototype {
    /// Whether material part `part` counts as leaves ([`Self::leaf_parts`]).
    fn is_leaf(&self, part: usize) -> bool {
        part < 32 && self.leaf_parts & (1 << part) != 0
    }

    /// LOD `lod` of part `part` (0 = the asset's own geometry).
    fn part_lod(&self, part: usize, lod: usize) -> Option<&ProtoLod> {
        match part {
            0 => self.lods.get(lod),
            p => self.parts.get(p - 1).and_then(|part| part.lods.get(lod)),
        }
    }

    /// Material and key of part `part`, tint variant `variant`; at rest when `still` and the
    /// material sways.
    fn part_material(&self, part: usize, variant: usize, still: bool) -> (&Arc<LabMaterial>, u64) {
        let (materials, keys, rest) = match part {
            0 => (&self.materials, &self.material_keys, &self.still),
            p => self
                .parts
                .get(p - 1)
                .map_or((&self.materials, &self.material_keys, &self.still), |part| {
                    (&part.materials, &part.material_keys, &part.still)
                }),
        };
        let v = variant.min(materials.len() - 1);
        match rest.get(v).filter(|_| still) {
            Some((material, key)) => (material, *key),
            None => (&materials[v], keys[v]),
        }
    }
}

#[derive(Clone, Copy)]
struct Instance {
    proto: u32,
    variant: u8,
    /// A painted instance marked [`PAINTED_FLAG_STILL`](super::painted::PAINTED_FLAG_STILL): drawn
    /// with the material at rest.
    still: bool,
    /// Stable per (class, cell, ordinal): the frame renderer's object cache key.
    object_key: u64,
    model: [[f32; 4]; 4],
    centre: [f32; 3],
    radius: f32,
    /// Ground contact point and footprint radius (contact map).
    ground: [f32; 3],
    footprint: f32,
    /// Sub-block of the tile (`BLOCKS`² of them, row-major).
    block: u8,
}

/// Sub-blocks per tile side: collect rejects whole blocks before testing their instances.
const BLOCKS: i32 = 4;

/// Conservative bounds of one sub-block (1 cm wider than its instances need, so a block test
/// never disagrees with the per-instance test it replaces, float rounding included).
#[derive(Clone, Copy, Default)]
struct Block {
    centre: [f32; 3],
    /// Farthest instance centre from `centre`.
    reach: f32,
    /// Largest instance bounding radius in the block. The block's cull reach — how far a caster
    /// may sit off-screen and still shadow the view — is `reach + local_shadow_reach(max_radius,
    /// sun_elevation) + SHADOW_MARGIN_M`, computed per frame because it follows the sun.
    max_radius: f32,
    count: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct TileKey {
    class: u16,
    tx: i32,
    tz: i32,
}

/// The seating of one instance, valid while its inputs are unchanged: the drawn node, its
/// morph band, the three vertex morph factors, the heights generation and the surface epoch.
/// The drawn height is a pure function of those, so a hit is bit-identical to a recompute.
#[derive(Clone, Copy, Default)]
struct SeatEntry {
    valid: bool,
    node: crate::cdlod::QuadNode,
    band: [u32; 2],
    ts: [u32; 3],
    generation: [u64; 2],
    dy: f32,
    /// Selection the entry was checked against and the eye then.
    selection: u64,
    eye: [f32; 2],
    /// Eye travel (metres) that keeps every vertex in its clamped morph state.
    slack: f32,
}

struct Tile {
    centre: [f32; 3],
    radius: f32,
    instances: Vec<Instance>,
    blocks: [Block; (BLOCKS * BLOCKS) as usize],
    /// Needed by the camera at least once since it was placed (prefetched tiles are inactive
    /// until then and are invisible to every query).
    active: bool,
    /// Seat cache, one entry per instance.
    seat: std::sync::Mutex<Vec<SeatEntry>>,
    /// LOD state (hysteresis, cross-fade), one entry per instance.
    lod: std::sync::Mutex<Vec<LodState>>,
}

/// The LOD an instance draws and the switch in progress.
#[derive(Clone, Copy)]
struct LodState {
    /// Current LOD ([`LodState::UNSET`] until first emitted).
    lod: u8,
    /// LOD before the last switch; equal to `lod` when no cross-fade runs.
    prev: u8,
    /// Field frame the last switch began.
    start: u64,
    /// Field frame the instance was last emitted.
    seen: u64,
}

impl LodState {
    const UNSET: u8 = u8::MAX;
    const NEW: Self = Self {
        lod: Self::UNSET,
        prev: Self::UNSET,
        start: 0,
        seen: 0,
    };
}

struct ClassRuntime {
    name: String,
    /// The class's subsurface factors ([`ScatterField::set_class_subsurface`]); off as built.
    subsurface: regia_render::SubsurfaceFactors,
    /// The class's shadow casters past a drawn LOD ([`ScatterField::set_class_caster_hint`]).
    caster: Option<ScatterCasterHint>,
    /// The class's season ([`ScatterField::set_class_season`]).
    season: Option<ScatterSeason>,
    /// The tint its season copies carry; differs from the season's while a frame in flight holds them.
    season_tint: Option<[f32; 3]>,
    cast_shadows: bool,
    shadow_max_lod: usize,
    /// Squared shadow distance (infinite when the class casts up to its fade end).
    shadow_distance: f32,
    enabled: bool,
    fade_start: f32,
    fade_end: f32,
    lod_bias: f32,
    contact_ao: f32,
    contact_radius: f32,
    /// Tile side, metres.
    tile_side: f32,
    /// Largest bounding radius any of the class's meshes can be placed with, metres.
    max_radius: f32,
    /// How far tiles stream: `fade_end`, or the impostor far end with impostors on.
    stream_end: f32,
    /// Impostor far end (equal to `fade_end` with impostors off).
    far_end: f32,
    /// Where the class's largest instance has shrunk out by its projected size
    /// ([`ScatterDetail`]; infinite without detail limits).
    detail_end: f32,
}

/// Everything tile placement reads; shared with the background placement tasks.
struct PlacerCtx {
    set: ScatterSet,
    source: ScatterSource,
    /// `proto_of[class][mesh]` = prototype index.
    proto_of: Vec<Vec<u32>>,
    shapes: Vec<ProtoShape>,
    /// Placement cells per tile side, per class.
    tile_cells: Vec<i32>,
    key_base: u64,
    /// Where the cell grids are anchored ([`ScatterField::set_placement_origin`]).
    origin: PlacementOrigin,
    /// Tile grid per class under `origin`.
    grids: Vec<TileGrid>,
    /// The heights placement and seating read: `source.map` until the first
    /// [`ScatterField::heights_changed`], then the field's own copy carrying the runtime edits
    /// (ADR-7585). A placement copies the window of it its tile reads (`Self::window`) under the
    /// read lock and lets go (ADR-7638): a tile is placed on one consistent snapshot, and no
    /// background placement holds this `Arc`, so an edit writes its samples in place instead of
    /// copying the map.
    /// Which of those samples runtime edits wrote rides along (ADR-7646).
    heights: std::sync::RwLock<FieldHeights>,
}

/// One class's tile grid: tile `(tx, tz)` covers `corner + (tx, tz) · tile_side` to one tile
/// further; tiles `first..=last` per axis touch the map.
#[derive(Clone, Copy, Debug, PartialEq)]
struct TileGrid {
    corner: [f32; 2],
    first: [i32; 2],
    last: [i32; 2],
}

impl TileGrid {
    fn new(map: &TerrainMap, class: &ScatterClass, cells_per_tile: i32, origin: &PlacementOrigin) -> Self {
        let cells = PlacementGrid::new(map, class.cell_size, origin);
        match origin {
            // The edge-anchored bounds, exactly as before world anchors existed.
            PlacementOrigin::MapEdge => {
                let half = map.world_width() * 0.5;
                let side = cells_per_tile as f32 * class.cell_size;
                let last = ((2.0 * half) / side).ceil() as i32 - 1;
                Self {
                    corner: cells.corner,
                    first: [0, 0],
                    last: [last, last],
                }
            }
            PlacementOrigin::World(_) => Self {
                corner: cells.corner,
                first: cells.first.map(|c| c.div_euclid(cells_per_tile)),
                last: [
                    (cells.end[0] - 1).div_euclid(cells_per_tile),
                    (cells.end[1] - 1).div_euclid(cells_per_tile),
                ],
            },
        }
    }
}

/// The heights a [`ScatterField`] places and seats on, and which of their samples runtime edits
/// wrote (ADR-7646).
struct FieldHeights {
    /// The heights (see [`PlacerCtx`]'s `heights`).
    map: Arc<TerrainMap>,
    /// The samples [`ScatterField::heights_changed`] wrote, recorded by the field itself: a
    /// painted instance placed on that ground takes the edited height whatever map object the
    /// host edited (comparing with the source map cannot tell when the host edited that map).
    edited: EditedSamples,
}

/// One bit per sample of a map: set when a runtime edit wrote the sample (ADR-7646). Empty
/// (no allocation) until the field's first edit or [`ScatterField::prepare_height_edits`];
/// then `width × height / 8` bytes.
#[derive(Debug, Default)]
struct EditedSamples {
    /// The map's sample columns (the row stride).
    width: u32,
    /// The bits, row-major, 64 samples per word.
    bits: Vec<u64>,
}

impl EditedSamples {
    /// Allocates the bits for a `width × height` map (once; a later call keeps them).
    fn reserve(&mut self, width: u32, height: u32) {
        if self.bits.is_empty() {
            self.width = width;
            self.bits = vec![0; (width as usize * height as usize).div_ceil(64)];
        }
    }

    /// Marks sample `index` (row-major) edited.
    fn mark(&mut self, index: usize) {
        if let Some(word) = self.bits.get_mut(index / 64) {
            *word |= 1 << (index % 64);
        }
    }

    /// Whether a runtime edit wrote sample (`col`, `row`).
    fn get(&self, col: u32, row: u32) -> bool {
        let index = row as usize * self.width as usize + col as usize;
        self.bits
            .get(index / 64)
            .is_some_and(|word| (word >> (index % 64)) & 1 == 1)
    }
}

impl PlacerCtx {
    /// The heights seating reads now (main thread only: a clone held across
    /// [`ScatterField::heights_changed`] would make the edit copy the map).
    fn heights(&self) -> Arc<TerrainMap> {
        Arc::clone(
            &self
                .heights
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .map,
        )
    }

    /// The samples (and height range) a placement of the world box `lo..hi` reads, and which of
    /// them runtime edits wrote, copied under the read lock (ADR-7638, ADR-7646).
    fn window(&self, lo: [f32; 2], hi: [f32; 2]) -> crate::sampler::HeightWindow {
        let heights = self
            .heights
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut window = heights.map.window(lo, hi);
        if !heights.edited.bits.is_empty() {
            window.set_edited(|col, row| heights.edited.get(col, row));
        }
        window
    }
}

/// One instance's footprint for the contact map.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContactFootprint {
    /// Ground contact point, metres.
    pub position: [f32; 3],
    /// Footprint radius, metres.
    pub radius: f32,
    /// Darkening at the core (class `contact_ao`).
    pub strength: f32,
}

type TileMap = HashMap<TileKey, Tile, std::hash::BuildHasherDefault<crate::gpu::shared::NodeHasher>>;

/// Most background placement tasks in flight.
const MAX_IN_FLIGHT: usize = 12;
/// Extra background placements in flight for impostor-only tiles.
const MAX_FAR_IN_FLIGHT: usize = 16;

/// A map's scatter: the set, its resolved meshes, and the tiles streamed around the camera.
pub struct ScatterField {
    ctx: Arc<PlacerCtx>,
    protos: Vec<Prototype>,
    /// Every subsurface copy made so far ([`SubsurfaceCopies`]).
    subsurface_copies: SubsurfaceCopies,
    classes: Vec<ClassRuntime>,
    tiles: TileMap,
    /// Active tile keys in emission order (class, tz, tx); rebuilt when the active set changes.
    order: Vec<TileKey>,
    order_dirty: bool,
    /// Background placements in flight.
    pending: HashMap<TileKey, regia_core::tasks::Task<Tile>>,
    background: bool,
    parallel: bool,
    reference: bool,
    enabled: bool,
    full_shadows: bool,
    /// The drawn terrain surface rocks are seated on (ADR-048); `None` = the true heightfield.
    surface: Option<crate::gpu::RenderedSurface>,
    /// Bumped by [`Self::set_rendered_surface`]: seat caches of another surface are stale.
    seat_epoch: u64,
    /// Bumped by every toggle (whole field or a class): the contact map rebuilds on change.
    state_generation: u64,
    tile_budget: usize,
    last_eye: Option<[f32; 3]>,
    /// Instances emitted by the last collect (capacity hint).
    last_emitted: std::sync::atomic::AtomicUsize,
    scratch_wanted: Vec<(f32, TileKey)>,
    scratch_prefetch: Vec<(f32, TileKey)>,
    /// Reused per-chunk record buffers of [`Self::collect`].
    record_pool: std::sync::Mutex<Vec<Vec<Record>>>,
    /// Frames seen ([`Self::update`] calls): the LOD cross-fade clock.
    frame: u64,
    /// LOD switches cross-fade (default); off switches at once (still with hysteresis).
    lod_crossfade: bool,
    /// Impostor far field ([`Self::set_impostors`]); `None` = off (the default).
    impostors: Option<super::impostor::ImpostorSettings>,
    /// Bumped whenever the set of instances [`Self::impostor_instances`] returns may change.
    impostor_generation: u64,
    /// Detail limits ([`Self::set_detail`]); `None` = off (the default).
    detail: Option<ScatterDetail>,
    /// Field-wide shadow LOD hint ([`Self::set_shadow_lod_hint`]); `None` = off (the default).
    shadow_lod_hint: Option<regia_render::ShadowLodHint>,
    /// Each class's copies of its leaf materials under a season, kept with the field.
    season_copies: SeasonCopies,
    /// Bumped when a class's season changes (the GPU impostor layer's prototype table).
    season_generation: u64,
    /// Painted instances ([`Self::set_painted`]), a snapshot background placements share.
    painted: Arc<PaintedScatter>,
    /// Speed-aware LOD ([`Self::set_motion_lod`], ADR-1503); `None` = off (the default).
    motion: Option<super::forest::MotionLod>,
    /// The eye's last step (this update's eye minus the previous one) while motion LOD is on and
    /// the step is not a cut.
    eye_step: Option<Vec3>,
}

/// One emitted instance before it is turned into a [`FrameMesh`] (plain data, so collect
/// chunks can run on the compute pool).
#[derive(Clone, Copy)]
struct Record {
    proto: u32,
    lod: u8,
    variant: u8,
    /// [`Instance::still`].
    still: bool,
    object_key: u64,
    model: [[f32; 4]; 4],
    shadow: ShadowCast,
    /// [`FrameMesh::lod_dither`].
    lod_dither: f32,
    /// Material part (0 = the asset's own, [`ScatterMeshAsset::parts`] from 1).
    part: u8,
    /// A shadow-only draw of the asset's shadow LOD ([`ScatterMeshAsset::shadow_lod`]).
    shadow_only: bool,
    /// A shadow-only draw from the lower-detail mesh keeping one card in this many; 0 draws the LOD's own.
    reduced: u8,
}

/// Grid (metres) the eye snaps to for shadow decisions. Which rocks cast, how large their
/// shadows are and which sun cascades they go to change only when the snapped eye moves to
/// the next grid point, so a moving camera redraws shadow pages in a batch every grid step
/// instead of a page or two every frame, and a held camera never does. (16 m: on DO27's
/// Switchbacks chase rocks redraw 0.31 cascade-1 pages per frame, 8 m gives 0.52, 24 m 0.24.)
const SHADOW_SNAP_M: f32 = 16.0;
/// A fading rock's shadow keeps one of this many sizes (the snapped eye's fade rounded up), so
/// a snap step resizes only the shadows whose step changed, not every shadow in the fade band.
const SHADOW_FADE_STEPS: f32 = 4.0;
/// Fade below which an instance is not drawn.
const MIN_FADE: f32 = 0.02;

/// Visible instances below which collect stays on the calling thread.
const PARALLEL_MIN_INSTANCES: usize = 1500;
/// Instances per parallel collect chunk (roughly).
const CHUNK_INSTANCES: usize = 1200;

/// Key namespace of scatter GPU resources (`FrameMesh` cache keys); disjoint from address keys.
const KEY_TAG: u64 = 0x5CA7_0000_0000_0000;
/// Second mix input of a painted instance's object key (a seeded instance's is its ordinal).
const PAINTED_KEY_TAG: u64 = 0x9A1D_0000_0000_0000;

/// Object key of material part `part` of an instance (part 0 keeps the instance's key).
fn part_object_key(key: u64, part: usize) -> u64 {
    if part == 0 {
        key
    } else {
        mix_key(key, part as u64, 0x9a47)
    }
}

/// The mesh key of a shadow-only draw: the renderer's shadow-LOD tag in the top byte.
fn shadow_only_key(key: u64) -> u64 {
    key | SHADOW_LOD_MESH_TAG
}

fn mix_key(base: u64, a: u64, b: u64) -> u64 {
    let mut h = base ^ 0x9e37_79b9_7f4a_7c15;
    for v in [a, b] {
        h ^= v;
        h = h.wrapping_mul(0x0100_0000_01b3).rotate_left(29);
        h ^= h >> 31;
    }
    KEY_TAG | (h & 0x0000_FFFF_FFFF_FFFF) | 1
}

impl ScatterField {
    /// Resolves the set's meshes and prepares streaming. Nothing is placed until
    /// [`ScatterField::update`].
    ///
    /// `key_base` namespaces the GPU cache keys (two fields on one renderer need different bases).
    ///
    /// # Errors
    ///
    /// [`ScatterError::Asset`] when a mesh or density map the set names is missing or malformed.
    pub fn new(
        set: &ScatterSet,
        source: ScatterSource,
        library: &ScatterLibrary,
        key_base: u64,
    ) -> Result<Self, ScatterError> {
        Self::new_with(set, source, library, key_base, ScatterBuildOptions::default())
    }

    /// [`Self::new`] with build options ([`ScatterBuildOptions`]).
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn new_with(
        set: &ScatterSet,
        source: ScatterSource,
        library: &ScatterLibrary,
        key_base: u64,
        options: ScatterBuildOptions,
    ) -> Result<Self, ScatterError> {
        // Shared materials by mesh path (built on first use).
        let mut shared: HashMap<(&str, usize), (Arc<LabMaterial>, u64)> = HashMap::new();
        let mut protos = Vec::new();
        let mut proto_of = Vec::new();
        for class in &set.classes {
            let mut row = Vec::new();
            for mesh_ref in &class.meshes {
                let asset = library.get(&mesh_ref.mesh).ok_or_else(|| ScatterError::Asset {
                    path: mesh_ref.mesh.clone(),
                    reason: format!("mesh of class '{}' was not supplied", class.name),
                })?;
                asset.validate(&mesh_ref.mesh)?;
                row.push(protos.len() as u32);
                // One shared material per mesh path and material part (part 0 keeps the key a
                // one-material mesh always had).
                let material = options.shared_materials.then(|| {
                    (0..=asset.parts.len())
                        .map(|part| {
                            shared
                                .entry((mesh_ref.mesh.as_str(), part))
                                .or_insert_with(|| {
                                    let id = mesh_ref.mesh.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
                                        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
                                    });
                                    let material = match part {
                                        0 => &asset.material,
                                        p => &asset.parts[p - 1].material,
                                    };
                                    (
                                        Arc::new(material.clone()),
                                        mix_key(key_base, id, 0x5a7e ^ (part as u64) << 32),
                                    )
                                })
                                .clone()
                        })
                        .collect::<Vec<_>>()
                });
                protos.push(build_prototype(
                    class,
                    asset,
                    key_base,
                    protos.len() as u64,
                    material,
                ));
            }
            proto_of.push(row);
            if let Some(path) = &class.density_map
                && !source.density_maps.contains_key(path)
            {
                return Err(ScatterError::Asset {
                    path: path.clone(),
                    reason: format!("density map of class '{}' was not supplied", class.name),
                });
            }
        }
        let tile_cells: Vec<i32> = set
            .classes
            .iter()
            .map(|c| ((TILE_SIZE_M / c.cell_size).round() as i32).max(1))
            .collect();
        let classes = set
            .classes
            .iter()
            .zip(&tile_cells)
            .map(|(c, &cells)| ClassRuntime {
                name: c.name.clone(),
                subsurface: regia_render::SubsurfaceFactors::OFF,
                caster: None,
                season: None,
                season_tint: None,
                cast_shadows: c.cast_shadows,
                shadow_max_lod: usize::from(c.shadow_max_lod),
                shadow_distance: if c.shadow_distance > 0.0 {
                    c.shadow_distance
                } else {
                    f32::INFINITY
                },
                enabled: true,
                fade_start: c.fade_start,
                fade_end: c.fade_end.max(c.fade_start + 1e-3),
                lod_bias: c.lod_bias,
                contact_ao: c.contact_ao,
                contact_radius: c.contact_radius,
                tile_side: cells as f32 * c.cell_size,
                max_radius: 0.0,
                stream_end: c.fade_end.max(c.fade_start + 1e-3),
                far_end: c.fade_end.max(c.fade_start + 1e-3),
                detail_end: f32::INFINITY,
            })
            .collect::<Vec<ClassRuntime>>();
        let mut classes = classes;
        for ((runtime, class), row) in classes.iter_mut().zip(&set.classes).zip(&proto_of) {
            for (mesh_ref, &p) in class.meshes.iter().zip(row) {
                let r = protos[p as usize].shape.radius * mesh_ref.max_scale.max(mesh_ref.min_scale);
                runtime.max_radius = runtime.max_radius.max(r);
            }
        }
        let origin = PlacementOrigin::MapEdge;
        let grids = set
            .classes
            .iter()
            .zip(&tile_cells)
            .map(|(class, &n)| TileGrid::new(&source.map, class, n, &origin))
            .collect();
        let heights = std::sync::RwLock::new(FieldHeights {
            map: Arc::clone(&source.map),
            edited: EditedSamples::default(),
        });
        let ctx = Arc::new(PlacerCtx {
            set: set.clone(),
            heights,
            source,
            proto_of,
            shapes: protos.iter().map(|p: &Prototype| p.shape).collect(),
            tile_cells,
            key_base,
            origin,
            grids,
        });
        Ok(Self {
            subsurface_copies: HashMap::new(),
            ctx,
            protos,
            classes,
            tiles: TileMap::default(),
            order: Vec::new(),
            order_dirty: false,
            pending: HashMap::new(),
            background: true,
            parallel: true,
            reference: false,
            enabled: true,
            full_shadows: false,
            surface: None,
            seat_epoch: 0,
            state_generation: 0,
            tile_budget: usize::MAX,
            last_eye: None,
            last_emitted: std::sync::atomic::AtomicUsize::new(0),
            scratch_wanted: Vec::new(),
            scratch_prefetch: Vec::new(),
            record_pool: std::sync::Mutex::new(Vec::new()),
            frame: 0,
            lod_crossfade: true,
            impostors: None,
            impostor_generation: 0,
            detail: None,
            shadow_lod_hint: None,
            season_copies: SeasonCopies::new(),
            season_generation: 0,
            painted: Arc::new(PaintedScatter::new()),
            motion: None,
            eye_step: None,
        })
    }

    /// The set the field places.
    #[must_use]
    pub fn set(&self) -> &ScatterSet {
        &self.ctx.set
    }

    /// Where the field's placement cell grids are anchored ([`PlacementOrigin`]).
    #[must_use]
    pub fn placement_origin(&self) -> PlacementOrigin {
        self.ctx.origin
    }

    /// [`Self::set_placement_origin`] as a builder step after [`Self::new`].
    #[must_use]
    pub fn with_placement_origin(mut self, origin: PlacementOrigin) -> Self {
        self.set_placement_origin(origin);
        self
    }

    /// Anchors every class's placement cell grid at `origin` (Q-08). The default,
    /// [`PlacementOrigin::MapEdge`], places exactly as before; [`PlacementOrigin::World`] keeps
    /// every rock inside a map where it is when the map grows by a backdrop ring or a low tier
    /// drops it, and is what [`super::colliders::bake_scenery_colliders`] must be given too.
    /// Changing the origin drops every resident and pending tile; the next [`Self::update`]
    /// places them again. Setting the current origin costs one comparison.
    pub fn set_placement_origin(&mut self, origin: PlacementOrigin) {
        if self.ctx.origin == origin {
            return;
        }
        let old = &self.ctx;
        let grids = old
            .set
            .classes
            .iter()
            .zip(&old.tile_cells)
            .map(|(class, &n)| TileGrid::new(&old.source.map, class, n, &origin))
            .collect();
        // A new context, not a mutation: background placements may still share the old one (their
        // tiles are dropped below). The heights and the record of edited samples move over, so a
        // field that owns its edited copy keeps owning it (the next edit copies nothing).
        let heights = {
            let mut h = old
                .heights
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            FieldHeights {
                map: std::mem::replace(&mut h.map, Arc::clone(&old.source.map)),
                edited: std::mem::take(&mut h.edited),
            }
        };
        self.ctx = Arc::new(PlacerCtx {
            set: old.set.clone(),
            source: old.source.clone(),
            proto_of: old.proto_of.clone(),
            shapes: old.shapes.clone(),
            tile_cells: old.tile_cells.clone(),
            key_base: old.key_base,
            origin,
            grids,
            heights: std::sync::RwLock::new(heights),
        });
        self.pending.clear();
        self.tiles.clear();
        self.order.clear();
        self.order_dirty = true;
        self.state_generation += 1;
        self.impostor_generation += 1;
    }

    /// Draws `painted` with the seeded instances, each class's painted instances under its name
    /// and each by its mesh path; what the set no longer names is kept in the store and not
    /// drawn. Only the tiles whose painted instances changed are placed again, on the next
    /// [`Self::update`]; a call with the store already drawn costs one comparison.
    pub fn set_painted(&mut self, painted: &PaintedScatter) {
        if painted.generation() == self.painted.generation() {
            return;
        }
        let _span = tracing::info_span!(target: "regia_terrain", "scatter.set_painted").entered();
        let next = Arc::new(painted.clone());
        for (index, class) in self.classes.iter().enumerate() {
            let side = class.tile_side;
            let corner = self.ctx.grids[index].corner;
            for (px, pz) in next.changed_tiles(&self.painted, &class.name) {
                // The field tiles a storage tile overlaps: its far edge is exclusive.
                let first = |p: i32, c: f32| ((p as f32 * PAINT_TILE_SIZE_M - c) / side).floor() as i32;
                let last =
                    |p: i32, c: f32| (((p + 1) as f32 * PAINT_TILE_SIZE_M - c) / side).ceil() as i32 - 1;
                for tz in first(pz, corner[1])..=last(pz, corner[1]) {
                    for tx in first(px, corner[0])..=last(px, corner[0]) {
                        let key = TileKey {
                            class: index as u16,
                            tx,
                            tz,
                        };
                        self.pending.remove(&key);
                        self.order_dirty |= self.tiles.remove(&key).is_some_and(|tile| tile.active);
                    }
                }
            }
        }
        if self.order_dirty {
            let tiles = &self.tiles;
            self.order.retain(|key| tiles.contains_key(key));
        }
        self.painted = next;
        self.state_generation += 1;
        self.impostor_generation += 1;
    }

    /// The generation of the painted store the field draws ([`PaintedScatter::generation`]).
    #[must_use]
    pub fn painted_generation(&self) -> u64 {
        self.painted.generation()
    }

    /// LOD cross-fade on (default) or off. Off, an instance switches LOD in one frame (the
    /// hysteresis still applies); for A/B reviews and the pop tests.
    pub fn set_lod_crossfade(&mut self, on: bool) {
        self.lod_crossfade = on;
    }

    /// Forgets every instance's LOD, hysteresis and running cross-fade, as on a fresh field: the
    /// next frame takes each instance's LOD from its projected size alone. A render-out job starts
    /// this way, so its frames do not depend on where the camera was before it (ADR-2672).
    pub fn restart_lod(&mut self) {
        for tile in self.tiles.values_mut() {
            tile.lod
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fill(LodState::NEW);
        }
    }

    /// Caps how many tiles one [`ScatterField::update`] places on the calling thread (nearest
    /// first); the rest wait for later updates. Unlimited by default.
    pub fn set_tile_budget(&mut self, tiles: usize) {
        self.tile_budget = tiles.max(1);
    }

    /// Background streaming (default on): tiles one tile side beyond a class's fade range are
    /// placed on the async compute pool ahead of need. A tile the camera needs that is not
    /// ready yet is still placed on the calling thread before [`Self::update`] returns (its
    /// background task is cancelled, never awaited), so what is emitted never depends on
    /// timing and the caller never waits for the pool. Off places everything on the calling
    /// thread.
    pub fn set_background_streaming(&mut self, on: bool) {
        self.background = on;
        if !on {
            self.pending.clear();
        }
    }

    /// Parallel collect (default on): with many visible instances, [`Self::collect`] splits
    /// the tiles into chunks on the compute pool (the calling thread works on them too). Off
    /// keeps collect on the calling thread (a host that already runs it on a worker). The
    /// output is identical either way.
    pub fn set_parallel_collect(&mut self, on: bool) {
        self.parallel = on;
    }

    /// Test hook: the plain algorithm — no tile or block shortcuts, no seat cache, blocking
    /// streaming, serial collect. Every fast path must give the same output as this.
    #[doc(hidden)]
    pub fn set_reference_mode(&mut self, on: bool) {
        self.reference = on;
        if on {
            self.set_background_streaming(false);
            self.parallel = false;
        }
    }

    /// Instances resident.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tiles
            .values()
            .filter(|t| t.active)
            .map(|t| t.instances.len())
            .sum()
    }

    /// True when nothing is resident.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tiles
            .values()
            .filter(|t| t.active)
            .all(|t| t.instances.is_empty())
    }

    /// Whether the field draws at all.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Turns the whole field on or off (agent `terrain.set_scatter`, A/B shots).
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.state_generation += 1;
            self.impostor_generation += 1;
        }
    }

    /// Changes whenever the field or a class is toggled (cheap change detection for hosts).
    #[must_use]
    pub fn state_generation(&self) -> u64 {
        self.state_generation
    }

    /// Seats emitted instances on the surface the terrain draw puts on screen
    /// ([`crate::TerrainSharedDraw::surface`]) instead of the true heightfield, so far rocks on
    /// a coarse CDLOD level neither float nor sink by the level's error. The field's map must
    /// be the draw's map. `None` restores true-height placement.
    pub fn set_rendered_surface(&mut self, surface: Option<crate::gpu::RenderedSurface>) {
        self.surface = surface;
        self.seat_epoch += 1;
    }

    /// A/B switch for reviews and tests: every instance casts into every shadow view
    /// ([`ShadowCast::All`]), ignoring the class shadow policy.
    pub fn set_full_shadows(&mut self, full: bool) {
        self.full_shadows = full;
    }

    /// Class names in file order with their enabled flag.
    #[must_use]
    pub fn classes(&self) -> Vec<(String, bool)> {
        self.classes.iter().map(|c| (c.name.clone(), c.enabled)).collect()
    }

    /// Turns one class on or off. Returns false when no class has that name.
    pub fn set_class_enabled(&mut self, name: &str, enabled: bool) -> bool {
        let mut found = false;
        for class in self.classes.iter_mut().filter(|c| c.name == name) {
            if class.enabled != enabled {
                class.enabled = enabled;
                self.state_generation += 1;
                self.impostor_generation += 1;
            }
            found = true;
        }
        found
    }

    /// Puts subsurface scattering `factors` on every material of the class `name` (every mesh,
    /// part, tint variant and still copy), so a leafy class scatters through the frame's subsurface
    /// path where the render profile asks for it (`RenderProfile::subsurface_scattering`, off in
    /// every preset; ADR-3170): the foliage profile reaches scatter as it reaches mesh leaves.
    /// [`regia_render::SubsurfaceFactors::OFF`] (the default) restores the materials and keys the
    /// field was built with, so the frame is byte-identical to one that never set them. Returns
    /// false when no class has that name.
    ///
    /// A scattering draw leaves the GPU-driven instance batches for the CPU colour loop (ADR-2049),
    /// so a dense class costs a draw per visible instance while the profile scatters. A material
    /// the path cannot scatter keeps its look and counts as unsupported: a swaying class's
    /// materials are material graphs ([`Self::class_subsurface_unsupported`]).
    pub fn set_class_subsurface(&mut self, name: &str, factors: regia_render::SubsurfaceFactors) -> bool {
        let mut found = false;
        let mut changed = Vec::new();
        for (index, class) in self.classes.iter_mut().enumerate() {
            if class.name != name {
                continue;
            }
            found = true;
            // The host applies the agent's overrides every frame: an unchanged class is free.
            if class.subsurface == factors {
                continue;
            }
            class.subsurface = factors;
            changed.push(index);
        }
        for index in changed {
            self.install_class_materials(index);
        }
        found
    }

    /// Puts the class's materials on its prototypes: as built, with the class's subsurface factors
    /// (ADR-3170), and on its leaf parts under its season's tint (ADR-3146). With the factors off and no
    /// tint these are the built `Arc`s and keys, so the frame is byte-identical to one that never set them.
    fn install_class_materials(&mut self, index: usize) {
        let Some(class) = self.classes.get(index) else {
            return;
        };
        let factors = class.subsurface;
        let tint = class.season_tint;
        let Some(protos) = self.ctx.proto_of.get(index) else {
            return;
        };
        for &p in protos {
            let Some(proto) = self.protos.get_mut(p as usize) else {
                continue;
            };
            let leaf_parts = proto.leaf_parts;
            for (part, (materials, keys, still)) in proto.built.clone().into_iter().enumerate() {
                let leaf = part < 32 && leaf_parts & (1 << part) != 0;
                let mut draw = |m: &Arc<LabMaterial>, key: u64| {
                    let (m, key) = with_subsurface(m, key, factors, &mut self.subsurface_copies);
                    match tint.filter(|_| leaf) {
                        Some(tint) => with_season_tint(&m, key, index, tint, &mut self.season_copies),
                        None => (m, key),
                    }
                };
                let (materials, keys): (Vec<_>, Vec<_>) =
                    materials.iter().zip(keys).map(|(m, key)| draw(m, key)).unzip();
                let still: Vec<_> = still.iter().map(|(m, key)| draw(m, *key)).collect();
                if part == 0 {
                    (proto.materials, proto.material_keys, proto.still) = (materials, keys, still);
                } else if let Some(slot) = proto.parts.get_mut(part - 1) {
                    (slot.materials, slot.material_keys, slot.still) = (materials, keys, still);
                }
            }
        }
    }

    /// Sets (`Some`) or clears (`None`, the default) the season of the class `name` (ADR-3146): the
    /// instances that keep their leaves follow [`leaf_present`] on the season's density, and the class's
    /// leaf parts take its tint. Returns false when no class has that name.
    ///
    /// Leaf parts are a mesh's alpha-masked materials; a mesh with none is all leaves. A leafless instance
    /// leaves them out of the frame and the shadows and keeps its other parts (the bark), so a far LOD made
    /// only of a masked billboard draws nothing while leafless. The impostor far field takes the same
    /// answer from the rank every instance holds on the GPU; bake with the season set
    /// ([`Self::bake_impostors`]) to give leafless instances a bare card, otherwise they draw no card.
    ///
    /// No per-instance data changes. The first tint on a class copies its leaf materials once, which
    /// uploads their textures once; every later tint rewrites their parameters. While a frame in flight
    /// holds the copies the tint waits for the next call, so call this every frame with the season wanted
    /// ([`super::control::ScatterControl::apply`] does).
    pub fn set_class_season(&mut self, name: &str, season: Option<ScatterSeason>) -> bool {
        let season = season.filter(|s| !s.is_neutral());
        let Some(index) = self.classes.iter().position(|c| c.name == name) else {
            return false;
        };
        let class = &mut self.classes[index];
        if class.season != season {
            class.season = season;
            // The emitted set and the impostors' prototype table change.
            self.state_generation += 1;
            self.season_generation += 1;
        }
        let wanted = season.map(|s| s.tint()).filter(|tint| *tint != [1.0; 3]);
        if self.classes[index].season_tint == wanted {
            return true;
        }
        // Nothing but this field may hold a copy while it is rewritten: take them off the prototypes.
        let carried = self.classes[index].season_tint.take();
        self.install_class_materials(index);
        let mut held = false;
        if let Some(tint) = wanted {
            for ((class, _), copy) in &mut self.season_copies {
                if *class != index {
                    continue;
                }
                match Arc::get_mut(&mut copy.copy) {
                    Some(material) => material.base_color = tinted(copy.source.base_color, tint),
                    None => held = true,
                }
            }
        }
        // A copy still held keeps the old tint on record, so this comes round again with the next call.
        self.classes[index].season_tint = if held { carried } else { wanted };
        self.install_class_materials(index);
        true
    }

    /// The season of the class `name` ([`Self::set_class_season`]).
    #[must_use]
    pub fn class_season(&self, name: &str) -> Option<ScatterSeason> {
        self.classes
            .iter()
            .find(|c| c.name == name)
            .and_then(|c| c.season)
    }

    /// Changes whenever a class's season changes: [`crate::ImpostorLayer::update`] then rewrites its
    /// prototype table, and no instance.
    #[must_use]
    pub fn season_generation(&self) -> u64 {
        self.season_generation
    }

    /// Per prototype, in field order: the season's tint (RGB) and density (A) of its class, `[1; 4]`
    /// without a season. `ImpProto::season` in `impostor_common.wgsl`.
    pub fn impostor_seasons(&self, out: &mut Vec<[f32; 4]>) {
        out.clear();
        out.resize(self.protos.len(), [1.0; 4]);
        for (class, row) in self.classes.iter().zip(&self.ctx.proto_of) {
            let Some(season) = class.season else { continue };
            let [r, g, b] = season.tint();
            for &p in row {
                if let Some(slot) = out.get_mut(p as usize) {
                    *slot = [r, g, b, season.density()];
                }
            }
        }
    }

    /// Sets (`Some`) or clears (`None`, the default: the frame is unchanged) the shadow casters of the
    /// class `name` past a drawn LOD (ADR-3146): the per-class extension of
    /// [`Self::set_shadow_lod_hint`]. Instances drawn at the hint's `from_lod` or coarser cast from the LOD
    /// its clamp gives, and their leaf parts keep one card in `leaf_keep_one_in`, so near shadows keep
    /// their detail and the far cascades lose triangles. Returns false when no class has that name.
    pub fn set_class_caster_hint(&mut self, name: &str, hint: Option<ScatterCasterHint>) -> bool {
        let hint = hint.filter(|h| !h.is_neutral());
        let Some(index) = self.classes.iter().position(|c| c.name == name) else {
            return false;
        };
        if self.classes[index].caster == hint {
            return true;
        }
        self.classes[index].caster = hint;
        // The emitted set changes.
        self.state_generation += 1;
        let Some(keep) = hint.map(|h| h.keep_one_in()).filter(|keep| *keep > 1) else {
            return true;
        };
        let key_base = self.ctx.key_base;
        let Some(protos) = self.ctx.proto_of.get(index) else {
            return true;
        };
        for &p in protos {
            let Some(proto) = self.protos.get_mut(p as usize) else {
                continue;
            };
            let leaf_parts = proto.leaf_parts;
            let main = std::iter::once(&mut proto.lods);
            let lods = main.chain(proto.parts.iter_mut().map(|part| &mut part.lods));
            for (part, lods) in lods.enumerate() {
                if part >= 32 || leaf_parts & (1 << part) == 0 {
                    continue;
                }
                for lod in lods.iter_mut().filter(|lod| !lod.is_empty()) {
                    if lod.reduced.iter().any(|(k, _, _)| *k == keep) {
                        continue;
                    }
                    if let Some((vertices, indices)) =
                        reduce_cards(&lod.mesh.vertices, &lod.mesh.indices, keep)
                    {
                        let key = mix_key(key_base, lod.mesh_key, 0x3e56 ^ u64::from(keep) << 16);
                        lod.reduced
                            .push((keep, Arc::new(CpuMesh { vertices, indices }), key));
                    }
                }
            }
        }
        true
    }

    /// The caster hint of the class `name` ([`Self::set_class_caster_hint`]).
    #[must_use]
    pub fn class_caster_hint(&self, name: &str) -> Option<ScatterCasterHint> {
        self.classes
            .iter()
            .find(|c| c.name == name)
            .and_then(|c| c.caster)
    }

    /// The subsurface factors of the class `name` ([`Self::set_class_subsurface`]); `None` when no
    /// class has that name.
    #[must_use]
    pub fn class_subsurface(&self, name: &str) -> Option<regia_render::SubsurfaceFactors> {
        self.classes.iter().find(|c| c.name == name).map(|c| c.subsurface)
    }

    /// Why the class `name` asks to scatter and cannot: the first of its materials the frame's
    /// path refuses (`material_graph` for a swaying class, ADR-3170). `None` when the class does
    /// not ask, or when all its materials can scatter.
    #[must_use]
    pub fn class_subsurface_unsupported(&self, name: &str) -> Option<regia_render::SubsurfaceUnsupported> {
        let (index, class) = self.classes.iter().enumerate().find(|(_, c)| c.name == name)?;
        if !class.subsurface.is_on() {
            return None;
        }
        self.ctx
            .proto_of
            .get(index)?
            .iter()
            .filter_map(|&p| self.protos.get(p as usize))
            .flat_map(|proto| {
                proto
                    .materials
                    .iter()
                    .chain(proto.parts.iter().flat_map(|part| part.materials.iter()))
            })
            .find_map(|material| regia_render::SubsurfaceUnsupported::of_material(material, false))
    }

    /// Instances resident per class, in file order.
    #[must_use]
    pub fn resident_per_class(&self) -> Vec<u32> {
        let mut counts = vec![0u32; self.classes.len()];
        for (key, tile) in self.tiles.iter().filter(|(_, t)| t.active) {
            counts[usize::from(key.class)] += tile.instances.len() as u32;
        }
        counts
    }

    /// Streams tiles around `eye`: every missing tile of an enabled class whose nearest point
    /// is inside that class's fade range is made resident (nearest first, up to the tile
    /// budget) — taken from a finished background placement or placed here, never waiting for
    /// an unfinished one — and
    /// tiles more than one tile side beyond it are evicted. With background streaming, the next
    /// ring (one tile side further) is queued on the async compute pool.
    #[tracing::instrument(skip_all, target = "regia_terrain")]
    pub fn update(&mut self, eye: [f32; 3]) -> ScatterStreamStats {
        let mut stats = ScatterStreamStats::default();
        self.eye_step = super::forest::motion_step(self.motion, self.last_eye, eye);
        self.last_eye = Some(eye);
        self.frame += 1;
        // Finished background placements become resident (inactive until needed).
        if !self.pending.is_empty() {
            let done: Vec<TileKey> = self
                .pending
                .iter()
                .filter(|(_, task)| task.is_finished())
                .map(|(key, _)| *key)
                .collect();
            for key in done {
                if let Some(task) = self.pending.remove(&key) {
                    let tile = regia_core::tasks::block_on(task);
                    self.tiles.insert(key, tile);
                }
            }
        }
        let mut wanted = std::mem::take(&mut self.scratch_wanted);
        let mut prefetch = std::mem::take(&mut self.scratch_prefetch);
        // Missing tiles that hold impostors only (wholly past their class's mesh range): placed
        // in the background, never awaited — a far rock that appears a few frames late is
        // never drawn as a mesh, so nothing waits on it.
        let mut far = Vec::new();
        let far_async = self.background && self.impostors.is_some();
        wanted.clear();
        prefetch.clear();
        for (ci, class) in self.classes.iter().enumerate() {
            if !class.enabled {
                continue;
            }
            let side = class.tile_side;
            let need = class.stream_end;
            let reach = if self.background { need + side } else { need };
            let grid = self.ctx.grids[ci];
            let lo =
                |v: f32, a: usize| (((v - reach - grid.corner[a]) / side).floor() as i32).max(grid.first[a]);
            let hi =
                |v: f32, a: usize| (((v + reach - grid.corner[a]) / side).floor() as i32).min(grid.last[a]);
            for tz in lo(eye[2], 1)..=hi(eye[2], 1) {
                for tx in lo(eye[0], 0)..=hi(eye[0], 0) {
                    let d = tile_distance(eye, tx, tz, side, grid.corner);
                    let key = TileKey {
                        class: ci as u16,
                        tx,
                        tz,
                    };
                    if d <= need {
                        match self.tiles.get_mut(&key) {
                            Some(tile) if tile.active => {}
                            Some(tile) => {
                                tile.active = true;
                                self.order_dirty = true;
                                stats.placed_tiles += 1;
                                stats.placed_instances += tile.instances.len() as u32;
                            }
                            None if far_async && d > class.fade_end => {
                                if !self.pending.contains_key(&key) {
                                    far.push((d, key));
                                }
                            }
                            None => wanted.push((d, key)),
                        }
                    } else if d <= reach && !self.tiles.contains_key(&key) && !self.pending.contains_key(&key)
                    {
                        prefetch.push((d, key));
                    }
                }
            }
        }
        wanted.sort_by(|a, b| {
            a.0.total_cmp(&b.0)
                .then((a.1.class, a.1.tz, a.1.tx).cmp(&(b.1.class, b.1.tz, b.1.tx)))
        });
        for (i, (_, key)) in wanted.iter().enumerate() {
            if i >= self.tile_budget {
                stats.pending_tiles = (wanted.len() - i) as u32;
                break;
            }
            let mut tile = match self.pending.remove(key) {
                Some(task) if task.is_finished() => {
                    stats.waited_tiles += 1;
                    regia_core::tasks::block_on(task)
                }
                Some(task) => {
                    // Still queued or running: never wait for the async pool, which other
                    // work (DDGI brick bakes, other placements) may hold for hundreds of
                    // milliseconds on a small CPU. Placement is deterministic, so placing
                    // here gives the same tile; dropping the task cancels it (a placement
                    // already running finishes on its worker and is discarded).
                    drop(task);
                    stats.replaced_tiles += 1;
                    place_tile(&self.ctx, &self.painted, *key)
                }
                None => {
                    stats.sync_tiles += 1;
                    place_tile(&self.ctx, &self.painted, *key)
                }
            };
            tile.active = true;
            self.order_dirty = true;
            stats.placed_tiles += 1;
            stats.placed_instances += tile.instances.len() as u32;
            self.tiles.insert(*key, tile);
        }
        let classes = &self.classes;
        let grids = &self.ctx.grids;
        let before = self.tiles.len();
        let keep = |key: &TileKey, active: bool| {
            let class = &classes[usize::from(key.class)];
            let limit = if active {
                class.stream_end + class.tile_side
            } else {
                class.stream_end + 2.0 * class.tile_side
            };
            class.enabled
                && tile_distance(
                    eye,
                    key.tx,
                    key.tz,
                    class.tile_side,
                    grids[usize::from(key.class)].corner,
                ) <= limit
        };
        let mut evicted_active = false;
        self.tiles.retain(|key, tile| {
            let kept = keep(key, tile.active);
            evicted_active |= !kept && tile.active;
            kept
        });
        self.order_dirty |= evicted_active;
        stats.evicted_tiles = (before - self.tiles.len()) as u32;
        self.pending.retain(|key, _| keep(key, false));
        if self.background {
            prefetch.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (_, key) in prefetch.iter() {
                if self.pending.len() >= MAX_IN_FLIGHT {
                    break;
                }
                let ctx = Arc::clone(&self.ctx);
                let painted = Arc::clone(&self.painted);
                let key = *key;
                let task = regia_core::tasks::spawn(async move { place_tile(&ctx, &painted, key) });
                self.pending.insert(key, task);
                stats.queued_tiles += 1;
            }
            far.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (_, key) in &far {
                if self.pending.len() >= MAX_IN_FLIGHT + MAX_FAR_IN_FLIGHT {
                    break;
                }
                let ctx = Arc::clone(&self.ctx);
                let painted = Arc::clone(&self.painted);
                let key = *key;
                let task = regia_core::tasks::spawn(async move { place_tile(&ctx, &painted, key) });
                self.pending.insert(key, task);
                stats.queued_tiles += 1;
            }
        }
        if self.order_dirty {
            self.order.clear();
            self.order
                .extend(self.tiles.iter().filter(|(_, t)| t.active).map(|(k, _)| *k));
            self.order.sort_by_key(|k| (k.class, k.tz, k.tx));
            self.order_dirty = false;
            self.impostor_generation += 1;
        }
        self.scratch_wanted = wanted;
        self.scratch_prefetch = prefetch;
        stats
    }

    /// [`Self::update`] until nothing streams any more: every tile in range, impostor-only
    /// tiles included, is resident and active. For level loads and still frames (a camera
    /// cut, an evidence shot); a moving camera keeps calling [`Self::update`], which never
    /// waits for impostor-only tiles.
    pub fn settle_streaming(&mut self, eye: [f32; 3]) -> ScatterStreamStats {
        let mut total = self.update(eye);
        while !self.pending.is_empty() {
            for (key, task) in std::mem::take(&mut self.pending) {
                let tile = regia_core::tasks::block_on(task);
                self.tiles.insert(key, tile);
            }
            let stats = self.update(eye);
            total.placed_tiles += stats.placed_tiles;
            total.placed_instances += stats.placed_instances;
            total.evicted_tiles += stats.evicted_tiles;
            total.waited_tiles += stats.waited_tiles;
            total.replaced_tiles += stats.replaced_tiles;
            total.queued_tiles += stats.queued_tiles;
            total.sync_tiles += stats.sync_tiles;
            total.pending_tiles = stats.pending_tiles;
        }
        total
    }

    /// Takes the field's own copy of the heights now, if the host still shares the map the field
    /// was built from (ADR-7638): call it at load, where a frame may be long, so the first
    /// [`Self::heights_changed`] in play writes in place too. Returns the bytes copied (0 when
    /// the field already owns its heights). Costs one copy of the map's samples in memory, plus
    /// the record of which samples edits wrote (one bit per sample, ADR-7646), allocated here
    /// too so the first edit allocates neither.
    pub fn prepare_height_edits(&mut self) -> u64 {
        let mut heights = self
            .ctx
            .heights
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let FieldHeights { map, edited } = &mut *heights;
        edited.reserve(map.width(), map.height());
        if Arc::get_mut(map).is_some() {
            return 0;
        }
        let _span = tracing::info_span!(target: "regia_terrain", "scatter.prepare_height_edits").entered();
        let bytes = std::mem::size_of_val(map.heights()) as u64;
        Arc::make_mut(map);
        bytes
    }

    /// Takes a runtime height edit (ADR-7585, spec 25 § 7.4): copies the samples of `rect` from
    /// `map` (the edited map; the same grid as the field's) into the heights the field places
    /// and seats on, then re-sits the instances standing on the changed ground. Only resident
    /// tiles whose area meets the samples' footprint, grown by one sample spacing (the reach
    /// of bilinear interpolation), are visited, in (class, row, column) order; inside them only
    /// instances whose contact height changed move, vertically, by the change (orientation and
    /// scale stay). Every other instance keeps its exact matrix. Background placements under
    /// the box are dropped and redone from the new heights when needed, so a tile placed after
    /// the edit stands on the new ground too (its placement rules see the new slope), painted
    /// instances on the changed ground included (they take the edited height).
    ///
    /// Placements copy only their tile's window of the heights (ADR-7638), so the edit writes the
    /// samples in place: the whole map is copied once, on the first edit, only while the host
    /// still shares the map the field was built from ([`ScatterResitStats::heights_copied_bytes`]).
    ///
    /// Called by [`crate::resit_runtime_scatter`] for every [`crate::TerrainHeightsChanged`];
    /// a host driving its own field calls it per message. A `map` of another grid or an empty
    /// `rect` does nothing.
    pub fn heights_changed(
        &mut self,
        map: &TerrainMap,
        rect: &crate::deform::DirtyRect,
    ) -> ScatterResitStats {
        let mut stats = ScatterResitStats {
            tiles_resident: self.tiles.len() as u32,
            ..ScatterResitStats::default()
        };
        if rect.is_empty() || rect.col_end > map.width() || rect.row_end > map.height() {
            return stats;
        }
        let _span = tracing::info_span!(target: "regia_terrain", "scatter_resit").entered();
        {
            let mut heights = self
                .ctx
                .heights
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let FieldHeights {
                map: field_map,
                edited,
            } = &mut *heights;
            if field_map.width() != map.width() || field_map.height() != map.height() {
                return stats;
            }
            // Copy-on-write: the first edit copies the map shared with the caller once. Nothing
            // else holds this `Arc` (placements copy windows, ADR-7638), so later edits write
            // in place.
            if Arc::strong_count(field_map) > 1 || Arc::weak_count(field_map) > 0 {
                stats.heights_copied_bytes = std::mem::size_of_val(field_map.heights()) as u64;
            }
            let own = Arc::make_mut(field_map);
            // The field records which samples it was told changed (ADR-7646): placement reads
            // that record, never a comparison with the source map, which may be the edited map.
            edited.reserve(map.width(), map.height());
            let stride = map.width() as usize;
            for row in rect.row_start..rect.row_end {
                let base = row as usize * stride;
                for col in rect.col_start..rect.col_end {
                    let index = base + col as usize;
                    own.set_height(index, map.height_at_index(index));
                    edited.mark(index);
                }
            }
        }
        let heights = self.ctx.heights();
        let target = crate::runtime_patch::PatchTarget::of(&heights);
        let ([x0, z0], [x1, z1]) = target.footprint(rect);
        // Bilinear heights change up to one cell past the changed samples.
        let (gx, gz) = (heights.sample_spacing(), heights.sample_spacing_z());
        let (lo, hi) = ([x0 - gx, z0 - gz], [x1 + gx, z1 + gz]);
        let inside = |p: &[f32; 3]| p[0] > lo[0] && p[0] < hi[0] && p[2] > lo[1] && p[2] < hi[1];
        let classes = &self.classes;
        let grids = &self.ctx.grids;
        // Tile boxes follow the placement origin (Q-08): under a world anchor tile (0, 0) starts
        // at the anchor and indices may be negative.
        let tile_meets = |key: &TileKey, grow: f32| {
            let side = classes[usize::from(key.class)].tile_side;
            let corner = grids[usize::from(key.class)].corner;
            let (tx0, tz0) = (corner[0] + key.tx as f32 * side, corner[1] + key.tz as f32 * side);
            tx0 <= hi[0] + grow
                && tx0 + side >= lo[0] - grow
                && tz0 <= hi[1] + grow
                && tz0 + side >= lo[1] - grow
        };
        let before = self.pending.len();
        // A placement's slope test (`normal_at`) reads one sample past the bilinear reach.
        let normal_reach = gx.max(gz);
        self.pending.retain(|key, _| !tile_meets(key, normal_reach));
        stats.pending_dropped = (before - self.pending.len()) as u32;
        let mut keys: Vec<TileKey> = self
            .tiles
            .keys()
            .copied()
            .filter(|k| tile_meets(k, 0.0))
            .collect();
        keys.sort_by_key(|k| (k.class, k.tz, k.tx));
        // The height placement reads under the field's origin, so a re-sat instance stands where a
        // placement after the edit puts it, bit for bit (world-anchored lattice reads, Q-08).
        let ground = Ground::new(&*heights, &self.ctx.origin);
        for key in keys {
            let Some(tile) = self.tiles.get_mut(&key) else {
                continue;
            };
            stats.tiles_touched += 1;
            let seat = tile
                .seat
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut moved = 0;
            for (inst, entry) in tile.instances.iter_mut().zip(seat.iter_mut()) {
                if !inside(&inst.ground) {
                    continue;
                }
                let y = ground.height(&*heights, inst.ground[0], inst.ground[2]);
                if y.to_bits() == inst.ground[1].to_bits() {
                    continue;
                }
                let dy = y - inst.ground[1];
                inst.ground[1] = y;
                inst.centre[1] += dy;
                inst.model[3][1] += dy;
                entry.valid = false;
                moved += 1;
            }
            if moved > 0 {
                let (centre, radius, blocks) = tile_bounds(&tile.instances);
                tile.centre = centre;
                tile.radius = radius;
                tile.blocks = blocks;
                stats.tiles_rebuilt += 1;
                stats.instances_resat += moved;
            }
        }
        if stats.tiles_rebuilt > 0 {
            // The impostor layer and the contact map read instance positions.
            self.impostor_generation += 1;
            self.state_generation += 1;
        }
        stats
    }

    /// Ground contact points of the resident instances, in tile (class, row, column) and
    /// placement order: for tests and the agent (what a re-sit moved).
    #[must_use]
    pub fn instance_grounds(&self) -> Vec<[f32; 3]> {
        let mut keys: Vec<&TileKey> = self.tiles.keys().collect();
        keys.sort_by_key(|k| (k.class, k.tz, k.tx));
        keys.into_iter()
            .flat_map(|k| self.tiles[k].instances.iter().map(|inst| inst.ground))
            .collect()
    }

    /// Model matrices of the resident instances, in [`Self::instance_grounds`] order.
    #[must_use]
    pub fn instance_models(&self) -> Vec<[[f32; 4]; 4]> {
        let mut keys: Vec<&TileKey> = self.tiles.keys().collect();
        keys.sort_by_key(|k| (k.class, k.tz, k.tx));
        keys.into_iter()
            .flat_map(|k| self.tiles[k].instances.iter().map(|inst| inst.model))
            .collect()
    }

    /// Footprints of the drawn instances whose contact point lies inside the XZ square
    /// `centre ± half` (contact map input). Disabled classes and a disabled field yield nothing.
    pub fn footprints_in(&self, centre: [f32; 2], half: f32, mut visit: impl FnMut(ContactFootprint)) {
        if !self.enabled {
            return;
        }
        for key in &self.order {
            let Some(tile) = self.tiles.get(key) else { continue };
            let class = &self.classes[usize::from(key.class)];
            if !class.enabled || class.contact_ao <= 0.0 || tile.instances.is_empty() {
                continue;
            }
            if (tile.centre[0] - centre[0]).abs() > half + tile.radius
                || (tile.centre[2] - centre[1]).abs() > half + tile.radius
            {
                continue;
            }
            for inst in &tile.instances {
                if (inst.ground[0] - centre[0]).abs() > half || (inst.ground[2] - centre[1]).abs() > half {
                    continue;
                }
                visit(ContactFootprint {
                    position: inst.ground,
                    radius: inst.footprint * class.contact_radius,
                    strength: class.contact_ao,
                });
            }
        }
    }

    /// Appends this frame's visible-candidate instances to `out` (borrowing mesh data from the
    /// field) and returns what was emitted. Call [`ScatterField::update`] with the same eye first.
    ///
    /// Tiles outside the frustum or beyond their class's fade range are skipped whole; the GPU
    /// culls the rest per instance (frustum + Hi-Z). Many visible instances are processed in
    /// tile chunks on the compute pool; the output order (and every value) is the same as a
    /// single pass.
    #[tracing::instrument(skip_all, target = "regia_terrain")]
    pub fn collect<'s>(&'s self, view: &ScatterView, out: &mut Vec<FrameMesh<'s>>) -> ScatterFrameStats {
        out.reserve(self.last_emitted.load(std::sync::atomic::Ordering::Relaxed));
        self.collect_records(view, None, &mut |r| out.push(self.mesh_of(r)))
    }

    /// [`Self::collect`] for the engine's ECS frame ([`regia_render::ExtractedFrame`]): appends
    /// this frame's instances to `out` as [`ExtractedMesh`]es that share the field's meshes and
    /// materials (`Arc`s held by the field, so the renderer's mesh and material caches see the
    /// same keys every frame), and returns what was emitted. Same instances, LODs, fades,
    /// dithers and shadow policies as [`Self::collect`], in the same order; the object keys are
    /// the field's (`entity` is [`regia_ecs::prelude::Entity::PLACEHOLDER`] and
    /// `object_key_xor` carries the key). A shadow-only draw carries
    /// [`SHADOW_LOD_MESH_TAG`] in `lod_key`. Call [`Self::update`] with the same eye first.
    #[tracing::instrument(skip_all, target = "regia_terrain")]
    pub fn extract_meshes(&self, view: &ScatterView, out: &mut Vec<ExtractedMesh>) -> ScatterFrameStats {
        out.reserve(self.last_emitted.load(std::sync::atomic::Ordering::Relaxed));
        self.collect_records(view, None, &mut |r| out.push(self.extracted_of(r)))
    }

    /// [`Self::extract_meshes`] for a view that may stand inside a cell (plan row W-22, ADR-8413):
    /// when `portals` [restricts](regia_render::viscells::OutdoorPortals::restricts) the view's eye,
    /// the instance blocks that lie in none of its outdoor frusta (widened by the instances'
    /// radius and shadow reach), and are not casters whose shadow can reach one of its mouths, are
    /// left out ([`ScatterFrameStats::portal_culled`]). Any other list (unrestricted, another eye)
    /// emits exactly what [`Self::extract_meshes`] does.
    #[tracing::instrument(skip_all, target = "regia_terrain")]
    pub fn extract_meshes_through(
        &self,
        view: &ScatterView,
        portals: &regia_render::viscells::OutdoorPortals,
        out: &mut Vec<ExtractedMesh>,
    ) -> ScatterFrameStats {
        out.reserve(self.last_emitted.load(std::sync::atomic::Ordering::Relaxed));
        let portals = Some(portals).filter(|p| p.restricts(view.eye));
        self.collect_records(view, portals, &mut |r| out.push(self.extracted_of(r)))
    }

    /// Culls, LOD-selects and fades the visible tiles and hands each emitted draw to `sink`, in
    /// emission order (the order is the same with and without the parallel chunks).
    fn collect_records(
        &self,
        view: &ScatterView,
        portals: Option<&regia_render::viscells::OutdoorPortals>,
        sink: &mut dyn FnMut(&Record),
    ) -> ScatterFrameStats {
        let mut stats = ScatterFrameStats {
            resident: self.len() as u32,
            tiles: self.order.len() as u32,
            ..ScatterFrameStats::default()
        };
        if !self.enabled {
            return stats;
        }
        let frustum = Frustum::from_view_proj(&Mat4::from_cols_array_2d(&view.view_proj));
        let eye = Vec3::from_array(view.eye);
        let (shadow_eye, snap_err) = snapped_eye(eye);
        // Tiles that the camera at the snapped eye could see (with the snap as margin): a
        // superset of what the camera sees, and fixed between snap steps under translation.
        let snap_shift = eye - shadow_eye;
        // Visible tiles, in emission order, with whether each lies wholly inside the frustum.
        let mut visible: Vec<(&Tile, &ClassRuntime, bool)> = Vec::with_capacity(self.order.len());
        let mut instances = 0usize;
        for key in &self.order {
            let Some(tile) = self.tiles.get(key) else { continue };
            let class = &self.classes[usize::from(key.class)];
            if !class.enabled || tile.instances.is_empty() {
                continue;
            }
            let to_tile = Vec3::from_array(tile.centre).distance(eye);
            if to_tile - tile.radius > class.fade_end.min(class.detail_end)
                || !frustum.intersects_sphere(
                    (Vec3::from_array(tile.centre) + snap_shift).to_array(),
                    tile.radius + snap_err,
                )
            {
                continue;
            }
            // Every instance sphere lies inside the tile sphere, so a tile wholly inside the
            // frustum passes every per-instance test.
            let inside = !self.reference
                && frustum.planes().iter().all(|p| {
                    p[0] * tile.centre[0] + p[1] * tile.centre[1] + p[2] * tile.centre[2] + p[3]
                        >= tile.radius
                });
            instances += tile.instances.len();
            visible.push((tile, class, inside));
        }
        stats.tiles_visible = visible.len() as u32;
        let mut run = |q: Option<&crate::gpu::shared::SurfaceQuery<'_>>| {
            if self.parallel && instances >= PARALLEL_MIN_INSTANCES {
                // Contiguous runs of tiles holding about CHUNK_INSTANCES instances each.
                let mut ranges = Vec::new();
                let (mut start, mut acc) = (0usize, 0usize);
                for (i, (tile, _, _)) in visible.iter().enumerate() {
                    acc += tile.instances.len();
                    if acc >= CHUNK_INSTANCES {
                        ranges.push(start..i + 1);
                        start = i + 1;
                        acc = 0;
                    }
                }
                if start < visible.len() {
                    ranges.push(start..visible.len());
                }
                let visible = &visible;
                let chunks = regia_core::tasks::scope(|scope| {
                    for range in ranges {
                        // Made here, a child of `collect`'s span; entered on the pool thread (ADR-1378).
                        let span = tracing::info_span!(target: "regia_terrain", "scatter_collect_tiles");
                        scope.spawn(async move {
                            span.in_scope(|| self.collect_tiles(&visible[range], view, &frustum, portals, q))
                        });
                    }
                });
                for (records, chunk) in chunks {
                    merge_stats(&mut stats, &chunk);
                    for record in &records {
                        sink(record);
                    }
                    self.give_back(records);
                }
            } else {
                let chunk = self.cull_tiles(&visible, view, &frustum, portals, q, |r| sink(&r));
                merge_stats(&mut stats, &chunk);
            }
        };
        match &self.surface {
            Some(surface) => surface.query(&self.ctx.heights(), run),
            None => run(None),
        }
        self.last_emitted
            .store(stats.emitted as usize, std::sync::atomic::Ordering::Relaxed);
        regia_core::telemetry::set_gauge(
            regia_core::telemetry::Gauge::ScatterEmitted,
            u64::from(stats.emitted),
        );
        stats
    }

    /// The mesh (tint variant applied) and material a record draws.
    fn draw_of(&self, r: &Record) -> (&Arc<CpuMesh>, u64, &Arc<LabMaterial>, u64) {
        let proto = &self.protos[r.proto as usize];
        let part = usize::from(r.part);
        // `emit_parts` emits only parts that hold the LOD; part 0's LOD 0 always exists.
        let lod = proto.part_lod(part, usize::from(r.lod)).unwrap_or(&proto.lods[0]);
        let (mesh, mesh_key) = lod
            .reduced(r.reduced)
            .unwrap_or_else(|| lod.variant(usize::from(r.variant)));
        let (material, material_key) = proto.part_material(part, usize::from(r.variant), r.still);
        let mesh_key = if r.shadow_only {
            shadow_only_key(mesh_key)
        } else {
            mesh_key
        };
        (mesh, mesh_key, material, material_key)
    }

    fn mesh_of<'s>(&'s self, r: &Record) -> FrameMesh<'s> {
        let (mesh, mesh_key, material, material_key) = self.draw_of(r);
        FrameMesh {
            is_static: false,
            mesh_key,
            material_key,
            object_key: r.object_key,
            vertices: &mesh.vertices,
            indices: &mesh.indices,
            material,
            model: r.model,
            joints: &[],
            // Rocks are static ground dressing: decals land on them like on terrain.
            receiver_layer: 0,
            shadow: r.shadow,
            uv1: &[],
            lod_dither: r.lod_dither,
        }
    }

    fn extracted_of(&self, r: &Record) -> ExtractedMesh {
        let (mesh, _, material, _) = self.draw_of(r);
        let anchor = regia_ecs::prelude::Entity::PLACEHOLDER;
        ExtractedMesh {
            is_static: false,
            entity: anchor,
            mesh: Arc::clone(mesh),
            material: Arc::clone(material),
            layer: 0,
            model: r.model,
            joints: Vec::new(),
            // Static ground dressing: decals land on it like on terrain (as `mesh_of`).
            decal_layer: 0,
            uv1: None,
            shadow: r.shadow,
            draw_indices: None,
            lod_key: if r.shadow_only { SHADOW_LOD_MESH_TAG } else { 0 },
            object_key_xor: r.object_key ^ anchor.to_bits(),
            // Thousands of instances: no per-instance GI brick.
            gi_proxy: false,
            lod_dither: r.lod_dither,
        }
    }

    /// Emits `record` once per material part that holds triangles at its LOD (part 0 is the
    /// asset's own mesh), and, when the asset casts from a coarser shadow LOD than the drawn
    /// one ([`ScatterMeshAsset::shadow_lod`]) or from a lower-detail mesh
    /// ([`Self::set_class_caster_hint`]), moves the shadow to shadow-only draws of that mesh. A leafless
    /// instance ([`Self::set_class_season`]) leaves its leaf parts out, in the frame and in the shadows.
    /// Returns the triangles it emitted into shadow maps.
    fn emit_parts(
        &self,
        record: Record,
        class: &ClassRuntime,
        leafless: bool,
        emit: &mut impl FnMut(Record),
    ) -> u64 {
        let proto = &self.protos[record.proto as usize];
        let lod = usize::from(record.lod);
        let (shadow_lod, keep) = self.caster_for(class, proto, lod);
        let keep_at = |part: usize| {
            proto
                .part_lod(part, shadow_lod)
                .filter(|_| proto.is_leaf(part))
                .and_then(|l| l.reduced(keep).map(|_| keep))
                .unwrap_or(0)
        };
        let thins = keep > 1 && (0..=proto.parts.len()).any(|part| keep_at(part) != 0);
        let casting = record.shadow != ShadowCast::Off;
        let split = (shadow_lod != lod || thins) && casting;
        let drawn = if split {
            Record {
                shadow: ShadowCast::Off,
                ..record
            }
        } else {
            record
        };
        let skip = |part: usize| leafless && proto.is_leaf(part);
        let triangles = |part_lod: &ProtoLod, reduced: u8| {
            let (mesh, _) = part_lod.reduced(reduced).unwrap_or((&part_lod.mesh, 0));
            mesh.indices.len() as u64 / 3
        };
        let mut caster_triangles = 0;
        for part in 0..=proto.parts.len() {
            if skip(part) {
                continue;
            }
            if let Some(part_lod) = proto.part_lod(part, lod).filter(|l| !l.is_empty()) {
                if casting && !split {
                    caster_triangles += triangles(part_lod, 0);
                }
                emit(Record {
                    part: part as u8,
                    object_key: part_object_key(drawn.object_key, part),
                    ..drawn
                });
            }
        }
        if split {
            for part in 0..=proto.parts.len() {
                if skip(part) {
                    continue;
                }
                if let Some(part_lod) = proto.part_lod(part, shadow_lod).filter(|l| !l.is_empty()) {
                    let reduced = keep_at(part);
                    caster_triangles += triangles(part_lod, reduced);
                    emit(Record {
                        lod: shadow_lod as u8,
                        part: part as u8,
                        object_key: part_object_key(record.object_key ^ SHADOW_LOD_OBJECT_TAG, part),
                        lod_dither: regia_render::LOD_DITHER_NONE,
                        shadow_only: true,
                        reduced,
                        ..record
                    });
                }
            }
        }
        caster_triangles
    }

    /// The LOD an instance of `class` drawn at `lod` casts its shadow from, and of how many cards a leaf
    /// part keeps one there (1 = all): the asset's own shadow LOD (never finer than the drawn one), then
    /// the field-wide hint ([`Self::set_shadow_lod_hint`]), then the class's from its first LOD on
    /// ([`Self::set_class_caster_hint`]).
    fn caster_for(&self, class: &ClassRuntime, proto: &Prototype, lod: usize) -> (usize, u8) {
        let chain = proto.lods.len();
        let base = proto.shadow_lod.max(lod);
        let base = self.shadow_lod_hint.map_or(base, |hint| hint.apply(base, chain));
        match class.caster.filter(|hint| lod >= usize::from(hint.from_lod)) {
            Some(hint) => (hint.lod().apply(base, chain), hint.keep_one_in()),
            None => (base, 1),
        }
    }

    fn take_buffer(&self) -> Vec<Record> {
        self.record_pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .unwrap_or_default()
    }

    fn give_back(&self, mut records: Vec<Record>) {
        records.clear();
        self.record_pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(records);
    }

    /// Culls, fades, LOD-selects and seats the instances of `tiles` (one chunk of collect).
    fn collect_tiles(
        &self,
        tiles: &[(&Tile, &ClassRuntime, bool)],
        view: &ScatterView,
        frustum: &Frustum,
        portals: Option<&regia_render::viscells::OutdoorPortals>,
        surface: Option<&crate::gpu::shared::SurfaceQuery<'_>>,
    ) -> (Vec<Record>, ScatterFrameStats) {
        let mut records = self.take_buffer();
        let stats = self.cull_tiles(tiles, view, frustum, portals, surface, |r| records.push(r));
        (records, stats)
    }

    /// The per-instance part of collect, handing each emitted instance to `emit` in order.
    fn cull_tiles(
        &self,
        tiles: &[(&Tile, &ClassRuntime, bool)],
        view: &ScatterView,
        frustum: &Frustum,
        portals: Option<&regia_render::viscells::OutdoorPortals>,
        surface: Option<&crate::gpu::shared::SurfaceQuery<'_>>,
        mut emit: impl FnMut(Record),
    ) -> ScatterFrameStats {
        let mut stats = ScatterFrameStats::default();
        let planes = frustum.planes();
        let eye = Vec3::from_array(view.eye);
        let inv_tan = 1.0 / (view.fov_y * 0.5).tan().max(1e-4);
        let sun_elevation = sun_elevation(view.sun_dir);
        let (shadow_eye, snap_err) = snapped_eye(eye);
        let snap_shift = eye - shadow_eye;
        // Block test from the snapped eye, widened by the snap and the sun-following shadow
        // reach: true whenever the block's own test passes, and fixed between snap steps while
        // the camera only translates. A low sun throws a longer shadow, so the reach — and the
        // margin that keeps an off-screen caster whose shadow reaches in — follows it.
        let snapped_visible = |block: &Block| {
            let c = Vec3::from_array(block.centre) + snap_shift;
            let cull = block.reach + local_shadow_reach(block.max_radius, sun_elevation) + SHADOW_MARGIN_M;
            planes
                .iter()
                .all(|p| p[0] * c.x + p[1] * c.y + p[2] * c.z + p[3] >= -(cull + snap_err))
        };
        let generation = [self.seat_epoch, surface.map_or(0, |q| q.heights_generation)];
        let heights = self.ctx.heights();
        let impostors = self.impostors.is_some();
        for &(tile, class, tile_inside) in tiles {
            // Holes cut through the drawn surface (tunnel mouths): no ground to stand on.
            let tile_holes = surface.and_then(|q| q.holes()).filter(|m| {
                m.intersects_rect(
                    tile.centre[0] - tile.radius,
                    tile.centre[2] - tile.radius,
                    2.0 * tile.radius,
                )
            });
            let mut seat = surface.map(|_| {
                tile.seat
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            });
            let mut lod_states = tile.lod.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // Per block: 0 = every instance fails the cull (the snapped one: a superset of the
            // camera's) or the fade (skip), 1 = test each instance, 2 = every instance passes
            // the frustum test (only the fade remains).
            let mut block_state = [1u8; (BLOCKS * BLOCKS) as usize];
            for (state, block) in block_state
                .iter_mut()
                .zip(&tile.blocks)
                .filter(|_| !self.reference)
            {
                if block.count == 0 {
                    *state = 0;
                    continue;
                }
                let dist = |p: &[f32; 4]| {
                    p[0] * block.centre[0] + p[1] * block.centre[1] + p[2] * block.centre[2] + p[3]
                };
                let block_end = self.detail.map_or(class.fade_end, |d| {
                    class.fade_end.min(d.end_distance(block.max_radius))
                });
                if Vec3::from_array(block.centre).distance(eye) - block.reach >= block_end
                    || (!tile_inside && !snapped_visible(block))
                {
                    *state = 0;
                } else if tile_inside || planes.iter().all(|p| dist(p) >= block.reach) {
                    *state = 2;
                }
            }
            // Inside a cell (plan row W-22, ADR-8413): a block in none of the outdoor frusta,
            // widened like the frustum test by the instances' radius and shadow reach, lies behind
            // the cell's walls. A caster block whose shadow can reach a mouth stays: the sun lights
            // the inside of the cell through it.
            if let Some(portals) = portals {
                for (state, block) in block_state.iter_mut().zip(&tile.blocks) {
                    if *state == 0 || block.count == 0 {
                        continue;
                    }
                    let cull = block.reach
                        + block.max_radius
                        + local_shadow_reach(block.max_radius, sun_elevation)
                        + SHADOW_MARGIN_M
                        + snap_err;
                    if portals.admits_sphere(block.centre, cull)
                        || (class.cast_shadows && portals.near_mouth(block.centre, cull))
                    {
                        continue;
                    }
                    *state = 0;
                    stats.portal_culled += block.count;
                }
            }
            // Blocks wholly past the class's shadow distance (from the snapped eye) cast nothing,
            // so their instances skip the shadow decision.
            let may_cast: [bool; (BLOCKS * BLOCKS) as usize] = std::array::from_fn(|b| {
                let block = &tile.blocks[b];
                !self.full_shadows
                    && class.cast_shadows
                    && Vec3::from_array(block.centre).distance(shadow_eye) - block.reach
                        <= class.shadow_distance
            });
            for (index, inst) in tile.instances.iter().enumerate() {
                let state = block_state[usize::from(inst.block)];
                if state == 0 {
                    continue;
                }
                if let Some(holes) = tile_holes
                    && over_hole(holes, inst.ground, inst.radius)
                {
                    continue;
                }
                stats.considered += 1;
                let proto = &self.protos[inst.proto as usize];
                // Whether the instance casts and its shadow's fade, decided from the snapped
                // eye. A caster is always drawn: the real eye is within `snap_err` of the
                // snapped one, so its own fade stays above MIN_FADE.
                let may = may_cast[usize::from(inst.block)];
                let shadow_d = if may {
                    Vec3::from_array(inst.centre).distance(shadow_eye)
                } else {
                    f32::MAX
                };
                let d = Vec3::from_array(inst.centre).distance(eye);
                // The screen-size fade (1 without detail limits) scales the shadow continuously
                // from the real eye: the snapped, quantized steps below are for the distance
                // fade, whose band lies where shadow pages are coarse; a rock shrinking by its
                // own size would step its shadow visibly. With the recommended limits the band
                // lies past the small classes' shadow distance, so few casters are in it.
                let detail_now = self.detail.map_or(1.0, |s| s.fade(inst.radius, d));
                let may = may && self.detail.is_none_or(|s| s.casts(inst.radius));
                let shadow_fade = if may { fade_at(class, shadow_d) } else { 0.0 };
                let shadow_size = (shadow_fade * SHADOW_FADE_STEPS).ceil() / SHADOW_FADE_STEPS * detail_now;
                let casts = may
                    && shadow_d <= class.shadow_distance
                    && fade_at(class, shadow_d + snap_err) >= MIN_FADE
                    && detail_now >= MIN_FADE
                    && lod_at(
                        proto,
                        inst.radius * shadow_fade * inv_tan / shadow_d.max(1e-3) * class.lod_bias,
                    ) <= class.shadow_max_lod;
                // Coarse CPU cull: the frame renderer draws every emitted rigid mesh into every
                // sun cascade, so instances that can neither be seen nor shadow the view are not
                // emitted. The margin covers the bounding sphere plus the sun-following shadow
                // reach, so a shadow reaching in from just off screen keeps its caster; the GPU
                // still culls what is emitted (frustum + Hi-Z) in the colour pass. Casters are
                // culled by their block as the snapped eye sees it (the block's cull reach already
                // includes the sun-following shadow reach), so the set of shadows only changes at
                // a snap step (or as the camera turns), a block at a time.
                if state == 1 {
                    let visible = if casts {
                        !self.reference || snapped_visible(&tile.blocks[usize::from(inst.block)])
                    } else {
                        frustum.intersects_sphere(
                            inst.centre,
                            inst.radius + local_shadow_reach(inst.radius, sun_elevation) + SHADOW_MARGIN_M,
                        )
                    };
                    if !visible {
                        continue;
                    }
                }
                if d >= class.fade_end {
                    continue;
                }
                // With impostors on the mesh keeps its size and dithers out across the fade
                // band against its impostor (`impostor_band`), which takes over at full size.
                let band = if impostors { impostor_band(class, d) } else { 0.0 };
                if band >= 1.0 {
                    continue;
                }
                let fade = if impostors { 1.0 } else { fade_at(class, d) }.min(detail_now);
                if fade < MIN_FADE {
                    continue;
                }
                let mut model = inst.model;
                if fade < 1.0 {
                    stats.fading += 1;
                    for column in model.iter_mut().take(3) {
                        for v in column.iter_mut().take(3) {
                            *v *= fade;
                        }
                    }
                }
                // Seat on the drawn surface: move by (drawn − true) height at the contact point,
                // in whole centimetres so a held camera keeps identical matrices.
                let mut lift = 0.0;
                if let (Some(q), Some(entries)) = (surface, seat.as_mut()) {
                    let dy = if self.reference {
                        seat_reference(q, inst.ground, &heights)
                    } else {
                        seat_dy(
                            q,
                            &mut entries[index],
                            inst.ground,
                            generation,
                            &heights,
                            &mut stats,
                        )
                    };
                    if dy != 0.0 {
                        model[3][1] += dy / 100.0;
                        lift = -dy / 100.0;
                        stats.seated += 1;
                        stats.seat_max_cm = stats.seat_max_cm.max(dy.abs() as u32);
                    }
                }
                let size = inst.radius * fade * inv_tan / d.max(1e-3) * class.lod_bias;
                // Speed-aware LOD (ADR-1503): chosen for where the instance will be.
                let size = self.motion.map_or(size, |m| {
                    let to = Vec3::from_array(inst.centre) - eye;
                    let closing = self.eye_step.map_or(0.0, |s| s.dot(to) / d.max(1e-3));
                    m.lod_size(size, d, inst.radius, closing)
                });
                let on_screen = frustum.intersects_sphere(inst.centre, inst.radius);
                let (lod_index, outgoing) = self.select_lod(proto, size, on_screen, &mut lod_states[index]);
                let variant = usize::from(inst.variant).min(proto.materials.len() - 1);
                let leafless = class
                    .season
                    .is_some_and(|season| !leaf_present(inst.object_key, season.density()));
                stats.leafless += u32::from(leafless);
                let record = Record {
                    proto: inst.proto,
                    lod: lod_index as u8,
                    variant: variant as u8,
                    still: inst.still,
                    object_key: inst.object_key,
                    model,
                    shadow: if self.full_shadows {
                        ShadowCast::All
                    } else if casts {
                        // A rock's shadow ends within `r / tan(elevation)` of it: the reach
                        // follows the sun (a low sun throws a much longer shadow), so a dusk
                        // shadow is not cut where the fixed `3 r + 1 m` reach (a ~20° sun)
                        // would have clipped it at a cascade boundary. The shadow is the rock at
                        // the snapped eye's fade, cast from the unseated height (the seat follows
                        // the terrain morph by centimetres every frame), and its cascades are
                        // chosen from the snapped eye's distance (the anchor sits where the
                        // snapped eye sees the rock from the real eye; the reach grows by the
                        // snap, which keeps the choice conservative).
                        ShadowCast::Local {
                            reach: local_shadow_reach(inst.radius * shadow_size, sun_elevation) + snap_err,
                            max_cascades: 2,
                            shadow_scale: shadow_size / fade,
                            shadow_lift: lift,
                            anchor: Some((Vec3::from_array(inst.centre) + (eye - shadow_eye)).to_array()),
                            // LOD switches keep the shadow pages: the strafe budget
                            // (`scatter_frame::rock_shadow_pages_stay_clean_while_rocks_fade`)
                            // does not allow a redraw per switch.
                            lod_switch_keeps_pages: true,
                        }
                    } else {
                        ShadowCast::Off
                    },
                    lod_dither: regia_render::LOD_DITHER_NONE,
                    part: 0,
                    shadow_only: false,
                    reduced: 0,
                };
                match outgoing {
                    // Impostor band: the mesh keeps the share `1 − band` of the pixels, the
                    // complement of what `impostor.wgsl` keeps; a LOD switch inside the band
                    // does not cross-fade (one mesh copy only).
                    _ if band > 0.0 => {
                        stats.caster_triangles += self.emit_parts(
                            Record {
                                lod_dither: -band.max(IMPOSTOR_MIN_BAND),
                                ..record
                            },
                            class,
                            leafless,
                            &mut emit,
                        );
                        stats.impostor_band += 1;
                    }
                    // Cross-fade: the incoming LOD keeps a growing share of the pixels and casts
                    // the shadow; the outgoing LOD keeps the rest and casts none.
                    Some((prev, coverage)) => {
                        stats.caster_triangles += self.emit_parts(
                            Record {
                                lod_dither: coverage,
                                ..record
                            },
                            class,
                            leafless,
                            &mut emit,
                        );
                        self.emit_parts(
                            Record {
                                lod: prev as u8,
                                object_key: record.object_key ^ LOD_FADE_OBJECT_TAG,
                                shadow: ShadowCast::Off,
                                lod_dither: -coverage,
                                ..record
                            },
                            class,
                            leafless,
                            &mut emit,
                        );
                        stats.lod_fading += 1;
                    }
                    None => stats.caster_triangles += self.emit_parts(record, class, leafless, &mut emit),
                }
                stats.emitted += 1;
                stats.per_lod[lod_index.min(MAX_SCATTER_LODS - 1)] += 1;
            }
        }
        stats
    }

    /// One mesh per prototype LOD × material variant at the origin: every draw variant this
    /// field can emit, for [`regia_render::FrameRenderer::prewarm`] at level load, so no
    /// rock that streams in later uploads geometry or builds a material on the frame path.
    #[must_use]
    pub fn prewarm_meshes(&self) -> Vec<FrameMesh<'_>> {
        let mut out = Vec::new();
        for proto in &self.protos {
            for part in 0..=proto.parts.len() {
                let lods = (0..proto.lods.len()).filter_map(|i| proto.part_lod(part, i));
                for lod in lods.filter(|lod| !lod.is_empty()) {
                    // Swaying prototypes also prewarm their still materials (painted at rest).
                    let sways = !proto.still.is_empty() || proto.parts.iter().any(|p| !p.still.is_empty());
                    let still = if sways { 2 } else { 1 };
                    for (variant, rest) in (0..proto.materials.len() * still)
                        .map(|v| (v % proto.materials.len(), v >= proto.materials.len()))
                    {
                        let (mesh, mesh_key) = lod.variant(variant);
                        let (material, material_key) = proto.part_material(part, variant, rest);
                        let tag = variant as u64 | u64::from(rest) << 8;
                        out.push(FrameMesh {
                            is_static: false,
                            mesh_key,
                            material_key,
                            object_key: mix_key(self.ctx.key_base, mesh_key, tag),
                            vertices: &mesh.vertices,
                            indices: &mesh.indices,
                            material,
                            model: Mat4::IDENTITY.to_cols_array_2d(),
                            joints: &[],
                            receiver_layer: 0,
                            shadow: ShadowCast::Off,
                            uv1: &[],
                            lod_dither: 0.0,
                        });
                    }
                    // Lower-detail shadow meshes, as their shadow-only draws key them.
                    let (material, material_key) = proto.part_material(part, 0, false);
                    for (_, mesh, key) in &lod.reduced {
                        let mesh_key = shadow_only_key(*key);
                        out.push(FrameMesh {
                            is_static: false,
                            mesh_key,
                            material_key,
                            object_key: mix_key(self.ctx.key_base, mesh_key, 0),
                            vertices: &mesh.vertices,
                            indices: &mesh.indices,
                            material,
                            model: Mat4::IDENTITY.to_cols_array_2d(),
                            joints: &[],
                            receiver_layer: 0,
                            shadow: ShadowCast::Off,
                            uv1: &[],
                            lod_dither: 0.0,
                        });
                    }
                }
            }
        }
        out
    }

    /// Turns the impostor far field on (`Some`) or off (`None`, the default).
    ///
    /// On, every class streams its tiles out to its impostor far end — the settings' far
    /// end, or nearer where the class's largest instance shrinks below
    /// [`ImpostorSettings::min_pixels`] at the reference resolution — and an instance past its
    /// class's `fade_start` hands over to its impostor: across the fade band the mesh keeps
    /// its size and is dithered out ([`FrameMesh::lod_dither`]) against the impostor's
    /// complementary share, and past `fade_end` only the impostor draws (the GPU impostor
    /// layer, [`crate::ImpostorLayer`]). Off, collect is exactly what it was without
    /// impostors (meshes shrink out over the fade band).
    pub fn set_impostors(&mut self, settings: Option<ImpostorSettings>) {
        let settings = settings.map(ImpostorSettings::sanitized);
        if self.impostors == settings {
            return;
        }
        self.impostors = settings;
        for class in &mut self.classes {
            let fade_end = class.fade_end;
            class.far_end = match settings {
                Some(s) => s
                    .far_end_m
                    .min(s.size_limit_distance(class.max_radius))
                    .max(fade_end),
                None => fade_end,
            };
            class.stream_end = if settings.is_some() {
                class.far_end
            } else {
                fade_end.min(class.detail_end)
            };
        }
        self.impostor_generation += 1;
    }

    /// Turns the detail limits on (`Some`) or off (`None`, the default: the frame is unchanged).
    ///
    /// On, each instance also shrinks out by its own projected size (pebbles end at tens of
    /// metres, boulders keep their class's range), a class streams only as far as its largest
    /// instance stays above the limit, and instances smaller than
    /// [`ScatterDetail::shadow_min_radius_m`] cast no shadow-map shadow. See
    /// [`super::detail`].
    pub fn set_detail(&mut self, detail: Option<ScatterDetail>) {
        let detail = detail.map(ScatterDetail::sanitized);
        if self.detail == detail {
            return;
        }
        self.detail = detail;
        for class in &mut self.classes {
            class.detail_end = detail.map_or(f32::INFINITY, |d| d.end_distance(class.max_radius));
            if self.impostors.is_none() {
                class.stream_end = class.fade_end.min(class.detail_end);
            }
        }
        // The emitted set and the contact footprints change.
        self.state_generation += 1;
        self.impostor_generation += 1;
    }

    /// Sets (`Some`) or clears (`None`, the default: the frame is unchanged) a field-wide shadow
    /// LOD hint, the scatter counterpart of the per-entity `ShadowLodHint` (`render.set_shadow_lod_hint`).
    ///
    /// Each instance casts from the asset's shadow LOD ([`ScatterMeshAsset::shadow_lod`], else its
    /// drawn LOD) clamped to the hint's `min_lod..=max_lod` and to the asset's chain. When that
    /// differs from the drawn LOD the shadow moves to shadow-only draws of it. A neutral hint
    /// is the same as `None`.
    pub fn set_shadow_lod_hint(&mut self, hint: Option<regia_render::ShadowLodHint>) {
        let hint = hint.filter(|h| !h.is_neutral());
        if self.shadow_lod_hint == hint {
            return;
        }
        self.shadow_lod_hint = hint;
        // The emitted set changes.
        self.state_generation += 1;
    }

    /// The field-wide shadow LOD hint, when set.
    #[must_use]
    pub fn shadow_lod_hint(&self) -> Option<regia_render::ShadowLodHint> {
        self.shadow_lod_hint
    }

    /// The detail limits, when on.
    #[must_use]
    pub fn detail(&self) -> Option<ScatterDetail> {
        self.detail
    }

    /// Turns speed-aware LOD selection on (`Some`) or off (`None`, the default: the frame is
    /// unchanged): LODs are chosen for where each approaching instance will be after the
    /// lookahead along the eye's last step, and instances within the full-detail radius draw
    /// LOD 0 ([`super::forest::MotionLod`], ADR-1503).
    pub fn set_motion_lod(&mut self, motion: Option<super::forest::MotionLod>) {
        self.motion = motion.map(super::forest::MotionLod::sanitized);
        if self.motion.is_none() {
            self.eye_step = None;
        }
    }

    /// The speed-aware LOD settings, when on.
    #[must_use]
    pub fn motion_lod(&self) -> Option<super::forest::MotionLod> {
        self.motion
    }

    /// The impostor settings, when the far field is on.
    #[must_use]
    pub fn impostors(&self) -> Option<ImpostorSettings> {
        self.impostors
    }

    /// Changes whenever [`Self::impostor_instances`] may return a different set (tiles
    /// streamed or evicted, a class or the field toggled, impostors switched).
    #[must_use]
    pub fn impostor_generation(&self) -> u64 {
        self.impostor_generation
    }

    /// Stable ids of the resident tiles whose instances [`Self::impostor_instances`] returns
    /// (enabled classes, active tiles), in emission order: the GPU layer uploads per tile, so
    /// a streamed tile costs one tile's upload. Empty while impostors or the field are off.
    pub fn impostor_tiles(&self, out: &mut Vec<u64>) {
        out.clear();
        if !self.enabled || self.impostors.is_none() {
            return;
        }
        out.extend(
            self.order
                .iter()
                .filter(|key| self.classes[usize::from(key.class)].enabled)
                .map(|key| tile_id(*key)),
        );
    }

    /// The impostor instances of tile `id` ([`Self::impostor_tiles`]) appended to `out`
    /// (instances over a hole are left out).
    pub fn impostor_tile_instances(&self, id: u64, out: &mut Vec<ImpostorInstance>) {
        let key = TileKey {
            class: (id >> 48) as u16,
            tx: ((id >> 24) & 0xff_ffff) as i32,
            tz: (id & 0xff_ffff) as i32,
        };
        let Some(tile) = self.tiles.get(&key) else { return };
        let holes = self.ctx.source.holes.as_ref().filter(|h| !h.is_empty());
        for inst in &tile.instances {
            if let Some(holes) = holes
                && over_hole(holes, inst.ground, inst.radius)
            {
                continue;
            }
            out.push(
                ImpostorInstance::from_model(&inst.model, inst.centre, inst.radius, inst.proto, inst.variant)
                    .with_leaf_rank(leaf_rank(inst.object_key)),
            );
        }
    }

    /// Every resident instance of an enabled class (not over a hole) as the GPU impostor
    /// layer draws it; the GPU picks those past their class's `fade_start` each frame. Empty
    /// while impostors or the field are off.
    pub fn impostor_instances(&self, out: &mut Vec<ImpostorInstance>) {
        out.clear();
        let mut ids = Vec::new();
        self.impostor_tiles(&mut ids);
        for id in ids {
            self.impostor_tile_instances(id, out);
        }
    }

    /// Bakes the impostor cards of every prototype (meshes shared by several classes are
    /// baked once) and the per-prototype draw table, for [`crate::ImpostorLayer::new`]. Uses
    /// the settings given to [`Self::set_impostors`], or `settings` when given. CPU only and
    /// deterministic; seconds for a large set, so call it at load (or on a worker).
    #[must_use]
    pub fn bake_impostors(&self, settings: Option<ImpostorSettings>) -> ImpostorAtlas {
        let settings = settings.or(self.impostors).unwrap_or_default().sanitized();
        // Class of each prototype (protos are built class by class, mesh by mesh).
        let mut class_of = vec![0usize; self.protos.len()];
        for (ci, row) in self.ctx.proto_of.iter().enumerate() {
            for &p in row {
                class_of[p as usize] = ci;
            }
        }
        // The bake reads the standard texture slots: a swaying material adopted from a plain one
        // (`FoliageSway::adopt`) bakes from its maps given back. Built materials: a card holds no season
        // tint or subsurface factors, those apply at draw time.
        let plain: Vec<Vec<Option<LabMaterial>>> = self
            .protos
            .iter()
            .map(|proto| {
                proto
                    .built
                    .iter()
                    .map(|(materials, _, _)| regia_render::FoliageSway::plain(&materials[0]))
                    .collect()
            })
            .collect();
        // Cards by geometry and textures: the same mesh in several classes bakes once.
        let mut card_of: HashMap<u64, u32> = HashMap::new();
        let mut sources = Vec::new();
        let proto_card: Vec<(u32, Option<u32>)> = self
            .protos
            .iter()
            .zip(&plain)
            .zip(&class_of)
            .map(|((proto, plain), &ci)| {
                let card = card_index(
                    card_parts(proto, plain, false),
                    proto.shape,
                    &mut card_of,
                    &mut sources,
                );
                // With a season set at bake time a leafless instance draws a card of its other parts.
                let bare_parts = card_parts(proto, plain, true);
                let wanted = self.classes[ci].season.is_some()
                    && proto.leaf_parts != u32::MAX
                    && !bare_parts.is_empty();
                let bare = wanted.then(|| card_index(bare_parts, proto.shape, &mut card_of, &mut sources));
                (card, bare)
            })
            .collect();
        let cards = super::impostor::bake_cards(&sources, &settings);
        let protos = self
            .protos
            .iter()
            .zip(&proto_card)
            .zip(&class_of)
            .map(|((proto, &(card, bare)), &ci)| {
                let class = &self.classes[ci];
                let material = &proto.materials[0];
                let far_end = match self.impostors {
                    Some(_) => class.far_end,
                    None => settings
                        .far_end_m
                        .min(settings.size_limit_distance(class.max_radius))
                        .max(class.fade_end),
                };
                ImpostorProto {
                    tint: [
                        proto.base_tint[0],
                        proto.base_tint[1],
                        proto.base_tint[2],
                        proto.tint_jitter,
                    ],
                    card: [
                        card as f32,
                        proto.shape.centre_y,
                        proto.shape.radius,
                        material.roughness,
                    ],
                    range: [class.fade_start, class.fade_end, far_end, material.metallic],
                    extra: [
                        proto.materials.len() as f32,
                        bare.map_or(-1.0, |card| card as f32),
                        0.0,
                        0.0,
                    ],
                    season: [1.0; 4],
                }
            })
            .collect();
        ImpostorAtlas {
            settings,
            cards,
            protos,
        }
    }

    /// World-space bounding sphere (centre, radius) of every resident instance of an enabled
    /// class, regardless of the view (tests project these to find rock pixels).
    #[must_use]
    pub fn instance_spheres(&self) -> Vec<([f32; 3], f32)> {
        let mut out = Vec::new();
        for (key, tile) in self.tiles.iter().filter(|(_, t)| t.active) {
            if self.classes[usize::from(key.class)].enabled {
                out.extend(tile.instances.iter().map(|inst| (inst.centre, inst.radius)));
            }
        }
        out
    }
}

/// Stable id of a tile for [`ScatterField::impostor_tiles`]. Indices keep their low 24 bits (two's
/// complement), so the id stays unique for the negative indices of a world anchor (Q-08) while
/// `|tx|, |tz| < 2^23`.
fn tile_id(key: TileKey) -> u64 {
    u64::from(key.class) << 48
        | u64::from(key.tx as u32 & 0xff_ffff) << 24
        | u64::from(key.tz as u32 & 0xff_ffff)
}

/// Identity of an impostor card: LOD 0 geometry and the baked texture slots (FNV-1a).
/// The parts of a prototype's LOD 0 that a card holds (with `bare`, those that stay without leaves),
/// each with its colour against part 0's: the draw table multiplies part 0's in.
fn card_parts<'a>(
    proto: &'a Prototype,
    plain: &'a [Option<LabMaterial>],
    bare: bool,
) -> Vec<ImpostorPart<'a>> {
    let base = proto.built[0].0[0].base_color;
    (0..=proto.parts.len())
        .filter(|&part| !(bare && proto.is_leaf(part)))
        .filter_map(|part| {
            let lod = proto.part_lod(part, 0).filter(|lod| !lod.is_empty())?;
            let built = &proto.built[part].0[0];
            let material = plain[part].as_ref().unwrap_or(built);
            Some(ImpostorPart {
                vertices: &lod.mesh.vertices,
                indices: &lod.mesh.indices,
                material,
                colour: [0, 1, 2].map(|k| {
                    if part == 0 {
                        1.0
                    } else {
                        built.base_color[k] / base[k].max(1e-3)
                    }
                }),
            })
        })
        .collect()
}

/// The atlas layer of the card holding `parts`: a new one unless an equal card exists.
fn card_index<'a>(
    parts: Vec<ImpostorPart<'a>>,
    shape: ProtoShape,
    card_of: &mut HashMap<u64, u32>,
    sources: &mut Vec<ImpostorSource<'a>>,
) -> u32 {
    let key = parts.iter().fold(0x1b87_3593_u64, |h, part| {
        let key = card_key(part.vertices, part.indices, part.material);
        let colour = part.colour.map(f32::to_bits);
        mix_key(
            h ^ key,
            u64::from(colour[0]) << 32 | u64::from(colour[1]),
            u64::from(colour[2]),
        )
    });
    let next = card_of.len() as u32;
    *card_of.entry(key).or_insert_with(|| {
        sources.push(ImpostorSource {
            parts,
            centre_y: shape.centre_y,
            radius: shape.radius,
        });
        next
    })
}

fn card_key(vertices: &[LabVertex], indices: &[u32], material: &LabMaterial) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    eat(bytemuck::cast_slice(indices));
    for v in vertices {
        eat(bytemuck::cast_slice(&v.position));
        eat(bytemuck::cast_slice(&v.normal));
        eat(bytemuck::cast_slice(&v.uv));
        eat(bytemuck::cast_slice(&v.tangent));
        eat(bytemuck::cast_slice(&v.color));
    }
    for tex in [&material.albedo, &material.normal_map] {
        match tex {
            Some(t) => {
                eat(&t.width.to_le_bytes());
                eat(&t.height.to_le_bytes());
                eat(&t.rgba);
            }
            None => eat(&[0xff]),
        }
    }
    eat(&material.normal_scale.to_le_bytes());
    h
}

/// Whether an instance standing at `ground` with bounding radius `radius` would hang over a
/// hole: its contact point or a point half its radius out (eight directions) is in one.
fn over_hole(holes: &crate::HolesMask, ground: [f32; 3], radius: f32) -> bool {
    let (x, z, r) = (ground[0], ground[2], radius * 0.5);
    holes.contains(x, z)
        || (0..8).any(|k| {
            let a = k as f32 * std::f32::consts::FRAC_PI_4;
            holes.contains(x + r * a.cos(), z + r * a.sin())
        })
}

/// The seat offset recomputed from scratch (the reference for [`seat_dy`]).
fn seat_reference(q: &crate::gpu::shared::SurfaceQuery<'_>, ground: [f32; 3], map: &TerrainMap) -> f32 {
    let (x, z) = (ground[0], ground[2]);
    let drawn = match q.node_at(x, z, None) {
        Some(draw) => {
            let (tri, ts) = q.triangle(&draw, x, z);
            q.height(&draw, &tri, ts, x, z)
        }
        None => map.sample_bilinear(x, z),
    };
    ((drawn - ground[1]) * 100.0).round().clamp(-500.0, 500.0)
}

/// The eye snapped to the shadow grid, and the farthest the snapped eye can be from the eye.
fn snapped_eye(eye: Vec3) -> (Vec3, f32) {
    (
        (eye / SHADOW_SNAP_M).round() * SHADOW_SNAP_M,
        SHADOW_SNAP_M * 0.5 * 3f32.sqrt(),
    )
}

/// Smallest nonzero impostor share: a dither code of exactly 0 would mean "drawn whole".
const IMPOSTOR_MIN_BAND: f32 = 1.0e-4;

/// The impostor's share of an instance's pixels at distance `d` (0 before the class's fade
/// band, 1 past it, linear across it). `impostor_cull.wgsl` / `impostor.wgsl` compute the same
/// value from the same centre and range, so mesh and impostor dither as complements.
fn impostor_band(class: &ClassRuntime, d: f32) -> f32 {
    ((d - class.fade_start) / (class.fade_end - class.fade_start)).clamp(0.0, 1.0)
}

/// The distance fade of `class` at distance `d` (1 = full size, 0 = gone).
fn fade_at(class: &ClassRuntime, d: f32) -> f32 {
    let t = ((class.fade_end - d) / (class.fade_end - class.fade_start)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl ScatterField {
    /// The LOD an instance draws at projected `size`, with hysteresis, and the outgoing LOD
    /// with the incoming LOD's pixel share while a switch cross-fades. Advances `state` once
    /// per field frame (a second collect in the same frame gives the same answer). Off
    /// screen (`on_screen` false: emitted for its shadow or by the conservative block cull) a
    /// switch is immediate: nobody sees it, and a fade begun there would play out as the rock
    /// enters the frame.
    fn select_lod(
        &self,
        proto: &Prototype,
        size: f32,
        on_screen: bool,
        state: &mut LodState,
    ) -> (usize, Option<(usize, f32)>) {
        // First sight, or back in view after frames culled: its last LOD is stale, and a
        // fade from it would stipple a rock entering the frame. It takes its LOD at once.
        let returning = state.seen + 1 < self.frame;
        if state.lod == LodState::UNSET || usize::from(state.lod) >= proto.lods.len() || returning {
            let lod = lod_at(proto, size);
            *state = LodState {
                lod: lod as u8,
                prev: lod as u8,
                start: self.frame,
                seen: self.frame,
            };
            return (lod, None);
        }
        state.seen = self.frame;
        let elapsed = self.frame.saturating_sub(state.start);
        if state.prev != state.lod && (elapsed >= LOD_FADE_FRAMES || !(self.lod_crossfade && on_screen)) {
            state.prev = state.lod;
        }
        if state.prev == state.lod {
            // Keep the LOD while the size stays inside the hysteresis band around its edges.
            let finest = lod_at(proto, size * (1.0 + LOD_HYSTERESIS));
            let coarsest = lod_at(proto, size * (1.0 - LOD_HYSTERESIS));
            let current = usize::from(state.lod);
            let target = current.clamp(finest, coarsest);
            if target != current {
                state.prev = if self.lod_crossfade && on_screen {
                    state.lod
                } else {
                    target as u8
                };
                state.lod = target as u8;
                state.start = self.frame;
            }
        }
        if state.prev == state.lod {
            return (usize::from(state.lod), None);
        }
        // Frame k of the fade (0-based) shows (k + 1) / (N + 1) of the incoming LOD.
        let k = self.frame.saturating_sub(state.start).min(LOD_FADE_FRAMES - 1);
        let coverage = (k + 1) as f32 / (LOD_FADE_FRAMES + 1) as f32;
        (usize::from(state.lod), Some((usize::from(state.prev), coverage)))
    }
}

/// The LOD a prototype draws at projected `size` (radius × fade over distance, scaled).
fn lod_at(proto: &Prototype, size: f32) -> usize {
    proto
        .lods
        .iter()
        .position(|l| size >= l.screen_size)
        .unwrap_or(proto.lods.len() - 1)
}

/// Adds one collect chunk's counters to the frame's.
fn merge_stats(stats: &mut ScatterFrameStats, chunk: &ScatterFrameStats) {
    stats.emitted += chunk.emitted;
    stats.fading += chunk.fading;
    stats.lod_fading += chunk.lod_fading;
    stats.seated += chunk.seated;
    stats.seat_max_cm = stats.seat_max_cm.max(chunk.seat_max_cm);
    stats.seat_fast += chunk.seat_fast;
    stats.seat_checked += chunk.seat_checked;
    stats.seat_computed += chunk.seat_computed;
    stats.considered += chunk.considered;
    stats.impostor_band += chunk.impostor_band;
    stats.caster_triangles += chunk.caster_triangles;
    stats.leafless += chunk.leafless;
    stats.portal_culled += chunk.portal_culled;
    for (a, b) in stats.per_lod.iter_mut().zip(chunk.per_lod) {
        *a += b;
    }
}

/// Seat offset (cm, rounded, clamped ±5 m) of an instance whose contact point is `ground`,
/// bit-identical to recomputing the drawn height every frame:
///
/// 1. same selection and generations, and the eye moved less than the entry's slack (no
///    vertex can have left its clamped morph state) → the cached value;
/// 2. same node, morph band, vertex morph factors and generations → the cached value;
/// 3. otherwise the drawn height is recomputed.
fn seat_dy(
    q: &crate::gpu::shared::SurfaceQuery<'_>,
    entry: &mut SeatEntry,
    ground: [f32; 3],
    generation: [u64; 2],
    map: &TerrainMap,
    stats: &mut ScatterFrameStats,
) -> f32 {
    let eye = q.eye();
    let selection = q.selection_key();
    if entry.valid && entry.generation == generation {
        // The entry's node is still drawn with the same morph band (a selected node that
        // contains the point is the one drawing it).
        let same_node = entry.selection == selection
            || q.node_draw(entry.node)
                .is_some_and(|d| [d.morph_start.to_bits(), d.morph_end.to_bits()] == entry.band);
        let (dx, dz) = (eye[0] - entry.eye[0], eye[1] - entry.eye[1]);
        // 1 mm margin over the f32 rounding of the vertex distances.
        if same_node && (dx * dx + dz * dz).sqrt() + 1e-3 < entry.slack {
            stats.seat_fast += 1;
            return entry.dy;
        }
    }
    let (x, z) = (ground[0], ground[2]);
    let hint = entry.valid.then_some(entry.node);
    let Some(draw) = q.node_at(x, z, hint) else {
        // Outside every selected node: the drawn height is the true one.
        entry.valid = false;
        stats.seat_computed += 1;
        return ((map.sample_bilinear(x, z) - ground[1]) * 100.0)
            .round()
            .clamp(-500.0, 500.0);
    };
    let (tri, ts, slack) = q.triangle_slack(&draw, x, z);
    let band = [draw.morph_start.to_bits(), draw.morph_end.to_bits()];
    let ts_bits = ts.map(f32::to_bits);
    let dy = if entry.valid
        && entry.node == draw.node
        && entry.band == band
        && entry.ts == ts_bits
        && entry.generation == generation
    {
        stats.seat_checked += 1;
        entry.dy
    } else {
        stats.seat_computed += 1;
        let drawn = q.height(&draw, &tri, ts, x, z);
        ((drawn - ground[1]) * 100.0).round().clamp(-500.0, 500.0)
    };
    *entry = SeatEntry {
        valid: true,
        node: draw.node,
        band,
        ts: ts_bits,
        generation,
        dy,
        selection,
        eye,
        slack,
    };
    dy
}

/// Places one tile (pure: the same key always gives the same tile, on any thread).
fn place_tile(ctx: &PlacerCtx, painted: &PaintedScatter, key: TileKey) -> Tile {
    let ci = usize::from(key.class);
    let class = &ctx.set.classes[ci];
    let density: Option<&DensityMap> = class
        .density_map
        .as_ref()
        .and_then(|p| ctx.source.density_maps.get(p));
    let n = ctx.tile_cells[ci];
    let side = n as f32 * class.cell_size;
    // The tile box follows the placement origin (Q-08; negative indices under a world anchor).
    let corner = ctx.grids[ci].corner;
    let origin = [corner[0] + key.tx as f32 * side, corner[1] + key.tz as f32 * side];
    let end = [origin[0] + side, origin[1] + side];
    // The tile's own window of the current heights (ADR-7638): the field's map is held only
    // while it is copied.
    let heights = ctx.window(origin, end);
    let ground = Ground::new(&heights, &ctx.origin);
    let inputs = ctx.source.inputs();
    let mut placed = Vec::new();
    let mut instances = Vec::new();
    for cz in key.tz * n..(key.tz + 1) * n {
        for cx in key.tx * n..(key.tx + 1) * n {
            let block = ((cz - key.tz * n) * BLOCKS / n * BLOCKS + (cx - key.tx * n) * BLOCKS / n) as u8;
            placed.clear();
            place_class_cell_on(
                &heights,
                ctx.set.seed,
                ci,
                class,
                density,
                inputs,
                &ctx.origin,
                cx,
                cz,
                &mut placed,
            );
            for (ordinal, p) in placed.iter().enumerate() {
                let Some(&proto_index) = ctx.proto_of[ci].get(usize::from(p.mesh)) else {
                    continue;
                };
                let shape = &ctx.shapes[proto_index as usize];
                let cell_id = (u64::from(cx as u32) << 32) | u64::from(cz as u32);
                let object_key = mix_key(ctx.key_base ^ (ci as u64) << 48, cell_id, ordinal as u64);
                let mut instance = build_instance(p, class, shape, proto_index, object_key);
                instance.block = block;
                instances.push(instance);
            }
        }
    }
    painted.visit_box(&class.name, origin, end, |path, paint| {
        let Some(mesh) = class.meshes.iter().position(|m| m.mesh == path) else {
            return;
        };
        let Some(&proto_index) = ctx.proto_of[ci].get(mesh) else {
            return;
        };
        let shape = &ctx.shapes[proto_index as usize];
        let object_key = mix_key(ctx.key_base ^ (ci as u64) << 48, paint.id, PAINTED_KEY_TAG);
        let mut placed = paint.placed(key.class, mesh as u16, class);
        // On ground a runtime edit wrote (a sample of its bilinear stencil is edited, which is
        // the box `ScatterField::heights_changed` re-sits), the instance stands on the edited
        // height, as that re-sit put it while resident; elsewhere it keeps its stored height (the
        // draw-time seat handles a surface that differs from the stored one). The field's own
        // record decides (ADR-7646), so an edit written into the source map itself counts too.
        let [x, _, z] = placed.position;
        if heights.edited_near(x, z) {
            placed.position[1] = ground.height(&heights, x, z);
        }
        let mut instance = build_instance(&placed, class, shape, proto_index, object_key);
        instance.still = paint.flags & super::painted::PAINTED_FLAG_STILL != 0;
        let block_of = |v: f32, o: f32| (((v - o) / side * BLOCKS as f32) as i32).clamp(0, BLOCKS - 1);
        let (col, row) = (
            block_of(paint.position[0], origin[0]),
            block_of(paint.position[2], origin[1]),
        );
        instance.block = (row * BLOCKS + col) as u8;
        instances.push(instance);
    });
    let seat = std::sync::Mutex::new(vec![SeatEntry::default(); instances.len()]);
    let lod = std::sync::Mutex::new(vec![LodState::NEW; instances.len()]);
    let (centre, radius, blocks) = tile_bounds(&instances);
    Tile {
        centre,
        radius,
        instances,
        blocks,
        active: false,
        seat,
        lod,
    }
}

/// A tile's bounding sphere and sub-block bounds from its instances (placement, and again after
/// [`ScatterField::heights_changed`] moved some of them).
fn tile_bounds(instances: &[Instance]) -> ([f32; 3], f32, [Block; (BLOCKS * BLOCKS) as usize]) {
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for inst in instances {
        for k in 0..3 {
            lo[k] = lo[k].min(inst.centre[k] - inst.radius);
            hi[k] = hi[k].max(inst.centre[k] + inst.radius);
        }
    }
    let (centre, radius) = if instances.is_empty() {
        ([0.0; 3], 0.0)
    } else {
        let c = [
            (lo[0] + hi[0]) * 0.5,
            (lo[1] + hi[1]) * 0.5,
            (lo[2] + hi[2]) * 0.5,
        ];
        let r = 0.5 * ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2) + (hi[2] - lo[2]).powi(2)).sqrt();
        (c, r)
    };
    let mut blocks = [Block::default(); (BLOCKS * BLOCKS) as usize];
    let mut sums = [[0.0f64; 3]; (BLOCKS * BLOCKS) as usize];
    for inst in instances {
        let b = usize::from(inst.block);
        blocks[b].count += 1;
        for (sum, v) in sums[b].iter_mut().zip(inst.centre) {
            *sum += f64::from(v);
        }
    }
    for (block, sum) in blocks.iter_mut().zip(&sums) {
        if block.count > 0 {
            block.centre = sum.map(|v| (v / f64::from(block.count)) as f32);
        }
    }
    for inst in instances {
        let block = &mut blocks[usize::from(inst.block)];
        let c = Vec3::from_array(inst.centre).distance(Vec3::from_array(block.centre));
        block.reach = block.reach.max(c + 0.01);
        block.max_radius = block.max_radius.max(inst.radius);
    }
    (centre, radius, blocks)
}

/// XZ distance from `eye` to the nearest point of tile (`tx`, `tz`) of a grid whose tile
/// `(0, 0)` starts at `corner`.
fn tile_distance(eye: [f32; 3], tx: i32, tz: i32, side: f32, corner: [f32; 2]) -> f32 {
    let x0 = corner[0] + tx as f32 * side;
    let z0 = corner[1] + tz as f32 * side;
    let dx = (x0 - eye[0]).max(eye[0] - (x0 + side)).max(0.0);
    let dz = (z0 - eye[2]).max(eye[2] - (z0 + side)).max(0.0);
    (dx * dx + dz * dz).sqrt()
}

/// `shared`: per material part (0 = the asset's own), the mesh's shared material and its key
/// ([`ScatterBuildOptions::shared_materials`]).
fn build_prototype(
    class: &ScatterClass,
    asset: &ScatterMeshAsset,
    key_base: u64,
    proto: u64,
    shared: Option<Vec<(Arc<LabMaterial>, u64)>>,
) -> Prototype {
    // Bounds of LOD 0 about the pivot; with more material parts, of every part's geometry (a
    // tree's crown is its twig cards, and its billboard spans the crown).
    let mut shape_points: Vec<[f32; 3]> = asset.lods[0].vertices.iter().map(|v| v.position).collect();
    for part in &asset.parts {
        shape_points.extend(
            part.lods
                .iter()
                .flat_map(|l| l.vertices.iter().map(|v| v.position)),
        );
    }
    let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
    for p in &shape_points {
        min_y = min_y.min(p[1]);
        max_y = max_y.max(p[1]);
    }
    let height = (max_y - min_y).max(1e-3);
    let centre_y = (min_y + max_y) * 0.5;
    let mut radius = 0.0f32;
    let mut footprint = 0.0f32;
    for p in &shape_points {
        radius = radius.max((p[0] * p[0] + (p[1] - centre_y).powi(2) + p[2] * p[2]).sqrt());
        if p[1] <= min_y + 0.2 * height {
            footprint = footprint.max((p[0] * p[0] + p[2] * p[2]).sqrt());
        }
    }
    // Every LOD carries two unreferenced vertices at the corners of the union box of all LODs
    // (of every part), so the renderer derives the same bounds for every LOD: a LOD switch leaves
    // the caster's shadow-page footprint unchanged and dirties no page (nothing is drawn
    // differently).
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    let every_lod = asset
        .lods
        .iter()
        .chain(asset.parts.iter().flat_map(|p| p.lods.iter()));
    for v in every_lod.flat_map(|l| l.vertices.iter()) {
        for k in 0..3 {
            lo[k] = lo[k].min(v.position[k]);
            hi[k] = hi[k].max(v.position[k]);
        }
    }
    let ground_line = min_y + class.sink * height;
    let blend_height = class.base_blend * height;
    let j = class.tint_jitter;
    let shades: Vec<f32> = (0..TINT_VARIANTS)
        .map(|v| 1.0 + j * (2.0 * v as f32 / (TINT_VARIANTS - 1) as f32 - 1.0))
        .collect();
    let corner_template = asset.lods[0].vertices[0];
    let build_part = |part: usize,
                      lods: &[ScatterMeshLod],
                      material: &LabMaterial,
                      shared: Option<&(Arc<LabMaterial>, u64)>|
     -> ProtoPart {
        // Part 0 keeps the keys a one-material mesh always had.
        let tag = (part as u64) << 40;
        // Base blend toward the class base colour, from the visible ground line up `base_blend`
        // of the height. Vertex colour multiplies the material colour, so the factor is the ratio.
        let base = material.base_color;
        let factor = [0, 1, 2].map(|k| {
            let albedo = (base[k] * class.tint[k]).max(1e-3);
            // Capped: a dark rock blended fully to a bright dust would print as dust.
            (class.base_tint[k] / albedo).clamp(0.0, 2.5)
        });
        let corners = [lo, hi].map(|p| {
            let mut v = corner_template;
            v.position = p;
            v
        });
        // With shared materials: the factor each variant's vertex colour carries, so that
        // material base × vertex colour = the default's (base × tint × shade, clamped) × blend.
        let variant_scale: Vec<[f32; 3]> = shades
            .iter()
            .map(|shade| {
                [0, 1, 2].map(|k| (base[k] * class.tint[k] * shade).clamp(0.0, 1.0) / base[k].max(1e-6))
            })
            .collect();
        let lods = lods
            .iter()
            .enumerate()
            .map(|(i, lod)| {
                let empty = lod_is_empty(lod);
                let mut vertices: Vec<LabVertex> = if empty {
                    Vec::new()
                } else {
                    lod.vertices
                        .iter()
                        .map(|v| {
                            let mut v = *v;
                            if blend_height > 0.0 {
                                let s = ((v.position[1] - ground_line) / blend_height).clamp(0.0, 1.0);
                                let w = 1.0 - s * s * (3.0 - 2.0 * s);
                                for (c, f) in v.color.iter_mut().zip(factor) {
                                    *c *= 1.0 + (f - 1.0) * w;
                                }
                            }
                            v
                        })
                        .collect()
                };
                if !empty {
                    vertices.extend(corners);
                }
                let indices = if empty { Vec::new() } else { lod.indices.clone() };
                let variants = if shared.is_some() && !empty {
                    variant_scale
                        .iter()
                        .enumerate()
                        .map(|(v, scale)| {
                            let tinted = vertices
                                .iter()
                                .map(|vertex| {
                                    let mut vertex = *vertex;
                                    for (c, s) in vertex.color.iter_mut().zip(scale) {
                                        *c *= s;
                                    }
                                    vertex
                                })
                                .collect();
                            (
                                Arc::new(CpuMesh {
                                    vertices: tinted,
                                    indices: indices.clone(),
                                }),
                                mix_key(key_base, proto << 8 | i as u64 | tag, 0x3e55 ^ (v as u64) << 20),
                            )
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                ProtoLod {
                    mesh: Arc::new(CpuMesh { vertices, indices }),
                    screen_size: asset.lods.get(i).map_or(0.0, |l| l.screen_size),
                    mesh_key: mix_key(key_base, proto << 8 | i as u64 | tag, 0x3e54),
                    variants,
                    reduced: Vec::new(),
                }
            })
            .collect();
        let (materials, material_keys): (Vec<Arc<LabMaterial>>, Vec<u64>) = match shared {
            Some((material, key)) => (0..TINT_VARIANTS).map(|_| (Arc::clone(material), *key)).unzip(),
            None => shades
                .iter()
                .enumerate()
                .map(|(v, shade)| {
                    let mut m = material.clone();
                    for ((out, b), t) in m.base_color.iter_mut().zip(base).zip(class.tint) {
                        *out = (b * t * shade).clamp(0.0, 1.0);
                    }
                    (
                        Arc::new(m),
                        mix_key(key_base, proto << 8 | v as u64 | tag, 0x0a7e),
                    )
                })
                .unzip(),
        };
        // A swaying material (`specs/88` § 2.5) also keeps a copy at rest per variant, for
        // painted instances marked still; nothing for a material that does not sway.
        // Shared materials are one `Arc` for every variant: one still copy for them all.
        let mut still: Vec<(Arc<LabMaterial>, u64)> = Vec::new();
        if regia_render::FoliageSway::of_material(material).is_some() {
            for (i, (m, key)) in materials.iter().zip(&material_keys).enumerate() {
                let same = i
                    .checked_sub(1)
                    .filter(|&prev| Arc::ptr_eq(&materials[prev], m))
                    .and_then(|prev| still.get(prev).cloned());
                still.push(same.unwrap_or_else(|| {
                    (
                        Arc::new(regia_render::FoliageSway::still(m)),
                        mix_key(*key, 0x5711, 0),
                    )
                }));
            }
        }
        ProtoPart {
            lods,
            materials,
            material_keys,
            still,
        }
    };
    let shared_of = |part: usize| shared.as_ref().and_then(|list| list.get(part));
    let main = build_part(0, &asset.lods, &asset.material, shared_of(0));
    let parts: Vec<ProtoPart> = asset
        .parts
        .iter()
        .enumerate()
        .map(|(p, part)| build_part(p + 1, &part.lods, &part.material, shared_of(p + 1)))
        .collect();
    let base = asset.material.base_color;
    let built = std::iter::once(&main)
        .chain(&parts)
        .map(|part: &ProtoPart| {
            (
                part.materials.clone(),
                part.material_keys.clone(),
                part.still.clone(),
            )
        })
        .collect();
    let masked = |material: &LabMaterial| material.alpha_mode == regia_render::ALPHA_MASK;
    let mask_bits = std::iter::once(&asset.material)
        .chain(asset.parts.iter().map(|part| &part.material))
        .enumerate()
        .filter(|(part, material)| *part < 32 && masked(material))
        .fold(0u32, |bits, (part, _)| bits | 1 << part);
    Prototype {
        built,
        leaf_parts: if mask_bits == 0 { u32::MAX } else { mask_bits },
        lods: main.lods,
        base_tint: [0, 1, 2].map(|k| base[k] * class.tint[k]),
        tint_jitter: j,
        materials: main.materials,
        material_keys: main.material_keys,
        still: main.still,
        parts,
        shadow_lod: usize::from(asset.shadow_lod).min(asset.lods.len() - 1),
        shape: ProtoShape {
            radius: radius.max(1e-3),
            centre_y,
            footprint: footprint.max(0.05 * radius),
            height,
        },
    }
}

fn build_instance(
    p: &PlacedInstance,
    class: &ScatterClass,
    proto: &ProtoShape,
    proto_index: u32,
    object_key: u64,
) -> Instance {
    let up = Vec3::from_array(p.up).normalize_or(Vec3::Y);
    let normal = Vec3::from_array(p.normal).normalize_or(Vec3::Y);
    let rotation = Quat::from_rotation_arc(Vec3::Y, up) * Quat::from_rotation_y(p.yaw);
    // Bury `sink` of the height, plus the gap a tilted-off-slope base leaves on its downhill side.
    let cos = up.dot(normal).clamp(0.2, 1.0);
    let residual_tan = (1.0 - cos * cos).sqrt() / cos;
    let height = proto.height * p.scale;
    let sink = (class.sink * height + proto.footprint * p.scale * residual_tan).min(0.6 * height);
    let pivot = Vec3::from_array(p.position) - up * sink;
    let model = Mat4::from_scale_rotation_translation(Vec3::splat(p.scale), rotation, pivot);
    let centre = model.transform_point3(Vec3::new(0.0, proto.centre_y, 0.0));
    let variant = (((p.shade - (1.0 - class.tint_jitter)) / (2.0 * class.tint_jitter).max(1e-6))
        * (TINT_VARIANTS - 1) as f32)
        .round()
        .clamp(0.0, (TINT_VARIANTS - 1) as f32) as u8;
    Instance {
        proto: proto_index,
        variant,
        still: false,
        object_key,
        model: model.to_cols_array_2d(),
        centre: centre.to_array(),
        radius: proto.radius * p.scale,
        ground: p.position,
        footprint: proto.footprint * p.scale,
        block: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scatter::placement::place_set;
    use crate::scatter::rock::{RockStyle, procedural_rock};
    use crate::scatter::set::ScatterMeshRef;

    fn flat() -> Arc<TerrainMap> {
        Arc::new(TerrainMap::from_heights(65, 65, vec![10.0; 65 * 65], 1024.0, 1.0, 0.0).expect("map"))
    }

    fn set() -> ScatterSet {
        ScatterSet {
            classes: vec![ScatterClass {
                name: "boulders".into(),
                meshes: vec![ScatterMeshRef {
                    mesh: "rock_a".into(),
                    weight: 1.0,
                    min_scale: 0.5,
                    max_scale: 1.5,
                    scale_bias: 1.0,
                }],
                cell_size: 4.0,
                density: 1.0,
                fade_start: 150.0,
                fade_end: 200.0,
                base_blend: 0.3,
                base_tint: [0.6, 0.4, 0.3],
                ..ScatterClass::default()
            }],
            ..ScatterSet::default()
        }
    }

    fn library() -> ScatterLibrary {
        let mut library = ScatterLibrary::new();
        library.insert(
            "rock_a".into(),
            procedural_rock(3, RockStyle::angular([0.5, 0.4, 0.35])),
        );
        library
    }

    fn field() -> ScatterField {
        let mut f = ScatterField::new(&set(), ScatterSource::new(flat()), &library(), 1).expect("field");
        f.update([0.0, 12.0, 0.0]);
        f
    }

    fn view(eye: [f32; 3], target: [f32; 3]) -> ScatterView {
        ScatterView::look_at(eye, target, 1.0, 16.0 / 9.0, 0.1, 2000.0)
    }

    /// LOD of each emitted mesh by object key: `(object, lod, lod_dither, casts)`.
    fn lods_of(f: &ScatterField, meshes: &[FrameMesh<'_>]) -> Vec<(u64, usize, f32, bool)> {
        meshes
            .iter()
            .map(|m| {
                let lod = f
                    .protos
                    .iter()
                    .find_map(|p| p.lods.iter().position(|l| l.mesh_key == m.mesh_key))
                    .expect("scatter mesh");
                (m.object_key, lod, m.lod_dither, m.shadow != ShadowCast::Off)
            })
            .collect()
    }

    /// A crater band (ADR-7585) over a 65 × 65, 1024 m map at 10 m with a 64 m height range: a
    /// 6 m bowl, 7 × 7 samples (96 m across) at sample (`c0`, `r0`), quantised like a patch.
    fn crater_band(map: &TerrainMap, c0: u32, r0: u32) -> crate::runtime_patch::HeightBand {
        let queue = crate::TerrainHeightPatches::default();
        queue.set_target(crate::PatchTarget::of(map));
        let heights = (0..49)
            .map(|i| {
                let (dc, dr) = ((i % 7) as f32 - 3.0, (i / 7) as f32 - 3.0);
                let t = ((dc * dc + dr * dr).sqrt() / 3.5).min(1.0);
                10.0 - 6.0 * (1.0 - t * t)
            })
            .collect();
        queue
            .request(crate::HeightPatch {
                col: c0,
                row: r0,
                width: 7,
                height: 7,
                heights,
            })
            .expect("crater");
        let mut bands = queue.take_frame();
        assert_eq!(bands.len(), 1);
        bands.remove(0)
    }

    fn crater_field_map() -> Arc<TerrainMap> {
        Arc::new(TerrainMap::from_heights(65, 65, vec![10.0; 65 * 65], 1024.0, 64.0, 0.0).expect("map"))
    }

    /// A crater under the field re-sits every instance on the changed ground onto the new
    /// surface (within one 16-bit step), leaves every other instance's matrix bit for bit, and
    /// visits only the tiles under the box (ADR-7585).
    #[test]
    fn a_crater_resits_only_the_instances_on_the_changed_ground() {
        let map = crater_field_map();
        let mut f =
            ScatterField::new(&set(), ScatterSource::new(Arc::clone(&map)), &library(), 1).expect("field");
        f.update([0.0, 12.0, 0.0]);
        let before = f.instance_models();
        let grounds = f.instance_grounds();
        assert!(before.len() > 200, "{} instances", before.len());
        let mut edited = (*map).clone();
        let band = crater_band(&map, 29, 29);
        band.apply(&mut edited).expect("same grid");
        let stats = f.heights_changed(&edited, &band.rect);
        let after = f.instance_models();
        let new_grounds = f.instance_grounds();
        assert_eq!(after.len(), before.len(), "a re-sit adds or removes nothing");
        let step = map.height_scale() / 65535.0;
        let (lo, hi) = crate::PatchTarget::of(&map).footprint(&band.rect);
        let spacing = map.sample_spacing();
        let (mut moved, mut kept) = (0u32, 0u32);
        for (i, (old, new)) in before.iter().zip(&after).enumerate() {
            let g = new_grounds[i];
            let surface = edited.sample_bilinear(g[0], g[2]);
            let near = g[0] > lo[0] - spacing
                && g[0] < hi[0] + spacing
                && g[2] > lo[1] - spacing
                && g[2] < hi[1] + spacing;
            if near {
                assert!(
                    (g[1] - surface).abs() <= step,
                    "instance {i} at {g:?}: surface {surface}"
                );
                let dy = new[3][1] - old[3][1];
                assert!(
                    (dy - (g[1] - grounds[i][1])).abs() < 1e-4,
                    "instance {i} moved by its ground change"
                );
                assert_eq!(&new[..3], &old[..3], "orientation and scale stay");
                moved += u32::from(old != new);
            } else {
                assert_eq!(old, new, "instance {i} outside the box is bit-identical");
                assert_eq!(g, grounds[i]);
                kept += 1;
            }
        }
        assert!(moved > 10 && kept > 100, "moved {moved}, kept {kept}");
        assert_eq!(stats.instances_resat, moved);
        assert!(stats.tiles_rebuilt >= 1 && stats.tiles_rebuilt <= stats.tiles_touched);
        assert!(
            stats.tiles_touched < stats.tiles_resident,
            "only the tiles under the box are visited: {stats:?}"
        );
        // The same band again changes nothing.
        let again = f.heights_changed(&edited, &band.rect);
        assert_eq!(again.instances_resat, 0);
        assert_eq!(f.instance_models(), after);
    }

    /// A tile placed after an edit stands on the edited ground, and an edit far from every
    /// resident tile visits none.
    #[test]
    fn tiles_placed_after_an_edit_stand_on_the_new_ground() {
        let map = crater_field_map();
        let mut f =
            ScatterField::new(&set(), ScatterSource::new(Arc::clone(&map)), &library(), 1).expect("field");
        f.set_background_streaming(false);
        let mut edited = (*map).clone();
        let band = crater_band(&map, 29, 29);
        band.apply(&mut edited).expect("same grid");
        let stats = f.heights_changed(&edited, &band.rect);
        assert_eq!((stats.tiles_resident, stats.tiles_touched), (0, 0));
        f.update([0.0, 12.0, 0.0]);
        let grounds = f.instance_grounds();
        assert!(!grounds.is_empty());
        let mut in_crater = 0;
        for g in &grounds {
            assert_eq!(
                g[1].to_bits(),
                edited.sample_bilinear(g[0], g[2]).to_bits(),
                "{g:?}"
            );
            in_crater += usize::from(g[1] < 9.9);
        }
        assert!(in_crater > 0, "some instances stand in the crater");
        // An edit at the far corner, outside every resident tile (the field streams 200 m).
        let models = f.instance_models();
        let far = crater_band(&map, 1, 1);
        far.apply(&mut edited).expect("same grid");
        let stats = f.heights_changed(&edited, &far.rect);
        assert_eq!((stats.tiles_touched, stats.instances_resat), (0, 0), "{stats:?}");
        assert_eq!(f.instance_models(), models);
    }

    /// Painted boulders over the crater site (`crater_band(29, 29)` spans x, z in ±48 m).
    fn painted_over_the_crater(map: &TerrainMap) -> PaintedScatter {
        let mut store = PaintedScatter::new();
        let brush = crate::ScatterBrush {
            shape: crate::BrushShape::Circle { radius: 60.0 },
            hardness: 1.0,
            density: 0.05,
            ..crate::ScatterBrush::for_class(&set().classes[0])
        };
        let inputs = crate::ScatterPaintInputs {
            map,
            holes: None,
            layers: None,
            seed: 3,
        };
        crate::scatter_stroke(
            &mut store,
            &set(),
            inputs,
            &brush,
            &[regia_core::glam::Vec2::ZERO],
        )
        .expect("stroke");
        assert!(store.len() > 20, "{} painted", store.len());
        store
    }

    /// Ground contact points of the field's painted instances (matched by their XZ bits), in the
    /// store's order.
    fn painted_grounds(f: &ScatterField, store: &PaintedScatter) -> Vec<[f32; 3]> {
        let grounds = f.instance_grounds();
        store
            .instances("boulders")
            .map(|p| {
                let found: Vec<_> = grounds
                    .iter()
                    .filter(|g| {
                        g[0].to_bits() == p.position[0].to_bits() && g[2].to_bits() == p.position[2].to_bits()
                    })
                    .collect();
                assert_eq!(
                    found.len(),
                    1,
                    "painted instance at {:?} is drawn once",
                    p.position
                );
                *found[0]
            })
            .collect()
    }

    /// v0.5.1 follow-up (ADR-7638): a painted instance on edited ground stands on the edited
    /// height when its tile is placed again, as the resident re-sit put it there: placed after
    /// the edit, and evicted and placed again after a re-sit, it takes the new ground (not its
    /// stored height, which only the ±5 m draw-time seat hid). Painted instances off the edited
    /// ground keep their stored height bit for bit.
    #[test]
    fn painted_instances_placed_after_an_edit_stand_on_the_new_ground() {
        let map = crater_field_map();
        let store = painted_over_the_crater(&map);
        let mut edited = (*map).clone();
        let band = crater_band(&map, 29, 29);
        band.apply(&mut edited).expect("same grid");
        let check = |grounds: &[[f32; 3]], what: &str| {
            let mut in_crater = 0;
            for (g, p) in grounds.iter().zip(store.instances("boulders")) {
                let new = edited.sample_bilinear(g[0], g[2]);
                if new.to_bits() == map.sample_bilinear(g[0], g[2]).to_bits() {
                    assert_eq!(
                        g[1].to_bits(),
                        p.position[1].to_bits(),
                        "{what}: {g:?} off the edit keeps its height"
                    );
                } else {
                    assert_eq!(
                        g[1].to_bits(),
                        new.to_bits(),
                        "{what}: {g:?} stands on the edited ground"
                    );
                    in_crater += 1;
                }
            }
            assert!(
                in_crater > 3,
                "{what}: {in_crater} painted instances in the crater"
            );
        };
        // Placed after the edit.
        let mut f =
            ScatterField::new(&set(), ScatterSource::new(Arc::clone(&map)), &library(), 1).expect("field");
        f.set_background_streaming(false);
        f.set_painted(&store);
        f.heights_changed(&edited, &band.rect);
        f.update([0.0, 12.0, 0.0]);
        check(&painted_grounds(&f, &store), "placed after the edit");
        // Resident, re-sat, then evicted and placed again: the same ground as the re-sit gave.
        let mut f =
            ScatterField::new(&set(), ScatterSource::new(Arc::clone(&map)), &library(), 1).expect("field");
        f.set_background_streaming(false);
        f.set_painted(&store);
        f.update([0.0, 12.0, 0.0]);
        f.heights_changed(&edited, &band.rect);
        let resat = painted_grounds(&f, &store);
        check(&resat, "re-sat");
        f.update([0.0, 12.0, 480.0]);
        assert!(
            f.instance_grounds().iter().all(|g| g[2] > 100.0),
            "the crater tiles are evicted"
        );
        f.update([0.0, 12.0, 0.0]);
        assert_eq!(
            painted_grounds(&f, &store),
            resat,
            "placed again where the re-sit left them"
        );
    }

    /// ADR-7646: which ground a runtime edit changed is recorded by the field itself, so a painted
    /// instance placed (or placed again) on edited ground takes the edited height even when the
    /// edit was written into the very map the field was built from. The re-seat used to compare
    /// the field's heights with its source map, and both read the same edited samples then.
    #[test]
    fn painted_instances_on_ground_edited_in_the_source_map_itself_take_the_new_height() {
        let map = crater_field_map();
        let store = painted_over_the_crater(&map);
        let band = crater_band(&map, 29, 29);
        let mut edited = (*map).clone();
        band.apply(&mut edited).expect("same grid");
        let check = |grounds: &[[f32; 3]], what: &str| {
            let mut in_crater = 0;
            for (g, p) in grounds.iter().zip(store.instances("boulders")) {
                let new = edited.sample_bilinear(g[0], g[2]);
                if new.to_bits() == map.sample_bilinear(g[0], g[2]).to_bits() {
                    assert_eq!(
                        g[1].to_bits(),
                        p.position[1].to_bits(),
                        "{what}: {g:?} off the edit keeps its height"
                    );
                } else {
                    assert_eq!(
                        g[1].to_bits(),
                        new.to_bits(),
                        "{what}: {g:?} stands on the edited ground"
                    );
                    in_crater += 1;
                }
            }
            assert!(
                in_crater > 3,
                "{what}: {in_crater} painted instances in the crater"
            );
        };
        // The field is the only holder of its source map; `prepare_height_edits` gives it its own
        // heights at load, so the host can then edit the source map in place.
        let field = || {
            let source = ScatterSource::new(Arc::new((*map).clone()));
            let mut f = ScatterField::new(&set(), source, &library(), 1).expect("field");
            f.set_background_streaming(false);
            f.set_painted(&store);
            assert!(f.prepare_height_edits() > 0, "the field's own heights");
            f
        };
        let edit_in_place = |f: &mut ScatterField| {
            let ctx = Arc::get_mut(&mut f.ctx).expect("no placement in flight");
            let source = Arc::get_mut(&mut ctx.source.map).expect("the field's own source map");
            band.apply(source).expect("same grid");
            let same = Arc::clone(&f.ctx.source.map);
            f.heights_changed(&same, &band.rect)
        };
        // Placed after the edit.
        let mut f = field();
        edit_in_place(&mut f);
        f.update([0.0, 12.0, 0.0]);
        check(&painted_grounds(&f, &store), "placed after the edit");
        // Resident, re-sat, then evicted and placed again.
        let mut f = field();
        f.update([0.0, 12.0, 0.0]);
        let stats = edit_in_place(&mut f);
        assert!(stats.instances_resat > 0, "{stats:?}");
        let resat = painted_grounds(&f, &store);
        check(&resat, "re-sat");
        f.update([0.0, 12.0, 480.0]);
        assert!(
            f.instance_grounds().iter().all(|g| g[2] > 100.0),
            "the crater tiles are evicted"
        );
        f.update([0.0, 12.0, 0.0]);
        assert_eq!(
            painted_grounds(&f, &store),
            resat,
            "placed again where the re-sit left them"
        );
    }

    /// ADR-7638: background placements read a window of the heights copied for their tile, never
    /// the field's map itself, so an edit while placements are in flight writes its samples in
    /// place. Only the first edit copies the map (the field's private copy of a map the host
    /// still shares); after it no edit copies a byte of the map, however many placements run.
    /// ADR-7638: `prepare_height_edits` takes the field's own copy at load (once), so the first
    /// edit in play copies no height map either.
    #[test]
    fn a_prepared_field_copies_no_height_map_on_its_first_edit() {
        let map = crater_field_map();
        let mut f =
            ScatterField::new(&set(), ScatterSource::new(Arc::clone(&map)), &library(), 1).expect("field");
        let map_bytes = std::mem::size_of_val(map.heights()) as u64;
        assert_eq!(
            f.prepare_height_edits(),
            map_bytes,
            "the host shares the map: one copy at load"
        );
        assert_eq!(f.prepare_height_edits(), 0, "already owned");
        let mut edited = (*map).clone();
        let far = crater_band(&map, 1, 1);
        far.apply(&mut edited).expect("same grid");
        assert_eq!(f.heights_changed(&edited, &far.rect).heights_copied_bytes, 0);
    }

    /// SC1 (ADR-8496): a world-anchored field (Q-08) under a runtime edit. The anchor sits on a
    /// sample, so placement reads the lattice ground, and the crater lies under tiles with
    /// negative indices (the anchor is inside the map). An edit must (a) re-sit the resident
    /// instances of those tiles onto the ground placement reads under the anchor, bit for bit as
    /// a field placed on the edited map, and (b) drop their background placements in flight so
    /// they are placed again from the new heights. With the map-edge tile boxes (`-half + tx ·
    /// side`) the negative tiles are never visited: their rocks keep the old ground and their
    /// in-flight placements survive the edit.
    #[test]
    fn world_anchored_edit_resits_and_drops_tiles_at_negative_indices() {
        use crate::scatter::placement::WorldAnchor;
        let map = crater_field_map();
        let origin = PlacementOrigin::World(WorldAnchor::at([16.0, -32.0]));
        let band = crater_band(&map, 29, 29);
        let mut edited = (*map).clone();
        band.apply(&mut edited).expect("same grid");
        let ground = Ground::new(&edited, &origin);
        assert!(
            matches!(ground, Ground::Lattice { .. }),
            "the anchor sits on a sample"
        );
        let field = |map: Arc<TerrainMap>, background: bool| {
            let mut f = ScatterField::new(&set(), ScatterSource::new(map), &library(), 1)
                .expect("field")
                .with_placement_origin(origin);
            f.set_background_streaming(background);
            f
        };
        // The crater's tile at (-1, -1): x in [-48, 16), z in [-96, -32).
        let crater_tile = TileKey {
            class: 0,
            tx: -1,
            tz: -1,
        };
        let in_crater_tile = |g: &[f32; 3]| g[0] < 16.0 && g[2] < -32.0;
        let eye = [0.0, 12.0, 0.0];
        let mut fresh = field(Arc::new(edited.clone()), false);
        fresh.update(eye);
        let fresh_grounds: HashMap<[u32; 2], u32> = fresh
            .instance_grounds()
            .iter()
            .map(|g| ([g[0].to_bits(), g[2].to_bits()], g[1].to_bits()))
            .collect();

        // (a) Resident tiles are re-sat.
        let mut f = field(Arc::clone(&map), false);
        f.update(eye);
        assert!(f.tiles.contains_key(&crater_tile), "the crater tile is resident");
        let stats = f.heights_changed(&edited, &band.rect);
        assert!(stats.instances_resat > 0, "{stats:?}");
        let grounds = f.instance_grounds();
        let (mut moved, mut moved_in_crater_tile, mut matched) = (0, 0, 0);
        for g in &grounds {
            let want = ground.height(&edited, g[0], g[2]);
            assert_eq!(
                g[1].to_bits(),
                want.to_bits(),
                "{g:?} stands on the edited ground"
            );
            if g[1] != 10.0 {
                moved += 1;
                moved_in_crater_tile += usize::from(in_crater_tile(g));
            }
            if let Some(&y) = fresh_grounds.get(&[g[0].to_bits(), g[2].to_bits()]) {
                assert_eq!(g[1].to_bits(), y, "{g:?} as a field placed after the edit");
                matched += 1;
            }
        }
        assert!(
            moved_in_crater_tile > 3,
            "{moved_in_crater_tile} re-sat in the negative tile ({moved} in all)"
        );
        assert!(
            matched * 2 > grounds.len(),
            "{matched} of {} matched",
            grounds.len()
        );

        // (b) A placement in flight for the crater tile is dropped. The eye stands 216 m east of
        // the tile, so the tile is the nearest in the background prefetch ring (200..264 m).
        let mut f = field(Arc::clone(&map), true);
        f.update([232.0, 12.0, -64.0]);
        assert!(
            f.pending.contains_key(&crater_tile),
            "the crater tile is placed in the background"
        );
        let stats = f.heights_changed(&edited, &band.rect);
        assert!(
            !f.pending.contains_key(&crater_tile),
            "the crater tile's placement is dropped"
        );
        assert!(stats.pending_dropped >= 1, "{stats:?}");
        // Placed again (synchronously) from the edited heights: the fresh field's tile.
        f.set_background_streaming(false);
        f.update(eye);
        let tile_grounds = |f: &ScatterField| -> Vec<[u32; 3]> {
            f.tiles[&crater_tile]
                .instances
                .iter()
                .map(|i| i.ground.map(f32::to_bits))
                .collect()
        };
        let again = tile_grounds(&f);
        assert!(
            again.iter().any(|g| f32::from_bits(g[1]) != 10.0),
            "rocks in the crater"
        );
        assert_eq!(again, tile_grounds(&fresh));
    }

    /// SC1 (ADR-8496): a field that owns its edited heights keeps them across a change of
    /// placement origin (the context is rebuilt, not cloned): the tiles placed under the new
    /// origin stand on the edited ground, and the next edit copies no height map.
    #[test]
    fn changing_the_origin_keeps_the_edited_heights() {
        use crate::scatter::placement::WorldAnchor;
        let map = crater_field_map();
        let origin = PlacementOrigin::World(WorldAnchor::at([16.0, -32.0]));
        let band = crater_band(&map, 29, 29);
        let mut edited = (*map).clone();
        band.apply(&mut edited).expect("same grid");
        let mut f =
            ScatterField::new(&set(), ScatterSource::new(Arc::clone(&map)), &library(), 1).expect("field");
        f.set_background_streaming(false);
        f.update([0.0, 12.0, 0.0]);
        assert!(f.heights_changed(&edited, &band.rect).heights_copied_bytes > 0);
        f.set_placement_origin(origin);
        assert_eq!(f.placement_origin(), origin);
        f.update([0.0, 12.0, 0.0]);
        let ground = Ground::new(&edited, &origin);
        let grounds = f.instance_grounds();
        assert!(grounds.iter().any(|g| g[1] != 10.0), "rocks in the crater");
        for g in &grounds {
            assert_eq!(
                g[1].to_bits(),
                ground.height(&edited, g[0], g[2]).to_bits(),
                "{g:?}"
            );
        }
        let far = crater_band(&map, 1, 1);
        far.apply(&mut edited).expect("same grid");
        assert_eq!(f.heights_changed(&edited, &far.rect).heights_copied_bytes, 0);
    }

    #[test]
    fn edits_while_placements_are_in_flight_copy_no_height_map() {
        let map = crater_field_map();
        let mut f =
            ScatterField::new(&set(), ScatterSource::new(Arc::clone(&map)), &library(), 1).expect("field");
        f.set_background_streaming(true);
        f.update([-300.0, 12.0, 0.0]);
        // A corner edit, outside every tile the dolly below streams (the field streams 200 m).
        let mut edited = (*map).clone();
        let far = crater_band(&map, 1, 1);
        far.apply(&mut edited).expect("same grid");
        let map_bytes = std::mem::size_of_val(map.heights());
        let first = f.heights_changed(&edited, &far.rect);
        assert_eq!(
            first.heights_copied_bytes, map_bytes as u64,
            "the first edit takes the field's own copy"
        );
        let mut held = Arc::as_ptr(&f.ctx.heights());
        let (mut copied, mut edits_in_flight) = (0usize, 0u32);
        for step in 0..400 {
            // The eye dollies along x and back, so tiles keep streaming in the background.
            let x = -300.0 + 3.0 * (step % 200) as f32;
            f.update([x, 12.0, 0.0]);
            edits_in_flight += u32::from(!f.pending.is_empty());
            let stats = f.heights_changed(&edited, &far.rect);
            assert_eq!(stats.pending_dropped, 0, "the corner edit drops no placement");
            assert_eq!(stats.heights_copied_bytes, 0, "step {step}");
            let now = Arc::as_ptr(&f.ctx.heights());
            if now != held {
                copied += map_bytes;
                held = now;
            }
        }
        assert!(
            edits_in_flight > 100,
            "{edits_in_flight} edits with placements in flight"
        );
        assert_eq!(
            copied, 0,
            "{copied} bytes of height map copied over {edits_in_flight} edits with placements in flight"
        );
    }

    /// One frame of a dolly: update, then collect.
    fn frame_at(f: &mut ScatterField, z: f32) -> Vec<(u64, usize, f32, bool)> {
        let eye = [0.0, 12.0, z];
        f.update(eye);
        let mut out = Vec::new();
        f.collect(&view(eye, [0.0, 10.0, z - 50.0]), &mut out);
        lods_of(f, &out)
    }

    #[test]
    fn lod_switches_have_hysteresis() {
        // The eye jitters ±1 % of the distance to the rocks around a fixed point. Without
        // hysteresis every instance whose size sits on a switch flips LOD with it; with it,
        // nothing switches after the first frame.
        let mut f = field();
        f.set_lod_crossfade(false);
        let first = frame_at(&mut f, 0.0);
        let mut switches = 0;
        let mut last: HashMap<u64, usize> = first.iter().map(|r| (r.0, r.1)).collect();
        for i in 1..40 {
            let z = if i % 2 == 0 { 0.4 } else { -0.4 };
            for (object, lod, _, _) in frame_at(&mut f, z) {
                if last.insert(object, lod).is_some_and(|before| before != lod) {
                    switches += 1;
                }
            }
        }
        // Plain selection over the same two poses flips this many instances per step.
        let plain = |z: f32| -> HashMap<u64, usize> {
            let eye = Vec3::new(0.0, 12.0, z);
            let inv_tan = 1.0 / 0.5f32.tan();
            f.tiles
                .values()
                .filter(|t| t.active)
                .flat_map(|t| t.instances.iter())
                .map(|inst| {
                    let class = &f.classes[0];
                    let d = Vec3::from_array(inst.centre).distance(eye);
                    let size = inst.radius * fade_at(class, d) * inv_tan / d.max(1e-3) * class.lod_bias;
                    (inst.object_key, lod_at(&f.protos[inst.proto as usize], size))
                })
                .collect()
        };
        let (a, b) = (plain(0.4), plain(-0.4));
        let flapping = a
            .iter()
            .filter(|(k, lod)| b.get(*k).is_some_and(|other| other != *lod))
            .count();
        eprintln!(
            "jittering eye: {flapping} instances flip LOD per step without hysteresis, {switches} switches with it"
        );
        assert!(
            flapping > 20,
            "the jitter crosses LOD switch sizes ({flapping} flips)"
        );
        assert_eq!(
            switches, 0,
            "LODs switched {switches} times while the eye jittered by 1 %"
        );
    }

    /// A dolly leaves LODs behind inside the hysteresis band and fades under way, so the same dolly
    /// again draws other frames. After a restart it draws the first run's frames (ADR-2672).
    #[test]
    fn a_lod_restart_plays_the_frames_of_a_fresh_field_again() {
        let dolly =
            |f: &mut ScatterField| -> Vec<_> { (0..32).map(|i| frame_at(f, i as f32 * 2.5)).collect() };
        let mut f = field();
        let first = dolly(&mut f);
        let again = dolly(&mut f);
        assert_ne!(
            first, again,
            "the second dolly starts from the LODs of the first one's end"
        );
        f.restart_lod();
        let restarted = dolly(&mut f);
        for (i, (a, b)) in first.iter().zip(&restarted).enumerate() {
            assert_eq!(a, b, "frame {i} of the restarted dolly");
        }
    }

    #[test]
    fn lod_switches_cross_fade_over_eight_frames() {
        // A slow dolly (0.25 m per frame) back from the rocks: every switch shows both LODs for
        // LOD_FADE_FRAMES frames with complementary dither, the incoming share rising, the
        // outgoing copy casting no shadow; a second collect in a frame changes nothing.
        let mut f = field();
        let mut fades: HashMap<u64, Vec<(usize, usize, f32)>> = HashMap::new();
        let mut switched = 0;
        let mut last: HashMap<u64, usize> = HashMap::new();
        // Frame each object was last emitted: an instance that left the view mid-fade is not
        // judged on that fade.
        let mut seen: HashMap<u64, usize> = HashMap::new();
        // Off screen a switch is immediate (and cuts a running fade short).
        let mut offscreen_switches = 0;
        for i in 0..160 {
            let z = i as f32 * 0.25;
            let rows = frame_at(&mut f, z);
            let frustum = Frustum::from_view_proj(&Mat4::from_cols_array_2d(
                &view([0.0, 12.0, z], [0.0, 10.0, z - 50.0]).view_proj,
            ));
            let on_screen: HashMap<u64, bool> = f
                .tiles
                .values()
                .flat_map(|t| t.instances.iter())
                .map(|inst| {
                    (
                        inst.object_key,
                        frustum.intersects_sphere(inst.centre, inst.radius),
                    )
                })
                .collect();
            let mut again = Vec::new();
            f.collect(&view([0.0, 12.0, z], [0.0, 10.0, z - 50.0]), &mut again);
            assert_eq!(
                rows,
                lods_of(&f, &again),
                "collect is not idempotent within a frame"
            );
            let mut copies: HashMap<u64, Vec<(usize, f32, bool)>> = HashMap::new();
            for (object, lod, dither, casts) in rows {
                let base = if dither < 0.0 {
                    object ^ LOD_FADE_OBJECT_TAG
                } else {
                    object
                };
                copies.entry(base).or_default().push((lod, dither, casts));
            }
            for (object, list) in copies {
                if seen.insert(object, i).is_some_and(|prev| prev + 1 != i) {
                    fades.remove(&object);
                    last.remove(&object);
                }
                match list.as_slice() {
                    [(lod, dither, _)] => {
                        assert_eq!(*dither, 0.0, "a lone copy is dithered");
                        let run = fades.remove(&object);
                        if last.insert(object, *lod).is_some_and(|before| before != *lod) {
                            if !on_screen.get(&object).copied().unwrap_or(true) {
                                offscreen_switches += 1;
                                continue;
                            }
                            // The fade ran for this object just before.
                            let Some(run) = run else {
                                panic!("object {object:x} switched LOD without a cross-fade");
                            };
                            assert_eq!(
                                run.len() as u64,
                                LOD_FADE_FRAMES,
                                "object {object:x} switched LOD with a {}-frame fade",
                                run.len()
                            );
                            assert!(
                                run.windows(2).all(|w| w[1].2 > w[0].2),
                                "coverage does not rise: {run:?}"
                            );
                            assert!(run.iter().all(|r| r.1 == *lod), "the fade ends on another LOD");
                            switched += 1;
                        }
                    }
                    [a, b] => {
                        let (incoming, outgoing) = if a.1 > 0.0 { (a, b) } else { (b, a) };
                        assert!(
                            incoming.1 > 0.0 && incoming.1 < 1.0,
                            "incoming dither {}",
                            incoming.1
                        );
                        assert_eq!(outgoing.1, -incoming.1, "copies are not complementary");
                        assert!(!outgoing.2, "the outgoing copy casts a shadow");
                        assert_ne!(incoming.0, outgoing.0, "both copies draw one LOD");
                        fades
                            .entry(object)
                            .or_default()
                            .push((outgoing.0, incoming.0, incoming.1));
                    }
                    other => panic!("object {object:x} emitted {} copies", other.len()),
                }
            }
        }
        eprintln!(
            "slow dolly: {switched} on-screen LOD switches, each cross-faded over {LOD_FADE_FRAMES} frames; \
             {offscreen_switches} off screen, immediate"
        );
        assert!(switched > 20, "the dolly crossed {switched} LOD switches");
    }

    #[test]
    fn streams_only_the_fade_range_and_matches_whole_map_placement() {
        let mut f = field();
        assert!(f.len() > 5_000, "{}", f.len());
        // Every resident instance is a whole-map placement instance (streaming changes nothing).
        let whole = place_set(&set(), ScatterSource::new(flat()).inputs()).expect("place");
        assert!(f.len() < whole.len() / 2, "streams a part of the map");
        let mut resident: Vec<[u32; 3]> = f
            .tiles
            .values()
            .flat_map(|t| t.instances.iter().map(|i| i.ground.map(f32::to_bits)))
            .collect();
        let mut expected: Vec<[u32; 3]> = whole
            .iter()
            .filter(|p| {
                let key_x = ((p.position[0] + 512.0) / 64.0).floor() as i32;
                let key_z = ((p.position[2] + 512.0) / 64.0).floor() as i32;
                tile_distance([0.0, 12.0, 0.0], key_x, key_z, 64.0, [-512.0, -512.0]) <= 200.0
            })
            .map(|p| p.position.map(f32::to_bits))
            .collect();
        resident.sort_unstable();
        expected.sort_unstable();
        assert_eq!(resident, expected);
        // Moving away evicts the old tiles and places new ones; coming back restores every
        // original instance bit for bit (hysteresis may keep a few extra tiles of the path).
        let away = f.update([400.0, 12.0, 0.0]);
        assert!(away.placed_tiles > 0 && away.evicted_tiles > 0, "{away:?}");
        f.update([0.0, 12.0, 0.0]);
        let back: std::collections::HashSet<[u32; 3]> = f
            .tiles
            .values()
            .flat_map(|t| t.instances.iter().map(|i| i.ground.map(f32::to_bits)))
            .collect();
        assert!(resident.iter().all(|p| back.contains(p)));
    }

    /// Q-08: a world-anchored field streams exactly the world-anchored whole-map placement,
    /// including the tiles with negative indices at the −X/−Z edge, and switching back to the
    /// map edge restores the edge-anchored instances.
    #[test]
    fn world_anchored_field_matches_world_anchored_placement() {
        let anchor = super::super::placement::WorldAnchor::at([6.0, -10.0]);
        let origin = PlacementOrigin::World(anchor);
        let whole = super::super::placement::place_set_with_origin(
            &set(),
            ScatterSource::new(flat()).inputs(),
            &origin,
        )
        .expect("place");
        let anchored = || {
            let mut f = ScatterField::new(&set(), ScatterSource::new(flat()), &library(), 1)
                .expect("field")
                .with_placement_origin(origin);
            f.set_background_streaming(false);
            f
        };
        let f = anchored();
        assert_eq!(f.placement_origin(), origin);
        let grid = f.ctx.grids[0];
        assert_eq!(grid.corner, [6.0, -10.0]);
        assert!(grid.first[0] < 0 && grid.first[1] < 0, "{grid:?}");
        // A fresh field per eye: tile hysteresis keeps old tiles active after a move.
        for eye in [[0.0, 12.0, 0.0], [-500.0, 12.0, -500.0], [500.0, 12.0, 480.0]] {
            let mut f = anchored();
            f.update(eye);
            let mut resident: Vec<[u32; 3]> = f
                .tiles
                .values()
                .filter(|t| t.active)
                .flat_map(|t| t.instances.iter().map(|i| i.ground.map(f32::to_bits)))
                .collect();
            let mut expected: Vec<[u32; 3]> = whole
                .iter()
                .filter(|p| {
                    let key_x = ((p.position[0] - 6.0) / 64.0).floor() as i32;
                    let key_z = ((p.position[2] + 10.0) / 64.0).floor() as i32;
                    tile_distance(eye, key_x, key_z, 64.0, [6.0, -10.0]) <= 200.0
                })
                .map(|p| p.position.map(f32::to_bits))
                .collect();
            resident.sort_unstable();
            expected.sort_unstable();
            assert!(!expected.is_empty());
            assert_eq!(resident, expected, "eye {eye:?}");
        }
        // Back to the map edge: the default field's instances, bit for bit.
        let mut f = anchored();
        f.update([0.0, 12.0, 0.0]);
        assert!(!f.is_empty());
        f.set_placement_origin(PlacementOrigin::MapEdge);
        assert!(f.is_empty(), "changing the origin drops every tile");
        f.update([0.0, 12.0, 0.0]);
        let edge = field();
        let bits = |f: &ScatterField| {
            let mut v: Vec<[u32; 3]> = f
                .tiles
                .values()
                .filter(|t| t.active)
                .flat_map(|t| t.instances.iter().map(|i| i.ground.map(f32::to_bits)))
                .collect();
            v.sort_unstable();
            v
        };
        assert_eq!(bits(&f), bits(&edge));
    }

    #[test]
    fn impostors_extend_the_stream_and_hand_over_across_the_fade_band() {
        let mut f = field();
        let eye = [0.0, 12.0, 0.0];
        let v = view(eye, [0.0, 10.0, -50.0]);
        let mut plain = Vec::new();
        f.collect(&v, &mut plain);
        let plain: Vec<_> = plain
            .iter()
            .map(|m| (m.object_key, m.model, m.lod_dither))
            .collect();
        let near = f.len();
        let generation = f.impostor_generation();
        f.set_impostors(Some(ImpostorSettings::default()));
        assert_ne!(f.impostor_generation(), generation);
        // Impostor-only tiles stream in the background and are never awaited: the first
        // update after switching on places none of them; later updates take them in.
        let first = f.update(eye);
        assert_eq!(first.waited_tiles, 0, "{first:?}");
        assert!(first.queued_tiles > 0, "{first:?}");
        // Bounded by time, not by iterations: a fixed number of yields ran out on a loaded
        // machine before the worker had streamed every tile.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            f.update(eye);
            if f.pending.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "background streaming did not finish in 30 s: {} tiles still pending",
                f.pending.len()
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        f.update(eye);
        // The far field streams the whole 1 km map (its far end is 2 km).
        assert!(f.len() > near * 3, "{} vs {near}", f.len());
        let mut sync = field();
        sync.set_background_streaming(false);
        sync.set_impostors(Some(ImpostorSettings::default()));
        sync.update(eye);
        assert_eq!(
            sync.len(),
            f.len(),
            "background and blocking streaming place the same far field"
        );
        let mut imps = Vec::new();
        f.impostor_instances(&mut imps);
        assert_eq!(imps.len(), f.len());
        let mut out = Vec::new();
        let stats = f.collect(&v, &mut out);
        assert!(stats.impostor_band > 0, "{stats:?}");
        assert_eq!(stats.fading, 0, "meshes never shrink with impostors on");
        for m in &out {
            let d = Vec3::new(m.model[3][0], m.model[3][1], m.model[3][2]).distance(Vec3::from_array(eye));
            // Pivot vs centre: within a rock's size of the band edges.
            if d < 140.0 {
                assert_eq!(m.lod_dither, regia_render::LOD_DITHER_NONE);
            }
            if m.lod_dither < 0.0 {
                // The mesh keeps the complement of the impostor's share.
                assert!(d > 145.0 && d < 205.0, "band mesh at {d}");
            }
            assert!(d < 205.0, "a mesh past the fade end at {d}");
        }
        // Far instances are exported with their exact bounding sphere and pivot.
        let far = imps
            .iter()
            .filter(|i| Vec3::from_array(i.centre).distance(Vec3::from_array(eye)) > 400.0);
        assert!(far.count() > 1000);
        // Off again: meshes shrink out over the fade band as before (the LOD states carry
        // over, so the first frames may cross-fade LODs), and nothing is exported.
        f.set_impostors(None);
        f.update(eye);
        let mut again = Vec::new();
        let stats = f.collect(&v, &mut again);
        assert_eq!(stats.impostor_band, 0);
        assert!(stats.fading > 0, "{stats:?}");
        assert!(again.len() >= plain.len());
        f.impostor_instances(&mut imps);
        assert!(imps.is_empty());
    }

    #[test]
    fn impostor_bake_dedupes_cards_and_fills_the_table() {
        let mut two = set();
        let mut second = two.classes[0].clone();
        second.name = "pebbles".into();
        second.fade_start = 60.0;
        second.fade_end = 80.0;
        two.classes.push(second);
        let f = ScatterField::new(&two, ScatterSource::new(flat()), &library(), 1).expect("field");
        let settings = ImpostorSettings {
            grid: 2,
            cell: 8,
            ..ImpostorSettings::default()
        };
        let atlas = f.bake_impostors(Some(settings));
        assert_eq!(atlas.protos.len(), 2);
        // Both classes place the same mesh with the same base blend: one card.
        assert_eq!(atlas.cards.len(), 1);
        let (a, b) = (atlas.protos[0], atlas.protos[1]);
        assert_eq!(a.card[0], 0.0);
        assert_eq!(b.card[0], 0.0);
        assert_eq!(a.range[0], 150.0);
        assert_eq!(b.range[1], 80.0);
        assert!(a.range[2] >= 200.0 && a.range[2] <= 2000.0);
        assert_eq!(b.tint, a.tint);
        assert_eq!(a.extra[0], TINT_VARIANTS as f32);
    }

    #[test]
    fn tile_budget_defers_far_tiles() {
        let mut f = ScatterField::new(&set(), ScatterSource::new(flat()), &library(), 1).expect("field");
        f.set_tile_budget(3);
        let first = f.update([0.0, 12.0, 0.0]);
        assert_eq!(first.placed_tiles, 3);
        assert!(first.pending_tiles > 0);
        let mut rounds = 0;
        while f.update([0.0, 12.0, 0.0]).pending_tiles > 0 {
            rounds += 1;
            assert!(rounds < 100);
        }
        assert_eq!(f.len(), field().len());
    }

    #[test]
    fn emits_only_tiles_in_view_and_range() {
        let f = field();
        let mut out = Vec::new();
        let stats = f.collect(&view([0.0, 12.0, 0.0], [0.0, 10.0, -50.0]), &mut out);
        assert_eq!(stats.emitted as usize, out.len());
        assert!(stats.emitted > 500, "{stats:?}");
        assert!(stats.tiles_visible < stats.tiles, "{stats:?}");
        for mesh in &out {
            let p = mesh.model[3];
            assert!((p[0] * p[0] + p[2] * p[2]).sqrt() < 200.0 + 2.0);
            assert!(p[2] < TILE_SIZE_M * 1.5, "behind the camera: {p:?}");
        }
        assert!(stats.per_lod[0] > 0 && stats.per_lod[2] > 0, "{stats:?}");
        assert!(stats.fading > 0);
    }

    #[test]
    fn keys_are_stable_and_disjoint() {
        let mut f = field();
        let v = view([0.0, 12.0, 0.0], [0.0, 10.0, -50.0]);
        let keys = |f: &ScatterField| {
            let mut out = Vec::new();
            f.collect(&v, &mut out);
            out.iter()
                .map(|m| (m.mesh_key, m.material_key, m.object_key))
                .collect::<Vec<_>>()
        };
        let a = keys(&f);
        // Evict and re-place: same keys.
        f.update([900.0, 12.0, 900.0]);
        f.update([0.0, 12.0, 0.0]);
        let b = keys(&f);
        assert_eq!(a, b);
        let mut objects: Vec<u64> = a.iter().map(|k| k.2).collect();
        objects.sort_unstable();
        objects.dedup();
        assert_eq!(objects.len(), a.len());
        assert!(a.iter().all(|k| k.2 != 0 && k.0 != k.1));
    }

    #[test]
    fn toggles_remove_everything() {
        let mut f = field();
        let v = view([0.0, 12.0, 0.0], [0.0, 10.0, -50.0]);
        f.set_enabled(false);
        assert_eq!(f.collect(&v, &mut Vec::new()).emitted, 0);
        f.set_enabled(true);
        assert!(f.set_class_enabled("boulders", false));
        assert!(!f.set_class_enabled("nope", false));
        assert_eq!(f.collect(&v, &mut Vec::new()).emitted, 0);
        let mut n = 0;
        f.footprints_in([0.0, 0.0], 50.0, |_| n += 1);
        assert_eq!(n, 0);
    }

    /// ADR-3170: a class's subsurface factors reach every material it draws with under new keys,
    /// and off restores the built materials and keys exactly.
    #[test]
    fn class_subsurface_reaches_the_materials_and_off_restores_them() {
        use regia_render::{SubsurfaceFactors, SubsurfaceProfile};
        let mut f = field();
        let v = view([0.0, 12.0, 0.0], [0.0, 10.0, -50.0]);
        let draws = |f: &ScatterField| -> Vec<(u64, u64, SubsurfaceFactors, usize)> {
            let mut out = Vec::new();
            f.collect(&v, &mut out);
            out.iter()
                .map(|m| {
                    (
                        m.object_key,
                        m.material_key,
                        m.material.subsurface,
                        std::ptr::from_ref(m.material) as usize,
                    )
                })
                .collect()
        };
        let built = draws(&f);
        assert!(!built.is_empty());
        assert!(built.iter().all(|d| d.2 == SubsurfaceFactors::OFF));
        assert_eq!(f.class_subsurface("boulders"), Some(SubsurfaceFactors::OFF));
        let leaf = SubsurfaceFactors::new(SubsurfaceProfile::Foliage);
        assert!(!f.set_class_subsurface("nope", leaf));
        assert!(f.set_class_subsurface("boulders", leaf));
        assert_eq!(f.class_subsurface("boulders"), Some(leaf));
        let on = draws(&f);
        assert_eq!(on.len(), built.len());
        for (a, b) in built.iter().zip(&on) {
            assert_eq!(a.0, b.0, "the same instances");
            assert_ne!(a.1, b.1, "a new material key");
            assert_eq!(b.2, leaf);
        }
        // Setting it again gives the same keys (the renderer's cache keeps its uploads).
        assert!(f.set_class_subsurface("boulders", leaf));
        assert_eq!(
            draws(&f).iter().map(|d| d.1).collect::<Vec<_>>(),
            on.iter().map(|d| d.1).collect::<Vec<_>>()
        );
        assert!(f.set_class_subsurface("boulders", SubsurfaceFactors::OFF));
        assert_eq!(draws(&f), built, "off restores the built materials and keys");
        // The renderer keys a runtime material by its address: switching back gives the same
        // copies, and none has been freed for another class's copy to land on.
        assert!(f.set_class_subsurface("boulders", leaf));
        assert_eq!(draws(&f), on, "on again is the same copies at the same addresses");
        let copies = f.subsurface_copies.len();
        assert!(copies > 0);
        assert!(f.set_class_subsurface("boulders", SubsurfaceFactors::OFF));
        assert!(f.set_class_subsurface("boulders", leaf));
        assert_eq!(
            f.subsurface_copies.len(),
            copies,
            "switching back makes no new copy"
        );
        let wider = SubsurfaceFactors {
            radius_mm: 2.0,
            ..leaf
        };
        assert!(f.set_class_subsurface("boulders", wider));
        assert!(f.subsurface_copies.len() > copies, "other factors, other copies");
        assert!(f.set_class_subsurface("boulders", leaf));
        assert_eq!(draws(&f), on, "the first copies are still there");
    }

    /// ADR-3170: a class whose materials are graphs (every swaying class) says why it does not
    /// scatter, and only while it asks to.
    #[test]
    fn a_class_says_why_it_cannot_scatter() {
        use regia_render::{SubsurfaceFactors, SubsurfaceProfile, SubsurfaceUnsupported, SurfaceGraph};
        let leaf = SubsurfaceFactors::new(SubsurfaceProfile::Foliage);
        let mut f = field();
        assert_eq!(f.class_subsurface_unsupported("boulders"), None, "does not ask");
        assert!(f.set_class_subsurface("boulders", leaf));
        assert_eq!(
            f.class_subsurface_unsupported("boulders"),
            None,
            "plain materials scatter"
        );
        assert_eq!(f.class_subsurface_unsupported("nope"), None);

        let mut f = field();
        let index = f
            .classes
            .iter()
            .position(|c| c.name == "boulders")
            .expect("class");
        let protos = f.ctx.proto_of[index].clone();
        for &p in &protos {
            for (materials, _, _) in &mut f.protos[p as usize].built {
                for material in materials {
                    *material = Arc::new(LabMaterial {
                        surface_graph: Some(Arc::new(SurfaceGraph::new("fn surface() {}"))),
                        ..(**material).clone()
                    });
                }
            }
        }
        assert_eq!(f.class_subsurface_unsupported("boulders"), None, "does not ask");
        assert!(f.set_class_subsurface("boulders", leaf));
        assert_eq!(
            f.class_subsurface_unsupported("boulders"),
            Some(SubsurfaceUnsupported::MaterialGraph)
        );
        assert!(f.set_class_subsurface("boulders", SubsurfaceFactors::OFF));
        assert_eq!(f.class_subsurface_unsupported("boulders"), None);
    }

    #[test]
    fn rocks_are_sunk_and_base_blended() {
        let f = field();
        let mut out = Vec::new();
        f.collect(&view([0.0, 12.0, 0.0], [0.0, 10.0, -50.0]), &mut out);
        for mesh in out.iter().take(200) {
            assert!(mesh.model[3][1] < 10.0, "{:?}", mesh.model[3]);
        }
        let lod = &f.protos[0].lods[0].mesh;
        let low = lod
            .vertices
            .iter()
            .min_by(|a, b| a.position[1].total_cmp(&b.position[1]))
            .expect("v");
        let high = lod
            .vertices
            .iter()
            .max_by(|a, b| a.position[1].total_cmp(&b.position[1]))
            .expect("v");
        assert!(low.color[0] / low.color[2] > high.color[0] / high.color[2] * 1.2);
    }

    #[test]
    fn shadow_policy_follows_the_class() {
        let with_distance = |set: ScatterSet| {
            let mut f = ScatterField::new(&set, ScatterSource::new(flat()), &library(), 1).expect("field");
            f.update([0.0, 12.0, 0.0]);
            let mut out = Vec::new();
            f.collect(&view([0.0, 12.0, 0.0], [0.0, 10.0, -50.0]), &mut out);
            out.iter()
                .map(|m| {
                    let d = Vec3::new(m.model[3][0], m.model[3][1] - 12.0, m.model[3][2]).length();
                    (m.shadow, d)
                })
                .collect::<Vec<_>>()
        };
        let shadows = |set: ScatterSet| with_distance(set).into_iter().map(|(s, _)| s).collect::<Vec<_>>();
        // Every rock casts into two cascades, except in the last stretch of the fade (fade_end
        // 200 m), where the snapped eye could already see the rock gone.
        let default = with_distance(set());
        // (The cut sits one snap error before the end, measured from the snapped eye, which
        // is itself up to one snap error away.)
        let edge = 200.0 - (SHADOW_SNAP_M * 3f32.sqrt() + 5.0);
        assert!(default.iter().all(|(s, d)| match s {
            ShadowCast::Local { max_cascades, .. } => *max_cascades == 2,
            ShadowCast::Off => *d > edge,
            ShadowCast::All => false,
        }));
        assert!(default.iter().filter(|(s, _)| *s != ShadowCast::Off).count() * 10 > default.len() * 9);
        let mut off = set();
        off.classes[0].cast_shadows = false;
        assert!(shadows(off).iter().all(|s| *s == ShadowCast::Off));
        let mut near = set();
        near.classes[0].shadow_max_lod = 0;
        let near = shadows(near);
        assert!(near.contains(&ShadowCast::Off));
        assert!(near.iter().any(|s| matches!(s, ShadowCast::Local { .. })));
    }

    #[test]
    fn detail_limits_drop_only_small_far_rocks() {
        let eye = [0.0, 12.0, 0.0];
        let v = view(eye, [0.0, 10.0, -50.0]);
        let field_with = |detail: Option<ScatterDetail>, set_it: bool| {
            let mut f = ScatterField::new(&set(), ScatterSource::new(flat()), &library(), 1).expect("field");
            if set_it {
                f.set_detail(detail);
            }
            f.update(eye);
            f
        };
        let emit = |f: &ScatterField| {
            let mut out = Vec::new();
            f.collect(&v, &mut out);
            out.iter()
                .map(|m| (m.object_key, m.model, m.shadow))
                .collect::<Vec<_>>()
        };
        let base = emit(&field_with(None, false));
        // Off is the default frame, bit for bit.
        assert_eq!(emit(&field_with(None, true)), base);
        // A coarse limit for this small test field: rocks under 6 px radius (12 px where the
        // fade starts) shrink out; shadows untouched.
        let detail = ScatterDetail {
            min_radius_px: 6.0,
            fade_ratio: 2.0,
            shadow_min_radius_m: 0.0,
            reference_height_px: 1080.0,
            reference_fov_y: 1.0,
        };
        let f = field_with(Some(detail), true);
        let on = emit(&f);
        assert!(
            !on.is_empty() && on.len() * 10 < base.len() * 9,
            "{} of {}",
            on.len(),
            base.len()
        );
        // Bounding radius of the smallest and the largest rock the class places.
        let r = f.protos[0].shape.radius;
        let (smallest, largest) = (r * 0.5, r * 1.5);
        let dist = |model: &[[f32; 4]; 4]| {
            Vec3::new(model[3][0], model[3][1], model[3][2]).distance(Vec3::from_array(eye))
        };
        let frustum = Frustum::from_view_proj(&Mat4::from_cols_array_2d(&v.view_proj));
        let mut near = 0;
        for (key, model, shadow) in &base {
            let d = dist(model);
            match on.iter().find(|(k, _, _)| k == key) {
                // Nearer than where even the smallest rock starts to shrink: untouched.
                Some((_, m, s)) if d + largest < detail.end_distance(smallest) * 0.5 => {
                    assert_eq!((m, s), (model, shadow));
                    near += 1;
                }
                Some(_) => {}
                // An off-screen rock is emitted only for its shadow, which the detail limits
                // decide from the snapped eye; on screen, gone only where a rock of this
                // class can have shrunk out.
                None if !frustum.intersects_sphere([model[3][0], model[3][1], model[3][2]], largest) => {}
                None => assert!(
                    d + largest > detail.end_distance(smallest),
                    "a rock at {d} m is gone; the smallest ends at {} m",
                    detail.end_distance(smallest)
                ),
            }
        }
        assert!(near > 10, "{near} near rocks compared");
        assert!(
            on.iter()
                .all(|(_, m, _)| dist(m) < detail.end_distance(largest) + largest)
        );
        // Small casters: no rock under the radius casts.
        let casts = |min_r: f32| {
            let f = field_with(
                Some(ScatterDetail {
                    shadow_min_radius_m: min_r,
                    ..detail
                }),
                true,
            );
            emit(&f)
                .iter()
                .filter(|(_, _, s)| matches!(s, ShadowCast::Local { .. }))
                .count()
        };
        let all = casts(0.0);
        assert!(all > 0);
        assert_eq!(casts(largest * 1.01), 0);
        let some = casts(r);
        assert!(some > 0 && some < all, "{some} of {all}");
    }

    #[test]
    fn shared_materials_draw_the_same_rocks_with_one_material_per_mesh() {
        // Two classes placing the same mesh with different tints.
        let mut two = set();
        let mut second = two.classes[0].clone();
        second.name = "boulders_fill".into();
        second.tint = [0.7, 0.6, 0.5];
        second.tint_jitter = 0.3;
        two.classes.push(second);
        let eye = [0.0, 12.0, 0.0];
        let v = view(eye, [0.0, 10.0, -50.0]);
        let build = |shared: bool| {
            let mut f = ScatterField::new_with(
                &two,
                ScatterSource::new(flat()),
                &library(),
                1,
                ScatterBuildOptions {
                    shared_materials: shared,
                },
            )
            .expect("field");
            f.update(eye);
            f
        };
        let (plain, shared) = (build(false), build(true));
        let (mut a, mut b) = (Vec::new(), Vec::new());
        plain.collect(&v, &mut a);
        shared.collect(&v, &mut b);
        assert_eq!(a.len(), b.len());
        let materials: std::collections::HashSet<u64> = b.iter().map(|m| m.material_key).collect();
        assert_eq!(materials.len(), 1, "one mesh path, one material");
        assert!(
            a.iter()
                .map(|m| m.material_key)
                .collect::<std::collections::HashSet<_>>()
                .len()
                > 4
        );
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(
                (x.object_key, x.model, x.shadow),
                (y.object_key, y.model, y.shadow)
            );
            assert_eq!(x.indices, y.indices);
            // Base colour × vertex colour is the default's, vertex by vertex.
            for (p, q) in x.vertices.iter().zip(y.vertices) {
                assert_eq!(p.position, q.position);
                for k in 0..3 {
                    let want = x.material.base_color[k] * p.color[k];
                    let got = y.material.base_color[k] * q.color[k];
                    assert!(
                        (want - got).abs() <= 1e-5 * want.abs().max(1.0),
                        "{want} vs {got}"
                    );
                }
            }
        }
        // Every draw variant is in the prewarm list.
        let pre = shared.prewarm_meshes();
        assert!(b.iter().all(|m| {
            pre.iter()
                .any(|p| p.mesh_key == m.mesh_key && p.material_key == m.material_key)
        }));
    }

    #[test]
    fn missing_assets_are_errors() {
        assert!(ScatterField::new(&set(), ScatterSource::new(flat()), &ScatterLibrary::new(), 1).is_err());
        let mut masked = set();
        masked.classes[0].density_map = Some("nope.png".into());
        assert!(ScatterField::new(&masked, ScatterSource::new(flat()), &library(), 1).is_err());
    }

    #[test]
    fn impostors_without_a_layer_stay_off() {
        use crate::scatter::control::ScatterControl;
        let mut f = field();
        let control = ScatterControl {
            impostors: true,
            ..ScatterControl::default()
        };
        f.set_impostors(Some(ImpostorSettings::default()));
        control.apply_impostors(&mut f, None, ImpostorSettings::default());
        assert!(f.impostors().is_none(), "no layer: the far field stays off");
    }

    #[test]
    fn unknown_scatter_class_is_reported_and_ignored() {
        use crate::scatter::control::ScatterControl;
        let mut f = field();
        let mut control = ScatterControl::default();
        control.set_class("nope", false);
        control.apply(&mut f, None);
        assert_eq!(f.classes(), vec![("boulders".to_string(), true)]);
        assert_eq!(control.unknown_classes(&f), vec!["nope".to_string()]);
        control.set_class("boulders", false);
        control.apply(&mut f, None);
        assert_eq!(f.classes(), vec![("boulders".to_string(), false)]);
        assert_eq!(control.unknown_classes(&f), vec!["nope".to_string()]);
        // The subsurface overrides report the same way, each name once.
        control.set_class_foliage_subsurface("nope", true);
        control.set_class_foliage_subsurface("nor_this", true);
        assert_eq!(
            control.unknown_classes(&f),
            vec!["nope".to_string(), "nor_this".to_string()]
        );
    }
}
