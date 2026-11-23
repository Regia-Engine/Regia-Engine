// SPDX-License-Identifier: MPL-2.0
//! What `regia bake-gi <scene>` bakes: a built-in scene (`cornell`, `furnace`,
//! `rolling-hills`) or a [`GiBakeDescription`] (`.bake.ron`, format v3, `docs/formats/bake-ron.md`)
//! naming the terrain (a raw 16-bit heightfield or a terrain grid mesh), static meshes (inline, or
//! a DO27 `static_meshes.json`), interiors, the suns, the sky (uniform or an HDRI), sky bodies, area
//! emitters, the probe layout (terrain layer, explicit and automatic detail bricks) and the bake
//! settings.
//!
//! Versions: v0 is the provisional pre-v1 shape (no `format_version` field); every v0 file is a
//! valid v1 file without the new fields, so the v0 → v1 step only stamps the version
//! ([`migrate_bake_ron`], ADR-279). v2 adds `sky_hdri` and `emitters` (ADR-1095) and the bake
//! quality controls to `bake` (`rr_start`, `clamp_direct`, `clamp_indirect`, `sample_groups`,
//! `firefly_threshold`; ADR-1096), all optional with defaults that bake as v1 did: every v1 file
//! is a valid v2 file, so v1 → v2 only rewrites the version number. v3 (ADR-7818) replaces the one
//! `sun: Option<..>` with a list, `suns: [..]` (at most [`MAX_BAKE_SUNS`]): the first is the scene's
//! sun, the others its extra suns (`PtScene::extra_suns`), and every probe and lightmap bake
//! integrates them all. v2 → v3 moves `sun: Some(s)` to `suns: [s]` and `sun: None` to `suns: []`,
//! in memory ([`GiBakeDescription::parse_versioned`]) and on disk ([`migrate_bake_ron`]); a one-sun
//! file bakes the same bytes before and after.

use std::path::{Path, PathBuf};

use regia_asset::baked_gi::{
    BakeInfo, BakedGiVolume, BrickFlags, DetailBrick, PORTABLE_VOLUME_BYTES, VolumeLayout,
};
use regia_core::glam::{Mat4, Vec3};
use serde::{Deserialize, Serialize};

use crate::PtError;
use crate::auto_bricks::{BrickBox, auto_bricks};
use crate::bake_inputs::{
    EmitterDesc, LightInputs, SkyAtmosphereDesc, SkyHdriDesc, add_emitter, atmosphere_sky, load_sky_hdri,
    validate_emitter, validate_sky_atmosphere, validate_sky_hdri,
};
use crate::bake_meshes::{
    BakeMeshLoader, CookedMeshLoader, Manifest, MeshStats, PlacedMesh, add_meshes, heightfield_from_mesh,
    parse_interiors_json, parse_static_meshes_json,
};
use crate::probe::{
    GiBakeSettings, ProbeTraceSettings, bake_info, bake_lights, check_probe_budget, detail_brick,
    terrain_layer,
};
use crate::scene::{PtCamera, PtHeightfield, PtMaterial, PtScene, PtSky, PtSkyBody, PtSun};
use crate::scenes;

/// Names of the built-in scenes.
pub const BUILTIN_SCENES: [&str; 3] = ["cornell", "furnace", "rolling-hills"];

/// The `.bake.ron` format version this build writes and reads (older versions migrate on load).
pub const BAKE_DESC_FORMAT_VERSION: u32 = 3;

/// The format version that replaced `sun` with `suns` (ADR-7818).
const SUNS_VERSION: u32 = 3;

/// Most suns a description may list (`suns`, format v3): the scene's sun plus the extra directional
/// lights every bake path takes (`regia_render::MAX_EXTRA_DIRECTIONAL`; the GPU bakes' uniforms
/// hold that many), so a description bakes the same suns on the CPU and the GPU.
pub const MAX_BAKE_SUNS: usize = 1 + regia_render::MAX_EXTRA_DIRECTIONAL;

/// A bake description error: an agent error code (`bake_desc_parse`, `bake_desc_newer`,
/// `bake_desc_invalid`, `bake_source_missing`, `bake_mesh_load`, `bake_manifest_mismatch`,
/// `bake_manifest_missing_entry`, `bake_layout`, `bake_volume_too_large`; `regia bake-gi --check`
/// adds `bake_volume_stale` and `bake_volume_unreadable`), the key path it concerns when there is one
/// (`layout.auto_bricks.spacing`, `meshes[2].source`, `static_meshes.json: meshes[3].layer`) and
/// a message. Documented in `book/agents/baked-gi-bake.md`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BakeDescError {
    /// Agent error code (lowercase ascii, digits and `_`).
    pub code: &'static str,
    /// Key path in the description (or in the file it names), when the error has one.
    pub path: Option<String>,
    /// What is wrong.
    pub message: String,
}

impl BakeDescError {
    /// `bake_desc_invalid` at `path`.
    #[must_use]
    pub fn invalid(path: &str, message: String) -> Self {
        Self {
            code: "bake_desc_invalid",
            path: Some(path.to_string()),
            message,
        }
    }

    /// `bake_desc_parse` (a syntax or shape error) in `what`.
    #[must_use]
    pub fn parse(what: &str, message: String) -> Self {
        Self {
            code: "bake_desc_parse",
            path: Some(what.to_string()),
            message,
        }
    }

    /// `bake_source_missing` (a file that cannot be read) at `path`.
    #[must_use]
    pub fn missing(path: &str, message: String) -> Self {
        Self {
            code: "bake_source_missing",
            path: Some(path.to_string()),
            message,
        }
    }

    /// `bake_layout` (the layout is inconsistent or empty).
    #[must_use]
    pub fn layout(path: Option<&str>, message: String) -> Self {
        Self {
            code: "bake_layout",
            path: path.map(str::to_string),
            message,
        }
    }
}

impl std::fmt::Display for BakeDescError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.path {
            Some(p) => write!(f, "{p}: {} [{}]", self.message, self.code),
            None => write!(f, "{} [{}]", self.message, self.code),
        }
    }
}

impl std::error::Error for BakeDescError {}

impl From<BakeDescError> for PtError {
    fn from(e: BakeDescError) -> Self {
        PtError::Scene(format!("bake description: {e}"))
    }
}

/// A bake job read from RON (`regia bake-gi map.bake.ron`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GiBakeDescription {
    /// Format version ([`BAKE_DESC_FORMAT_VERSION`]); absent in v0 files (read as 0).
    #[serde(default)]
    pub format_version: u32,
    /// Directory mesh sources, `static_meshes`, `interiors_file`, `manifest` and `terrain_mesh`
    /// are relative to (a bundle root), itself relative to the description; default: the
    /// description's directory. `terrain.heights` and `terrain.holes` stay relative to the
    /// description.
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// The terrain heightfield.
    #[serde(default)]
    pub terrain: Option<TerrainDesc>,
    /// The terrain as a grid mesh, rasterized into the traced heightfield (instead of `terrain`).
    #[serde(default)]
    pub terrain_mesh: Option<TerrainMeshDesc>,
    /// The suns (format v3, ADR-7818), at most [`MAX_BAKE_SUNS`]: the first is the scene's sun
    /// (what `sky_atmosphere` follows and the volume records as its baked sun), the others light
    /// the bake the same way, in the sun component. Empty = no sun.
    #[serde(default)]
    pub suns: Vec<SunDesc>,
    /// Format v0-v2's one sun (`sun: Some(..)`): moved into `suns` on load, so it is always
    /// `None` after `parse_versioned`; a v3 file that sets it is refused.
    #[serde(default, rename = "sun")]
    legacy_sun: Option<SunDesc>,
    /// Uniform sky radiance (cd/m²); `None` = black (airless).
    #[serde(default)]
    pub sky: Option<[f32; 3]>,
    /// Bodies in the sky (planet-shine fill).
    #[serde(default)]
    pub sky_bodies: Vec<SkyBodyDesc>,
    /// An equirect HDRI sky instead of `sky` (format v2).
    #[serde(default)]
    pub sky_hdri: Option<SkyHdriDesc>,
    /// The runtime's atmosphere sky for the first sun's direction, instead of `sky` (format v2).
    #[serde(default)]
    pub sky_atmosphere: Option<SkyAtmosphereDesc>,
    /// Area emitters: emissive quads, discs and spheres (format v2).
    #[serde(default)]
    pub emitters: Vec<EmitterDesc>,
    /// Static meshes: occluders and bounce surfaces.
    #[serde(default)]
    pub meshes: Vec<MeshDesc>,
    /// A DO27 `static_meshes.json` (version 1) whose entries are added to `meshes`.
    #[serde(default)]
    pub static_meshes: Option<PathBuf>,
    /// Enclosed volumes: bricks inside one are tagged with it.
    #[serde(default)]
    pub interiors: Vec<InteriorDesc>,
    /// A DO27 `interiors.json` (`do27-bake-interiors/1`) whose entries are added to `interiors`.
    #[serde(default)]
    pub interiors_file: Option<PathBuf>,
    /// A `MANIFEST.sha256` under the root: every file the bake reads under the root must be
    /// listed with a matching hash.
    #[serde(default)]
    pub manifest: Option<PathBuf>,
    /// Linear albedo standing in for a base color texture (times the material's factor) on
    /// mesh parts without an `albedo` override: the bake samples no textures.
    #[serde(default = "default_mesh_albedo")]
    pub default_albedo: [f32; 3],
    /// Probe layout.
    pub layout: LayoutDesc,
    /// Bake settings.
    #[serde(default)]
    pub bake: BakeDesc,
}

fn default_mesh_albedo() -> [f32; 3] {
    [0.5; 3]
}

/// A raw heightfield: `width × depth` little-endian u16 samples, row 0 at −Z, mapped to
/// `sample / 65535 · height_scale + height_offset` metres over `world_width` metres of X,
/// centred on the origin (the runtime `TerrainMap` convention).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerrainDesc {
    /// The `.r16` file, relative to the description.
    pub heights: PathBuf,
    /// Samples along X.
    pub width: u32,
    /// Samples along Z.
    pub depth: u32,
    /// Extent along X, metres.
    pub world_width: f32,
    /// Metres per full u16 range.
    pub height_scale: f32,
    /// Height of sample 0, metres.
    #[serde(default)]
    pub height_offset: f32,
    /// Linear diffuse albedo.
    #[serde(default = "default_albedo")]
    pub albedo: [f32; 3],
    /// The map's `<map>.holes.ron` (tunnel mouths), relative to the description: the samples
    /// inside are cut ([`PtHeightfield::cut_holes`]). A missing file cuts nothing.
    #[serde(default)]
    pub holes: Option<PathBuf>,
}

fn default_albedo() -> [f32; 3] {
    [0.3; 3]
}

fn identity() -> [f32; 16] {
    Mat4::IDENTITY.to_cols_array()
}

fn yes() -> bool {
    true
}

/// The terrain as a grid mesh (`terrain/terrain.gltf`): rasterized top-down every `spacing`
/// metres into the traced heightfield (`bake_meshes::heightfield_from_mesh`; uncovered samples
/// are holes). At the mesh's own vertex spacing the heightfield is the mesh.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerrainMeshDesc {
    /// The mesh, relative to the root. A `static_meshes` entry with the same source is not
    /// placed again as a mesh.
    pub source: PathBuf,
    /// Sample spacing, metres.
    pub spacing: f32,
    /// Object → world, column-major (default identity).
    #[serde(default = "identity")]
    pub transform: [f32; 16],
    /// Linear diffuse albedo.
    #[serde(default = "default_albedo")]
    pub albedo: [f32; 3],
    /// A `<map>.holes.ron`, relative to the root, cut as for `terrain`.
    #[serde(default)]
    pub holes: Option<PathBuf>,
}

/// A static mesh entry (one `static_meshes.json` entry maps to one of these).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshDesc {
    /// Name in reports and errors (default `meshes[<index>]`).
    #[serde(default)]
    pub id: Option<String>,
    /// A cooked mesh (`.fckd`) or a glTF, relative to the root.
    pub source: PathBuf,
    /// Object → world, column-major 4 × 4 (default identity). A negative determinant mirrors:
    /// the winding is flipped so faces stay front-facing.
    #[serde(default = "identity")]
    pub transform: [f32; 16],
    /// Per-instance transforms (column-major), each placed as `transform × instance`; `None` =
    /// one placement at `transform`, `Some([])` = none.
    #[serde(default)]
    pub instances: Option<Vec<[f32; 16]>>,
    /// Layer tag (`terrain`, `setpiece`, `scatter`, ...); `structure` and `dynamic` are not baked.
    #[serde(default)]
    pub layer: Option<String>,
    /// `false` = not baked (moves at runtime).
    #[serde(rename = "static", default = "yes")]
    pub is_static: bool,
    /// Material overrides for every part of the mesh.
    #[serde(default)]
    pub material: Option<MaterialOverride>,
    /// The placement is a coarse stand-in (DO27's rock hints): reported, baked like any other.
    #[serde(default)]
    pub approximate: bool,
}

/// Material overrides of a mesh entry (`None` keeps the source material's value).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialOverride {
    /// Linear albedo.
    #[serde(default)]
    pub albedo: Option<[f32; 3]>,
    /// Emitted radiance, cd/m² (a non-zero value makes the parts lights).
    #[serde(default)]
    pub emissive: Option<[f32; 3]>,
    /// Perceptual roughness.
    #[serde(default)]
    pub roughness: Option<f32>,
    /// Metallic factor.
    #[serde(default)]
    pub metallic: Option<f32>,
}

/// An enclosed volume. A brick whose box lies inside it is tagged with it; unless `sky_open`, the
/// tag asks for the brick's INTERIOR flag (no sky light through the brick's walls: ADR-279).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteriorDesc {
    /// Name.
    pub id: String,
    /// Box minimum corner, world metres.
    pub min: [f32; 3],
    /// Box maximum corner.
    pub max: [f32; 3],
    /// Open to the sky (a shaft): not an INTERIOR; its bricks keep the mesh-aware leak control.
    #[serde(default)]
    pub sky_open: bool,
    /// Has openings (a vehicle bay). Still INTERIOR as a whole in v0.2.0: no light enters
    /// through the opening (documented limitation).
    #[serde(default)]
    pub openings: bool,
    /// Probe spacing of the brick `auto_bricks` places over it (default: the auto spacing).
    #[serde(default)]
    pub spacing: Option<f32>,
}

/// Probe layout.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutDesc {
    /// Terrain layer column spacing (X, Z), metres; needs a terrain.
    #[serde(default)]
    pub terrain_spacing: Option<[f32; 2]>,
    /// Terrain layer heights above the ground, metres.
    #[serde(default = "default_levels")]
    pub levels: Vec<f32>,
    /// Detail bricks placed by hand.
    #[serde(default)]
    pub bricks: Vec<BrickDesc>,
    /// Detail bricks placed over the interiors and the static mesh clusters.
    #[serde(default)]
    pub auto_bricks: Option<AutoBricksDesc>,
    /// Octahedral moment texels per edge.
    #[serde(default = "default_moment_res")]
    pub moment_resolution: u32,
    /// Distance clamp of the moments, metres (default: twice the largest terrain spacing, terrain level
    /// extent or brick spacing, and at least 1 m).
    #[serde(default)]
    pub max_distance: Option<f32>,
}

fn default_levels() -> Vec<f32> {
    vec![1.0, 2.0, 4.0, 8.0, 16.0]
}

fn default_moment_res() -> u32 {
    4
}

/// A detail brick over a box.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrickDesc {
    /// Box minimum corner.
    pub min: [f32; 3],
    /// Box maximum corner.
    pub max: [f32; 3],
    /// Probe spacing, metres.
    pub spacing: f32,
}

/// Automatic detail bricks (`crate::auto_bricks`): one over each interior, then one per cluster
/// of static mesh placements (boxes grown by `margin`, overlapping ones merged).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoBricksDesc {
    /// Probe spacing, metres.
    pub spacing: f32,
    /// Margin around each mesh box, metres.
    #[serde(default = "default_margin")]
    pub margin: f32,
    /// Most probes in all the automatic bricks, interiors included. Over it the cluster spacing
    /// coarsens first; the interiors coarsen (with everything, by one common factor) only when
    /// they alone leave fewer than 8 probes per cluster (ADR-357).
    #[serde(default = "default_max_probes")]
    pub max_probes: u32,
    /// Layers whose meshes get bricks (empty = every layer but `terrain`).
    #[serde(default)]
    pub layers: Vec<String>,
    /// Also place a brick over each interior.
    #[serde(default = "yes")]
    pub interiors: bool,
}

fn default_margin() -> f32 {
    2.0
}

fn default_max_probes() -> u32 {
    1 << 18
}

/// Bake settings ([`GiBakeSettings`]).
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BakeDesc {
    /// Rays per probe per sample. Both tracers trace `clamp(n, 1, 4096)`. Validation accepts
    /// a larger value, and the bake summary prints the value stored here.
    pub rays_per_probe: u32,
    /// Samples per probe.
    pub samples: u32,
    /// Maximum path vertices (1..=64).
    pub max_bounces: u32,
    /// Seed.
    pub seed: u32,
    /// Probes per GPU batch.
    pub probes_per_batch: u32,
    /// Relocation rounds.
    pub relocation_iterations: u32,
    /// Run the 3×3×3 filter.
    pub filter: bool,
    /// Path vertex from which Russian roulette may end a path (v2; at least 1).
    pub rr_start: u32,
    /// Luminance cap of light seen without scattering, cd/m², 0 = off (v2).
    pub clamp_direct: f32,
    /// Luminance cap of light after a scattering event, cd/m², 0 = off (v2).
    pub clamp_indirect: f32,
    /// Sample groups: 2+ give the noise estimate, 3+ allow firefly suppression (v2).
    pub sample_groups: u32,
    /// Firefly suppression threshold over the median group, 0 = off, else above 1 (v2).
    pub firefly_threshold: f32,
}

impl Default for BakeDesc {
    fn default() -> Self {
        let t = ProbeTraceSettings::default();
        let g = GiBakeSettings::default();
        Self {
            rays_per_probe: t.rays_per_probe,
            samples: t.samples,
            max_bounces: t.max_bounces,
            seed: t.seed,
            probes_per_batch: t.probes_per_batch,
            relocation_iterations: g.relocation_iterations,
            filter: g.filter,
            rr_start: t.rr_start,
            clamp_direct: t.clamp_direct,
            clamp_indirect: t.clamp_indirect,
            sample_groups: t.sample_groups,
            firefly_threshold: t.firefly_threshold,
        }
    }
}

impl BakeDesc {
    /// The bake settings.
    #[must_use]
    pub fn settings(&self) -> GiBakeSettings {
        GiBakeSettings {
            trace: ProbeTraceSettings {
                rays_per_probe: self.rays_per_probe,
                samples: self.samples,
                max_bounces: self.max_bounces,
                seed: self.seed,
                probes_per_batch: self.probes_per_batch,
                rr_start: self.rr_start,
                clamp_direct: self.clamp_direct,
                clamp_indirect: self.clamp_indirect,
                sample_groups: self.sample_groups,
                firefly_threshold: self.firefly_threshold,
                ..ProbeTraceSettings::default()
            },
            relocation_iterations: self.relocation_iterations,
            filter: self.filter,
            ..GiBakeSettings::default()
        }
    }
}

/// Where a detail brick came from.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrickSource {
    /// A built-in scene's brick.
    Builtin,
    /// `layout.bricks[index]`.
    Explicit {
        /// Index in `layout.bricks`.
        index: usize,
    },
    /// The brick `auto_bricks` placed over an interior.
    Interior {
        /// The interior's id.
        id: String,
    },
    /// The brick `auto_bricks` placed over a cluster of mesh placements.
    Cluster {
        /// Ids of the meshes in the cluster (sorted, unique).
        meshes: Vec<String>,
        /// Placements in the cluster.
        placements: usize,
    },
}

/// What one detail brick is for (parallel to `VolumeLayout::bricks`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BrickPlan {
    /// Where it came from.
    pub source: BrickSource,
    /// The interior whose box holds the brick (first in description order).
    pub interior: Option<String>,
    /// The brick carries the INTERIOR flag: its whole box is inside an interior that is not
    /// `sky_open` (written into the brick's `flags`, so the `.fgiv` stores it; ADR-279, ADR-357).
    pub interior_flag: bool,
    /// The interior is open to the sky.
    pub sky_open: bool,
}

/// What `auto_bricks` did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AutoBrickReport {
    /// Requested spacing.
    pub requested_spacing: f32,
    /// Spacing used for the mesh-cluster bricks (coarser when `max_probes` capped them).
    pub spacing: f32,
    /// Spacing multiplier applied to the mesh-cluster bricks (1 = as requested).
    pub scale: f32,
    /// Spacing multiplier applied to the interior bricks: 1 unless the interiors alone left
    /// fewer than 8 probes per cluster under `max_probes` (then equal to `scale`).
    pub interior_scale: f32,
    /// Mesh placements considered.
    pub placements: usize,
    /// Mesh-cluster bricks.
    pub clusters: usize,
    /// Probes in all the automatic bricks (interiors and clusters).
    pub probes: usize,
    /// Interior bricks.
    pub interiors: usize,
}

/// The layout a description produced and how ([`GiBakeJob::plan`]): printed by
/// `regia bake-gi --dry-run` and with the bake summary.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BakePlan {
    /// Format version of the file as read (0 = a v0 file read through the migrator).
    pub format_version: u32,
    /// The traced heightfield was rasterized from `terrain_mesh`.
    pub terrain_from_mesh: bool,
    /// Static meshes.
    pub meshes: MeshStats,
    /// `static_meshes.json` entries skipped, `(id, reason)`.
    pub skipped: Vec<(String, String)>,
    /// Interiors known to the bake.
    pub interiors: usize,
    /// One per detail brick.
    pub bricks: Vec<BrickPlan>,
    /// What `auto_bricks` did.
    pub auto_bricks: Option<AutoBrickReport>,
    /// Files checked against the manifest.
    pub manifest_verified: usize,
    /// Bytes the volume takes in the renderer's one storage binding
    /// ([`VolumeLayout::runtime_buffer_bytes`]); above [`PORTABLE_VOLUME_BYTES`] the bake writes
    /// a split volume ([`Self::split_parts`]), and the build fails with `bake_volume_too_large`
    /// only when no split fits.
    pub volume_bytes: u64,
    /// What lights the bake: sun, sky (with its upward irradiance), emitters and their power.
    pub lights: LightInputs,
    /// Bytes of each part when the layout is above [`PORTABLE_VOLUME_BYTES`] and bakes as a split
    /// volume (ADR-2618, `regia_asset::baked_gi_split`; empty: one volume).
    pub split_parts: Vec<u64>,
    /// Per part of a split volume, the bricks it holds cropped at a cut (ADR-3237: a split that
    /// keeps every brick whole does not fit; empty for one volume or a whole-brick split).
    pub split_cropped_bricks: Vec<usize>,
}

/// The scene half of [`GiBakeDescription::build_with`]: everything but the probe layout.
struct BuiltScene {
    scene: PtScene,
    placed: Vec<PlacedMesh>,
    interiors: Vec<InteriorDesc>,
    plan: BakePlan,
}

/// A scene ready to bake.
#[derive(Clone, Debug)]
pub struct GiBakeJob {
    /// The traced scene.
    pub scene: PtScene,
    /// Probe layout.
    pub layout: VolumeLayout,
    /// Bake settings.
    pub settings: GiBakeSettings,
    /// How the layout was built.
    pub plan: BakePlan,
}

impl GiBakeJob {
    /// What differs between this job and `volume` (`regia bake-gi --check`): `scene` (the
    /// traced geometry, materials, sun or sky: the scene hash), `layout` (terrain layer, bricks
    /// and their flags, moment resolution, distance clamp), `lights` (the light levels the
    /// components were baked for), `settings` (seed, rays, samples, bounces, filter) and
    /// `emitters` (the scene has emitters but the volume does not name them: a format v1/v2
    /// bake, whose emitters the area lights count twice, ADR-1386), in that order. Empty when the
    /// volume is a bake of this job. Settings the volume does not record
    /// (relocation rounds, the back-face and front-face fractions) are not compared.
    ///
    /// One part of a split volume (ADR-2618) alone is a `layout` change: check the whole set
    /// with [`Self::stale_reasons_of_set`].
    #[must_use]
    pub fn stale_reasons(&self, volume: &BakedGiVolume) -> Vec<&'static str> {
        let mut out = Vec::new();
        let info = bake_info(&self.scene, &self.settings);
        if volume.info.scene_hash != info.scene_hash {
            out.push("scene");
        }
        let mut layout = self.layout.clone();
        layout.assign_brick_offsets();
        if volume.layout() != layout {
            out.push("layout");
        }
        if volume.lights != bake_lights(&self.scene) {
            out.push("lights");
        }
        let expected = BakeInfo {
            scene_hash: volume.info.scene_hash,
            ..info
        };
        if volume.info != expected {
            out.push("settings");
        }
        if volume.emitters.is_none() && !crate::probe::baked_emitters(&self.scene).is_empty() {
            out.push("emitters");
        }
        out
    }

    /// [`Self::stale_reasons`] of a volume as `BakedGiVolume::read_split_set` returns it (one
    /// entry for a whole volume, every part of a split one, ADR-2618): the parts are joined back
    /// into the volume the bake split (`BakedGiVolume::join_split_set`) and that is compared.
    /// Parts that do not join (a missing or foreign part) are a `layout` change.
    #[must_use]
    pub fn stale_reasons_of_set(&self, parts: &[BakedGiVolume]) -> Vec<&'static str> {
        match BakedGiVolume::join_split_set(parts) {
            Ok(whole) => self.stale_reasons(&whole),
            Err(_) => vec!["layout"],
        }
    }
}

fn camera() -> PtCamera {
    PtCamera::pinhole([0.0, 10.0, 10.0], [0.0; 3], 1.0, 4, 4)
}

/// Byte offset of the `(` that opens the root struct (skipping whitespace, comments, `ron`'s inner
/// attributes such as `#![enable(implicit_some)]` and a struct name), `None` when the root is not a
/// parenthesized struct.
fn root_open_paren(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => return Some(i),
            b'#' if bytes.get(i + 1) == Some(&b'!') && bytes.get(i + 2) == Some(&b'[') => {
                let mut depth = 0usize;
                i += 2;
                while i < bytes.len() {
                    match bytes[i] {
                        b'[' => depth += 1,
                        b']' => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b if b.is_ascii_whitespace() || b.is_ascii_alphanumeric() || b == b'_' => i += 1,
            _ => return None,
        }
    }
    None
}

/// The `format_version` of `.bake.ron` text without a full parse: 0 when absent (a v0 file, or
/// text that does not parse; the full parse then reports the error).
#[must_use]
pub fn bake_desc_version(text: &str) -> u32 {
    version_peek(text).0
}

/// [`bake_desc_version`] and how many bytes of `text` the peek read (ADR-3121). A lexical scan of
/// the root struct to its `format_version` field: a `ron::Value` parse of the whole text (what
/// `regia_core::version::peek_ron_version` does) costs (numbers × text length) in `ron` 0.12, whose
/// number check searches the rest of the input for `..` (see `regia_scene::text_split`), so a
/// level's exported description of thousands of placements took seconds to peek.
fn version_peek(text: &str) -> (u32, usize) {
    let Some(open) = root_open_paren(text) else {
        return (0, text.len());
    };
    match version_number_span(text, open) {
        Some((a, b)) => (text[a..b].parse().unwrap_or(0), b),
        None => (0, text.len()),
    }
}

/// Byte range of the number after the root struct's `format_version:` (comments and strings
/// skipped, nested values ignored), `None` when the root has no such field.
fn version_number_span(text: &str, open: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let key = b"format_version";
    let mut depth = 1usize;
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut nest = 1;
                i += 2;
                while i < bytes.len() && nest > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        nest += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        nest -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return None;
                }
                i += 1;
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if depth == 1 && bytes[start..i] == key[..] {
                    let mut j = i;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if bytes.get(j) != Some(&b':') {
                        continue;
                    }
                    j += 1;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    let digits = j;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    return (j > digits).then_some((digits, j));
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Skips whitespace and comments from `i`.
fn skip_trivia(bytes: &[u8], mut i: usize) -> usize {
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        match (bytes.get(i), bytes.get(i + 1)) {
            (Some(b'/'), Some(b'/')) => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            (Some(b'/'), Some(b'*')) => {
                let mut nest = 1;
                i += 2;
                while i < bytes.len() && nest > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        nest += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        nest -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            _ => return i,
        }
    }
}

/// The byte after the string literal whose opening quote is at `i`.
fn skip_string(bytes: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < bytes.len() && bytes[i] != b'"' {
        i += if bytes[i] == b'\\' { 2 } else { 1 };
    }
    (i + 1).min(bytes.len())
}

/// The end (exclusive) of the value starting at `start`: the end of its last token before the `,`
/// or closing bracket that ends it at its own depth (trailing comments and whitespace excluded).
/// `None` when the text ends first.
fn value_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = start;
    let mut end = start;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i = skip_string(bytes, i);
                end = i;
            }
            b'/' if matches!(bytes.get(i + 1), Some(b'/' | b'*')) => i = skip_trivia(bytes, i),
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
                end = i;
            }
            b')' | b']' | b'}' => {
                if depth == 0 {
                    return Some(end);
                }
                depth -= 1;
                i += 1;
                end = i;
            }
            b',' if depth == 0 => return Some(end),
            b if b.is_ascii_whitespace() => i += 1,
            _ => {
                i += 1;
                end = i;
            }
        }
    }
    None
}

/// A field of the root struct: the byte ranges of its key and of its value.
struct RootField {
    key: (usize, usize),
    value: (usize, usize),
}

/// The root struct's field `key` (its parenthesis at `open`; comments, strings and nested values
/// skipped), `None` when the root has no such field.
fn root_field(text: &str, open: usize, key: &[u8]) -> Option<RootField> {
    let bytes = text.as_bytes();
    let mut depth = 1usize;
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = skip_string(bytes, i),
            b'/' if matches!(bytes.get(i + 1), Some(b'/' | b'*')) => i = skip_trivia(bytes, i),
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return None;
                }
                i += 1;
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if depth == 1 && bytes[start..i] == key[..] {
                    let colon = skip_trivia(bytes, i);
                    if bytes.get(colon) != Some(&b':') {
                        continue;
                    }
                    let value = skip_trivia(bytes, colon + 1);
                    return value_end(bytes, value).map(|end| RootField {
                        key: (start, i),
                        value: (value, end),
                    });
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// The v2 → v3 text step (ADR-7818): the root's `sun: Some(s)` becomes `suns: [s]`, `sun: None`
/// becomes `suns: []` (and an implicit-some `sun: s` becomes `suns: [s]`); every other byte, the
/// sun's own text and comments included, stays. Text without a root `sun` is returned unchanged.
fn sun_to_suns(text: &str, open: usize) -> Result<String, BakeDescError> {
    let Some(field) = root_field(text, open, b"sun") else {
        return Ok(text.to_string());
    };
    let value = &text[field.value.0..field.value.1];
    let list = if value == "None" {
        "[]".to_string()
    } else if let Some(rest) = value.strip_prefix("Some") {
        let inner = skip_trivia(rest.as_bytes(), 0);
        if !(rest[inner..].starts_with('(') && rest.ends_with(')')) || rest.len() < inner + 2 {
            return Err(BakeDescError::parse(
                "sun",
                format!("cannot rewrite `sun: {value}` as a `suns` list"),
            ));
        }
        format!("[{}]", &rest[inner + 1..rest.len() - 1])
    } else {
        format!("[{value}]")
    };
    Ok(format!(
        "{}suns{}{list}{}",
        &text[..field.key.0],
        &text[field.key.1..field.value.0],
        &text[field.value.1..]
    ))
}

/// Migrates `.bake.ron` text to [`BAKE_DESC_FORMAT_VERSION`] on disk (`regia upgrade-project`),
/// keeping the rest of the file, comments included, byte for byte: v0 gets `format_version: N`
/// after the root's opening parenthesis, v1 and v2 have their version number rewritten (every v1
/// file is a valid v2 file), and a v0-v2 root `sun: Some(s)` / `sun: None` becomes `suns: [s]` /
/// `suns: []` (v3, ADR-7818). The result must read back as the description the in-memory
/// migration ([`GiBakeDescription::parse_versioned`]) loads from `text`. Current text is returned
/// unchanged.
///
/// # Errors
///
/// The text does not parse as a description, is newer than this build, has no root struct, or its
/// `sun` cannot be rewritten as a list that reads back the same (`bake_desc_parse`).
pub fn migrate_bake_ron(text: &str) -> Result<String, BakeDescError> {
    let (loaded, found) = GiBakeDescription::parse_versioned(text)?;
    if found >= BAKE_DESC_FORMAT_VERSION {
        return Ok(text.to_string());
    }
    let no_root = || BakeDescError::parse(".bake.ron", "no root `(`".to_string());
    let open = root_open_paren(text).ok_or_else(no_root)?;
    // v2 → v3 first: it only moves bytes after the root's `(`.
    let suns = if found < SUNS_VERSION {
        sun_to_suns(text, open)?
    } else {
        text.to_string()
    };
    let text = suns.as_str();
    let open = root_open_paren(text).ok_or_else(no_root)?;
    let version = BAKE_DESC_FORMAT_VERSION.to_string();
    let mut out = String::with_capacity(text.len() + 24);
    if found == 0 {
        out.push_str(&text[..=open]);
        out.push_str("\n    format_version: ");
        out.push_str(&version);
        out.push(',');
        out.push_str(&text[open + 1..]);
    } else {
        let (a, b) = version_number_span(text, open).ok_or_else(|| {
            BakeDescError::parse(".bake.ron", "no root `format_version` number".to_string())
        })?;
        out.push_str(&text[..a]);
        out.push_str(&version);
        out.push_str(&text[b..]);
    }
    let read_back = GiBakeDescription::parse_versioned(&out)
        .ok()
        .filter(|(again, v)| *v == BAKE_DESC_FORMAT_VERSION && format!("{again:?}") == format!("{loaded:?}"));
    if read_back.is_none() {
        return Err(BakeDescError::parse(
            ".bake.ron",
            format!(
                "the v{found} → v{BAKE_DESC_FORMAT_VERSION} rewrite does not read back as the loaded description; \
                 move `sun: Some(..)` into `suns: [..]` by hand"
            ),
        ));
    }
    Ok(out)
}

/// The serde field-list fingerprint of `.bake.ron` (hashed by `cargo xtask check-formats`): the
/// root struct and every nested struct.
#[must_use]
pub fn bake_desc_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<GiBakeDescription>(),
        serde_field_names::<TerrainDesc>(),
        serde_field_names::<TerrainMeshDesc>(),
        serde_field_names::<SunDesc>(),
        serde_field_names::<SkyBodyDesc>(),
        serde_field_names::<MeshDesc>(),
        serde_field_names::<MaterialOverride>(),
        serde_field_names::<InteriorDesc>(),
        serde_field_names::<LayoutDesc>(),
        serde_field_names::<BrickDesc>(),
        serde_field_names::<AutoBricksDesc>(),
        serde_field_names::<BakeDesc>(),
        serde_field_names::<SkyHdriDesc>(),
        serde_field_names::<SkyAtmosphereDesc>(),
        serde_variant_names::<crate::bake_inputs::PlanetPreset>(),
        serde_field_names::<EmitterDesc>(),
        serde_variant_names::<crate::bake_inputs::EmitterShape>(),
    ])
}

/// The versions a `.bake.ron` migration step leaves (`from` side): v0 → v1, v1 → v2, v2 → v3.
#[must_use]
pub fn bake_desc_migration_sources() -> Vec<u32> {
    vec![0, 1, 2]
}

/// Checks the bake quality settings (`bake.*` in a description; `regia bake-gi` checks its
/// command-line overrides with it too): `max_bounces` within 1..=64, `rr_start` at least 1, the
/// clamps zero or positive, `sample_groups` within 1..=`samples` (at most
/// [`crate::bake_quality::MAX_SAMPLE_GROUPS`]), `firefly_threshold` 0 or above 1 with three
/// sample groups or more.
///
/// # Errors
///
/// `bake_desc_invalid` naming the first bad key (`bake.max_bounces`, `bake.rr_start`,
/// `bake.clamp_direct`, `bake.clamp_indirect`, `bake.sample_groups`, `bake.firefly_threshold`).
pub fn validate_bake_settings(settings: &GiBakeSettings) -> Result<(), BakeDescError> {
    use crate::bake_quality::{MAX_SAMPLE_GROUPS, MIN_FIREFLY_GROUPS};
    let bad = |p: &str, m: String| Err(BakeDescError::invalid(p, m));
    let t = &settings.trace;
    if !(1..=64).contains(&t.max_bounces) {
        return bad(
            "bake.max_bounces",
            format!("{} must be within 1..=64", t.max_bounces),
        );
    }
    if t.rr_start == 0 {
        return bad("bake.rr_start", "must be at least 1".to_string());
    }
    for (key, v) in [
        ("bake.clamp_direct", t.clamp_direct),
        ("bake.clamp_indirect", t.clamp_indirect),
    ] {
        if !(v >= 0.0 && v.is_finite()) {
            return bad(key, format!("{v} must be 0 (off) or a positive luminance, cd/m²"));
        }
    }
    let most = t.samples.clamp(1, MAX_SAMPLE_GROUPS);
    if !(1..=most).contains(&t.sample_groups) {
        return bad(
            "bake.sample_groups",
            format!(
                "{} must be within 1..={most} (each group needs a sample; at most {MAX_SAMPLE_GROUPS})",
                t.sample_groups
            ),
        );
    }
    let k = t.firefly_threshold;
    if !(k == 0.0 || (k > 1.0 && k.is_finite())) {
        return bad(
            "bake.firefly_threshold",
            format!("{k} must be 0 (off) or above 1"),
        );
    }
    if k > 0.0 && (t.sample_groups as usize) < MIN_FIREFLY_GROUPS {
        return bad(
            "bake.sample_groups",
            format!(
                "firefly suppression needs at least {MIN_FIREFLY_GROUPS} sample groups, got {}",
                t.sample_groups
            ),
        );
    }
    Ok(())
}

fn positive(v: f32) -> bool {
    v > 0.0 && v.is_finite()
}

fn finite3(v: &[f32; 3]) -> bool {
    v.iter().all(|x| x.is_finite())
}

/// The bake-time leak control a brick's interior asks for, by the first interior that holds it.
fn classify(b: &DetailBrick, interiors: &[InteriorDesc]) -> (Option<String>, bool, bool) {
    let end = |k: usize| b.origin[k] + b.spacing[k] * (b.counts[k].saturating_sub(1)) as f32;
    for i in interiors {
        let tol = |k: usize| 1e-3 * (i.max[k] - i.min[k]).abs() + 1e-3;
        if (0..3).all(|k| b.origin[k] >= i.min[k] - tol(k) && end(k) <= i.max[k] + tol(k)) {
            return (Some(i.id.clone()), !i.sky_open, i.sky_open);
        }
    }
    (None, false, false)
}

impl GiBakeDescription {
    /// Parses RON (any supported version).
    ///
    /// # Errors
    ///
    /// The text is not a valid description.
    pub fn from_ron(text: &str) -> Result<Self, PtError> {
        Ok(Self::parse_versioned(text)?.0)
    }

    /// Parses RON and runs the in-memory migration chain; returns the description (at
    /// [`BAKE_DESC_FORMAT_VERSION`]) and the version the file had.
    ///
    /// # Errors
    ///
    /// `bake_desc_newer` for a version above this build's, `bake_desc_parse` for bad RON or an
    /// unknown field.
    pub fn parse_versioned(text: &str) -> Result<(Self, u32), BakeDescError> {
        let found = bake_desc_version(text);
        if found > BAKE_DESC_FORMAT_VERSION {
            return Err(BakeDescError {
                code: "bake_desc_newer",
                path: Some("format_version".into()),
                message: format!(
                    "version {found} is newer than this engine ({BAKE_DESC_FORMAT_VERSION}); upgrade the engine"
                ),
            });
        }
        let mut desc: Self =
            ron::from_str(text).map_err(|e| BakeDescError::parse(".bake.ron", e.to_string()))?;
        // v0 → v1 → v2: every newer field is optional, so the steps only move the version.
        // v2 → v3 (ADR-7818): the one `sun` becomes a list of at most one.
        if found < SUNS_VERSION {
            if !desc.suns.is_empty() {
                return Err(BakeDescError::invalid(
                    "suns",
                    format!(
                        "`suns` is a format v{SUNS_VERSION} field; this file is version {found} (use `sun`)"
                    ),
                ));
            }
            desc.suns = desc.legacy_sun.take().into_iter().collect();
        } else if desc.legacy_sun.is_some() {
            return Err(BakeDescError::invalid(
                "sun",
                format!("format v{SUNS_VERSION} lists its suns in `suns: [..]`; `sun` is the v0-v2 field"),
            ));
        }
        if desc.format_version < BAKE_DESC_FORMAT_VERSION {
            desc.format_version = BAKE_DESC_FORMAT_VERSION;
        }
        Ok((desc, found))
    }

    /// Checks the values serde cannot (positive spacings, boxes, exclusive fields, the bake
    /// settings: [`validate_bake_settings`]).
    ///
    /// # Errors
    ///
    /// `bake_desc_invalid` naming the first bad key.
    pub fn validate(&self) -> Result<(), BakeDescError> {
        let bad = |p: &str, m: &str| Err(BakeDescError::invalid(p, m.to_string()));
        if self.suns.len() > MAX_BAKE_SUNS {
            return Err(BakeDescError::invalid(
                "suns",
                format!(
                    "{} suns; a bake takes at most {MAX_BAKE_SUNS} (the sun and {} more)",
                    self.suns.len(),
                    MAX_BAKE_SUNS - 1
                ),
            ));
        }
        if self.terrain.is_some() && self.terrain_mesh.is_some() {
            return bad("terrain_mesh", "give either terrain or terrain_mesh, not both");
        }
        if let Some(t) = &self.terrain_mesh {
            if !positive(t.spacing) {
                return bad("terrain_mesh.spacing", "must be positive");
            }
            if t.transform.iter().any(|v| !v.is_finite()) {
                return bad("terrain_mesh.transform", "must be finite");
            }
        }
        if !self.default_albedo.iter().all(|a| (0.0..=1.0).contains(a)) {
            return bad("default_albedo", "must be within 0..=1");
        }
        if let Some(h) = &self.sky_hdri {
            if self.sky.is_some() {
                return bad("sky_hdri", "give either sky or sky_hdri, not both");
            }
            if h.extract_sun && !self.suns.is_empty() {
                return bad("sky_hdri.extract_sun", "the description has a sun already");
            }
            validate_sky_hdri(h)?;
        }
        if let Some(a) = &self.sky_atmosphere {
            if self.sky.is_some() || self.sky_hdri.is_some() {
                return bad("sky_atmosphere", "give one of sky, sky_hdri and sky_atmosphere");
            }
            if self.suns.is_empty() {
                return bad(
                    "sky_atmosphere",
                    "needs a sun (the table follows the first one's direction)",
                );
            }
            validate_sky_atmosphere(a)?;
        }
        for (i, e) in self.emitters.iter().enumerate() {
            validate_emitter(i, e)?;
        }
        for (i, m) in self.meshes.iter().enumerate() {
            if m.source.as_os_str().is_empty() {
                return bad(&format!("meshes[{i}].source"), "must name a mesh");
            }
            if let Some(a) = m.material.as_ref().and_then(|o| o.albedo)
                && !a.iter().all(|v| (0.0..=1.0).contains(v))
            {
                return bad(&format!("meshes[{i}].material.albedo"), "must be within 0..=1");
            }
        }
        for (i, r) in self.interiors.iter().enumerate() {
            if !(finite3(&r.min) && finite3(&r.max)) || (0..3).any(|k| r.min[k] >= r.max[k]) {
                return bad(&format!("interiors[{i}].max"), "must exceed min on every axis");
            }
            if r.spacing.is_some_and(|s| !positive(s)) {
                return bad(&format!("interiors[{i}].spacing"), "must be positive");
            }
        }
        let l = &self.layout;
        if let Some(s) = l.terrain_spacing {
            if !(positive(s[0]) && positive(s[1])) {
                return bad("layout.terrain_spacing", "must be positive");
            }
            if self.terrain.is_none() && self.terrain_mesh.is_none() {
                return bad("layout.terrain_spacing", "a terrain layer needs a terrain");
            }
        }
        for (i, b) in l.bricks.iter().enumerate() {
            if !positive(b.spacing) {
                return bad(&format!("layout.bricks[{i}].spacing"), "must be positive");
            }
            if !(finite3(&b.min) && finite3(&b.max)) || (0..3).any(|k| b.min[k] > b.max[k]) {
                return bad(&format!("layout.bricks[{i}].max"), "must not be below min");
            }
        }
        if let Some(a) = &l.auto_bricks {
            if !positive(a.spacing) {
                return bad("layout.auto_bricks.spacing", "must be positive");
            }
            if !(a.margin >= 0.0 && a.margin.is_finite()) {
                return bad("layout.auto_bricks.margin", "must be zero or positive");
            }
            if a.max_probes == 0 {
                return bad("layout.auto_bricks.max_probes", "must be positive");
            }
        }
        if l.moment_resolution == 0 || l.moment_resolution > 32 {
            return bad("layout.moment_resolution", "must be within 1..=32");
        }
        if l.max_distance.is_some_and(|d| !positive(d)) {
            return bad("layout.max_distance", "must be positive");
        }
        validate_bake_settings(&self.bake.settings())
    }

    /// Loads the heightfield (paths relative to `base`) and builds the job; mesh sources load
    /// through [`CookedMeshLoader`] (cooked meshes only; `regia bake-gi` passes a glTF loader to
    /// [`Self::build_with`]).
    ///
    /// # Errors
    ///
    /// A missing or short heightfield file, a bad terrain mapping, or an inconsistent layout.
    pub fn build(&self, base: &Path) -> Result<GiBakeJob, PtError> {
        Ok(self.build_with(base, &mut CookedMeshLoader)?)
    }

    /// [`Self::build`] with `loader` for the mesh sources: validates, loads the terrain, places the
    /// static meshes, reads the interiors, checks the manifest, and lays out the terrain layer,
    /// the explicit bricks and the automatic ones (interiors first, then mesh clusters).
    ///
    /// # Errors
    ///
    /// A [`BakeDescError`] with its code and key path.
    pub fn build_with(
        &self,
        base: &Path,
        loader: &mut dyn BakeMeshLoader,
    ) -> Result<GiBakeJob, BakeDescError> {
        let _span = tracing::info_span!("pt_bake_desc_build").entered();
        let BuiltScene {
            scene,
            placed,
            interiors,
            mut plan,
        } = self.build_scene(base, loader)?;
        let layout = self.layout(&scene, &placed, &interiors, &mut plan)?;
        Ok(GiBakeJob {
            scene,
            layout,
            settings: self.bake.settings(),
            plan,
        })
    }

    /// The traced scene and what placed it, without the probe layout: what a bake that has no
    /// probes needs (`regia bake-lightmaps --scene`, ADR-2024). A map too large for one probe
    /// volume still builds here.
    ///
    /// # Errors
    ///
    /// As [`Self::build_with`], less the layout's own errors.
    pub fn build_scene_with(
        &self,
        base: &Path,
        loader: &mut dyn BakeMeshLoader,
    ) -> Result<(PtScene, Vec<PlacedMesh>), BakeDescError> {
        let _span = tracing::info_span!("pt_bake_desc_build_scene").entered();
        let built = self.build_scene(base, loader)?;
        Ok((built.scene, built.placed))
    }

    fn build_scene(&self, base: &Path, loader: &mut dyn BakeMeshLoader) -> Result<BuiltScene, BakeDescError> {
        self.validate()?;
        let root = base.join(self.root.as_deref().unwrap_or(Path::new("")));
        let mut manifest = match &self.manifest {
            Some(m) => Some(Manifest::load(&root.join(m), &root, "manifest")?),
            None => None,
        };
        let mut plan = BakePlan {
            format_version: self.format_version,
            ..BakePlan::default()
        };
        let mut scene = PtScene::new(camera());
        if let Some(t) = &self.terrain {
            let path = base.join(&t.heights);
            if let Some(m) = manifest.as_mut() {
                m.verify(&path, "terrain.heights")?;
            }
            let bytes = std::fs::read(&path)
                .map_err(|e| BakeDescError::missing("terrain.heights", format!("{}: {e}", path.display())))?;
            let samples: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes(*b))
                .collect();
            let hf = regia_asset::Heightfield {
                width: t.width,
                height: t.depth,
                samples,
            };
            let map = regia_terrain::TerrainMap::new(&hf, t.world_width, t.height_scale, t.height_offset)
                .map_err(|e| BakeDescError::invalid("terrain", format!("{}: {e}", path.display())))?;
            let material = scene.add_material(PtMaterial::lambert(t.albedo));
            let mut hf = PtHeightfield::from_terrain_map(&map, material);
            if let Some(holes) = &t.holes {
                self.cut_holes(&mut hf, &base.join(holes), manifest.as_mut(), "terrain.holes")?;
            }
            scene.terrain = Some(hf);
        }
        if let Some(t) = &self.terrain_mesh {
            let path = root.join(&t.source);
            if let Some(m) = manifest.as_mut() {
                m.verify(&path, "terrain_mesh.source")?;
                for c in loader.companions(&path) {
                    m.verify(&c, "terrain_mesh.source")?;
                }
            }
            let source = loader.load(&path).map_err(|e| BakeDescError {
                code: "bake_mesh_load",
                path: Some("terrain_mesh.source".into()),
                message: e,
            })?;
            let material = scene.add_material(PtMaterial::lambert(t.albedo));
            let mut hf =
                heightfield_from_mesh(&source, &Mat4::from_cols_array(&t.transform), t.spacing, material)
                    .map_err(|e| BakeDescError::invalid("terrain_mesh", e))?;
            if let Some(holes) = &t.holes {
                self.cut_holes(
                    &mut hf,
                    &root.join(holes),
                    manifest.as_mut(),
                    "terrain_mesh.holes",
                )?;
            }
            scene.terrain = Some(hf);
            plan.terrain_from_mesh = true;
        }

        // Static meshes: inline first, then the DO27 file's entries.
        let (mut placed, mut stats) = add_meshes(
            &mut scene,
            &self.meshes,
            &root,
            loader,
            manifest.as_mut(),
            self.default_albedo,
            "meshes",
        )?;
        if let Some(file) = &self.static_meshes {
            let path = root.join(file);
            let text = self.read_text(&path, manifest.as_mut(), "static_meshes")?;
            let mut import = parse_static_meshes_json(&text, "static_meshes.json")?;
            if let Some(t) = &self.terrain_mesh {
                // The terrain mesh is the heightfield already.
                import.meshes.retain(|m| {
                    let same = m.source == t.source;
                    if same {
                        plan.skipped
                            .push((m.id.clone().unwrap_or_default(), "terrain_mesh".into()));
                    }
                    !same
                });
            }
            plan.skipped.extend(
                import
                    .skipped
                    .iter()
                    .map(|(id, why)| (id.clone(), (*why).to_string())),
            );
            let (more, s) = add_meshes(
                &mut scene,
                &import.meshes,
                &root,
                loader,
                manifest.as_mut(),
                self.default_albedo,
                "static_meshes.json: meshes",
            )?;
            placed.extend(more);
            stats.entries += import.entries;
            stats.sources += s.sources;
            stats.placements += s.placements;
            stats.triangles += s.triangles;
            stats.mirrored += s.mirrored;
            stats.skipped += s.skipped + import.skipped.len();
            stats.default_albedo_parts += s.default_albedo_parts;
        }
        plan.meshes = stats;

        let mut interiors = self.interiors.clone();
        if let Some(file) = &self.interiors_file {
            let path = root.join(file);
            let text = self.read_text(&path, manifest.as_mut(), "interiors_file")?;
            interiors.extend(parse_interiors_json(&text, "interiors.json")?);
        }
        plan.interiors = interiors.len();

        // The first sun is the scene's sun, the rest its extra suns (ADR-7818): one sun builds the
        // scene (and so the bake bytes) a v2 file's `sun` did.
        let mut suns = self.suns.iter().map(|s| PtSun {
            toward: s.toward,
            color: s.color,
            illuminance: s.illuminance,
            angular_radius: s.angular_radius,
        });
        scene.sun = suns.next();
        scene.extra_suns = suns.collect();
        if let Some(c) = self.sky {
            scene.sky = PtSky::Uniform(c);
        }
        let mut hdri_report = None;
        if let Some(h) = &self.sky_hdri {
            let path = root.join(&h.source);
            if let Some(m) = manifest.as_mut() {
                m.verify(&path, "sky_hdri.source")?;
            }
            let hdri = load_sky_hdri(h, &path)?;
            scene.sky = hdri.sky;
            if let Some(sun) = hdri.sun {
                scene.sun = Some(sun);
            }
            hdri_report = Some(hdri.report);
        }
        if let (Some(a), Some(sun)) = (&self.sky_atmosphere, &scene.sun) {
            let (sky, report) = atmosphere_sky(a, sun.toward);
            scene.sky = sky;
            hdri_report = Some(report);
        }
        for b in &self.sky_bodies {
            let mut body = PtSkyBody::uniform(b.direction, b.angular_radius, b.radiance);
            body.light_scale = b.light_scale;
            scene.sky_bodies.push(body);
        }
        let emitters: Vec<_> = self
            .emitters
            .iter()
            .enumerate()
            .map(|(i, e)| add_emitter(&mut scene, i, e))
            .collect();
        plan.manifest_verified = manifest.as_ref().map_or(0, Manifest::verified);
        plan.lights = light_inputs(&scene, hdri_report, emitters);
        Ok(BuiltScene {
            scene,
            placed,
            interiors,
            plan,
        })
    }

    fn read_text(
        &self,
        path: &Path,
        manifest: Option<&mut Manifest>,
        key: &str,
    ) -> Result<String, BakeDescError> {
        if let Some(m) = manifest {
            m.verify(path, key)?;
        }
        std::fs::read_to_string(path)
            .map_err(|e| BakeDescError::missing(key, format!("{}: {e}", path.display())))
    }

    fn cut_holes(
        &self,
        hf: &mut PtHeightfield,
        path: &Path,
        manifest: Option<&mut Manifest>,
        key: &str,
    ) -> Result<(), BakeDescError> {
        if let Some(m) = manifest
            && path.exists()
        {
            m.verify(path, key)?;
        }
        let mask = regia_terrain::load_holes_ron(path)
            .map_err(|e| BakeDescError::invalid(key, format!("{}: {e}", path.display())))?;
        if let Some(mask) = mask {
            hf.cut_holes(&mask);
        }
        Ok(())
    }

    fn layout(
        &self,
        scene: &PtScene,
        placed: &[PlacedMesh],
        interiors: &[InteriorDesc],
        plan: &mut BakePlan,
    ) -> Result<VolumeLayout, BakeDescError> {
        let l = &self.layout;
        let terrain = match (l.terrain_spacing, &scene.terrain) {
            (Some(spacing), Some(hf)) => Some(
                terrain_layer(hf, spacing, &l.levels)
                    .map_err(|e| BakeDescError::layout(Some("layout.terrain_spacing"), e.to_string()))?,
            ),
            (Some(_), None) => {
                return Err(BakeDescError::invalid(
                    "layout.terrain_spacing",
                    "a terrain layer needs a terrain".into(),
                ));
            }
            (None, _) => None,
        };
        let mut bricks: Vec<DetailBrick> = Vec::new();
        let mut sources: Vec<BrickSource> = Vec::new();
        for (index, b) in l.bricks.iter().enumerate() {
            bricks.push(detail_brick(b.min, b.max, b.spacing));
            sources.push(BrickSource::Explicit { index });
        }
        if let Some(a) = &l.auto_bricks {
            // Interiors first, each at its own spacing; they share the probe budget with the
            // clusters, which coarsen first (ADR-357).
            let fixed: Vec<BrickBox> = if a.interiors {
                interiors
                    .iter()
                    .map(|i| (i.min, i.max, i.spacing.unwrap_or(a.spacing)))
                    .collect()
            } else {
                Vec::new()
            };
            let chosen: Vec<&PlacedMesh> = placed
                .iter()
                .filter(|p| {
                    let layer = p.layer.as_deref().unwrap_or("");
                    if a.layers.is_empty() {
                        layer != "terrain"
                    } else {
                        a.layers.iter().any(|w| w == layer)
                    }
                })
                .collect();
            let boxes: Vec<([f32; 3], [f32; 3])> = chosen.iter().map(|p| (p.min, p.max)).collect();
            let auto = auto_bricks(&boxes, a.spacing, a.margin, a.max_probes as usize, &fixed)
                .map_err(|e| BakeDescError::layout(Some("layout.auto_bricks"), e))?;
            let interior_bricks = fixed.len();
            for (brick, i) in auto.bricks.iter().zip(interiors.iter().take(interior_bricks)) {
                bricks.push(*brick);
                sources.push(BrickSource::Interior { id: i.id.clone() });
            }
            for (brick, cluster) in auto.bricks[interior_bricks..].iter().zip(&auto.clusters) {
                let mut ids: Vec<String> = cluster.members.iter().map(|&k| chosen[k].id.clone()).collect();
                ids.sort();
                ids.dedup();
                bricks.push(*brick);
                sources.push(BrickSource::Cluster {
                    meshes: ids,
                    placements: cluster.members.len(),
                });
            }
            plan.auto_bricks = Some(AutoBrickReport {
                requested_spacing: a.spacing,
                spacing: auto.spacing,
                scale: auto.scale,
                interior_scale: auto.interior_scale,
                placements: chosen.len(),
                clusters: auto.clusters.len(),
                probes: auto.probes,
                interiors: interior_bricks,
            });
        }
        plan.bricks = bricks
            .iter_mut()
            .zip(sources)
            .map(|(b, source)| {
                let (interior, interior_flag, sky_open) = classify(b, interiors);
                if interior_flag {
                    // The runtime only reads the brick's flag word, not the plan. The test is the
                    // whole brick box inside the interior (`classify`), not its centre: a cluster
                    // brick that reaches out of the room over the apron stays outdoor.
                    b.flags |= BrickFlags::INTERIOR;
                }
                BrickPlan {
                    source,
                    interior,
                    interior_flag,
                    sky_open,
                }
            })
            .collect();
        let largest = terrain
            .iter()
            .flat_map(|t| {
                [
                    t.spacing[0],
                    t.spacing[1],
                    t.levels.last().copied().unwrap_or(0.0),
                ]
            })
            .chain(bricks.iter().flat_map(|b| b.spacing))
            .fold(0.0f32, f32::max);
        check_probe_budget(terrain.as_ref(), &bricks)
            .map_err(|e| BakeDescError::layout(None, e.to_string()))?;
        let mut layout = VolumeLayout {
            terrain,
            bricks,
            moment_resolution: l.moment_resolution,
            max_distance: l.max_distance.unwrap_or(2.0 * largest.max(0.5)),
        };
        layout.assign_brick_offsets();
        if layout.probe_count() == 0 {
            return Err(BakeDescError::layout(None, "the layout has no probes".into()));
        }
        layout
            .validate()
            .map_err(|e| BakeDescError::layout(None, e.to_string()))?;
        let bytes = layout.runtime_buffer_bytes();
        plan.volume_bytes = bytes;
        plan.split_parts.clear();
        plan.split_cropped_bricks.clear();
        if bytes > PORTABLE_VOLUME_BYTES {
            // Past one portable binding the bake writes a split volume (ADR-2618), cropping the
            // bricks that cross a cut when no whole-brick split fits (ADR-3237); only a layout no
            // split fits (a map past four parts of the binding) fails.
            match regia_asset::baked_gi_split::split_layout(&layout, PORTABLE_VOLUME_BYTES) {
                Ok(parts) => {
                    plan.split_parts = parts.iter().map(|p| p.runtime_buffer_bytes()).collect();
                    let cropped: Vec<usize> =
                        parts.iter().map(|p| p.split.cropped_bricks(&p.layout)).collect();
                    if cropped.iter().any(|c| *c > 0) {
                        plan.split_cropped_bricks = cropped;
                    }
                }
                Err(split) => {
                    let mib = |b: u64| b as f64 / f64::from(1u32 << 20);
                    return Err(BakeDescError {
                        code: "bake_volume_too_large",
                        path: Some("layout".into()),
                        message: format!(
                            "{} probes need {bytes} bytes ({:.1} MiB) in one GPU binding, above the \
                             portable {:.0} MiB, and {split}: coarsen terrain_spacing or \
                             auto_bricks.spacing, lower auto_bricks.max_probes, or lower \
                             moment_resolution",
                            layout.probe_count(),
                            mib(bytes),
                            mib(PORTABLE_VOLUME_BYTES)
                        ),
                    });
                }
            }
        }
        Ok(layout)
    }
}

/// The sun.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SunDesc {
    /// Vector toward the sun.
    pub toward: [f32; 3],
    /// Illuminance at normal incidence, lux.
    pub illuminance: f32,
    /// Linear color.
    #[serde(default = "white")]
    pub color: [f32; 3],
    /// Angular radius, radians (0 = delta).
    #[serde(default)]
    pub angular_radius: f32,
}

fn white() -> [f32; 3] {
    [1.0; 3]
}

/// A uniform disc in the sky.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkyBodyDesc {
    /// Vector toward the disc's centre.
    pub direction: [f32; 3],
    /// Angular radius, radians.
    pub angular_radius: f32,
    /// Radiance, cd/m².
    pub radiance: [f32; 3],
    /// Lighting multiplier (the host's fill over the disc's own illuminance).
    #[serde(default = "one")]
    pub light_scale: f32,
}

fn one() -> f32 {
    1.0
}

/// [`LightInputs::of_scene`] with what the description's HDRI and emitters added.
fn light_inputs(
    scene: &PtScene,
    hdri: Option<crate::bake_inputs::SkyInput>,
    emitters: Vec<crate::bake_inputs::EmitterReport>,
) -> LightInputs {
    let mut lights = LightInputs::of_scene(scene);
    if let Some(h) = hdri {
        lights.sky.source = h.source;
        lights.sky.source_size = h.source_size;
        lights.sky.downsample = h.downsample;
        if h.sun_extraction.is_some()
            && let Some(sun) = lights.sun.as_mut()
        {
            sun.from_hdri = true;
        }
        lights.sky.sun_extraction = h.sun_extraction;
    }
    lights.emitter_power_lm = emitters.iter().map(|e| e.power_lm).sum();
    lights.emitters = emitters;
    lights
}

fn builtin_plan(scene: &PtScene, layout: &VolumeLayout) -> BakePlan {
    BakePlan {
        format_version: BAKE_DESC_FORMAT_VERSION,
        lights: LightInputs::of_scene(scene),
        bricks: layout
            .bricks
            .iter()
            .map(|_| BrickPlan {
                source: BrickSource::Builtin,
                interior: None,
                interior_flag: false,
                sky_open: false,
            })
            .collect(),
        volume_bytes: layout.runtime_buffer_bytes(),
        ..BakePlan::default()
    }
}

/// A built-in scene by name ([`BUILTIN_SCENES`]): `cornell` (the Cornell box with a 5 cm detail
/// brick), `furnace` (uniform sky, empty, a 3×3×3 brick), `rolling-hills` (a 256 m procedural
/// heightfield under a sun and a uniform sky with a 2.5D terrain layer).
#[must_use]
pub fn builtin(name: &str) -> Option<GiBakeJob> {
    let settings = GiBakeSettings::default();
    let (scene, mut layout) = match name {
        "cornell" => (
            scenes::cornell_box(4, 4, true),
            VolumeLayout {
                terrain: None,
                bricks: vec![detail_brick([0.03, 0.03, 0.03], [0.53, 0.52, 0.53], 0.05)],
                moment_resolution: 8,
                max_distance: 1.0,
            },
        ),
        "furnace" => {
            let mut scene = PtScene::new(camera());
            scene.sky = PtSky::Uniform([1.0; 3]);
            (
                scene,
                VolumeLayout {
                    terrain: None,
                    bricks: vec![detail_brick([-1.0; 3], [1.0; 3], 1.0)],
                    moment_resolution: 4,
                    max_distance: 4.0,
                },
            )
        }
        "rolling-hills" => {
            let mut scene = PtScene::new(camera());
            let n = 257u32;
            let spacing = 1.0f32;
            let half = (n - 1) as f32 * spacing * 0.5;
            let mut heights = Vec::with_capacity((n * n) as usize);
            for j in 0..n {
                for i in 0..n {
                    let (x, z) = (i as f32 * spacing - half, j as f32 * spacing - half);
                    heights.push(
                        6.0 * (x * 0.045).sin() * (z * 0.037).cos() + 3.0 * (x * 0.11 + z * 0.07).sin(),
                    );
                }
            }
            let ground = scene.add_material(PtMaterial::lambert([0.35, 0.3, 0.26]));
            scene.terrain = Some(PtHeightfield {
                width: n,
                depth: n,
                origin: [-half, -half],
                spacing: [spacing; 2],
                heights,
                material: ground,
            });
            // A boulder for the relocation and visibility paths.
            let rock = scene.add_mesh(scenes::uv_sphere(4.0, 32, 16, false));
            let grey = scene.add_material(PtMaterial::lambert([0.4; 3]));
            scene.add_instance(rock, grey, Mat4::from_translation(Vec3::new(10.0, 2.0, -6.0)));
            scene.sun = Some(PtSun {
                toward: Vec3::new(0.4, 0.6, 0.3).normalize().to_array(),
                color: [1.0, 0.96, 0.9],
                illuminance: 100_000.0,
                angular_radius: 0.004_65,
            });
            scene.sky = PtSky::Uniform([2500.0, 3500.0, 5000.0]);
            let hf = scene.terrain.as_ref()?;
            let terrain = terrain_layer(hf, [8.0, 8.0], &[1.0, 2.0, 4.0, 8.0, 16.0]).ok()?;
            (
                scene,
                VolumeLayout {
                    terrain: Some(terrain),
                    bricks: Vec::new(),
                    moment_resolution: 4,
                    max_distance: 32.0,
                },
            )
        }
        _ => return None,
    };
    layout.assign_brick_offsets();
    let plan = builtin_plan(&scene, &layout);
    Some(GiBakeJob {
        scene,
        layout,
        settings,
        plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bake_meshes::{BakeMeshPart, BakeMeshSource};
    use crate::scene::PtMesh;

    #[test]
    fn builtins_build() {
        for name in BUILTIN_SCENES {
            let job = builtin(name).expect(name);
            assert!(job.layout.probe_count() > 0, "{name}");
            job.layout.validate().expect(name);
            assert_eq!(job.plan.bricks.len(), job.layout.bricks.len());
        }
        assert!(builtin("nope").is_none());
        let hills = builtin("rolling-hills").expect("hills");
        assert_eq!(hills.layout.probe_count(), 32 * 32 * 5);
    }

    /// Today's provisional description (format v0, no `format_version`), as committed before v1.
    const V0: &str = r#"(
        terrain: Some((heights: "map.r16", width: 33, depth: 33, world_width: 64.0, height_scale: 10.0)),
        sun: Some((toward: (0.0, 1.0, 0.0), illuminance: 1000.0)),
        sky: Some((1.0, 1.0, 1.0)),
        sky_bodies: [(direction: (0.0, 0.5, 0.8), angular_radius: 0.2, radiance: (5.0, 4.0, 3.0), light_scale: 0.5)],
        layout: (terrain_spacing: Some((8.0, 8.0)), levels: [1.0, 3.0], bricks: [(min: (0.0, 5.0, 0.0), max: (2.0, 7.0, 2.0), spacing: 1.0)]),
        bake: (rays_per_probe: 32, samples: 2),
    )"#;

    fn heightfield_dir(tag: &str) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir(tag);
        let samples: Vec<u8> = (0..33u16 * 33).flat_map(|k| (k * 7).to_le_bytes()).collect();
        std::fs::write(dir.join("map.r16"), samples).expect("write");
        dir
    }

    #[test]
    fn a_description_loads_its_heightfield() {
        let dir = heightfield_dir("regia_bake_desc");
        let desc = GiBakeDescription::from_ron(V0).expect("parse");
        let job = desc.build(&dir).expect("build");
        let t = job.layout.terrain.as_ref().expect("terrain layer");
        assert_eq!(t.counts, [8, 8]);
        assert_eq!(job.layout.bricks[0].first_probe, 128);
        assert_eq!(job.layout.probe_count(), 128 + 27);
        assert_eq!(job.settings.trace.rays_per_probe, 32);
        assert_eq!(job.scene.sky_bodies[0].light_scale, 0.5);
        assert!((job.layout.max_distance - 16.0).abs() < 1e-6);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(GiBakeDescription::from_ron("(layout: (), extra: 1)").is_err());
        let missing = GiBakeDescription::from_ron(
            r#"(terrain: Some((heights: "none.r16", width: 2, depth: 2, world_width: 1.0, height_scale: 1.0)), layout: (levels: [1.0]))"#,
        )
        .expect("parse");
        assert!(missing.build(Path::new("/nonexistent")).is_err());
    }

    /// A v0 file reads as v0, migrates on disk to the current version by stamping it (comments
    /// kept), and both build the same job: same layout, settings and scene hash.
    #[test]
    fn a_v0_file_migrates_and_builds_identically() {
        let dir = heightfield_dir("regia_bake_desc_v0");
        let text = format!("// Bore Twelve stand-in\n{V0}");
        assert_eq!(bake_desc_version(&text), 0);
        let (v0, found) = GiBakeDescription::parse_versioned(&text).expect("v0");
        assert_eq!((found, v0.format_version), (0, BAKE_DESC_FORMAT_VERSION));
        let migrated = migrate_bake_ron(&text).expect("migrate");
        assert!(
            migrated.starts_with(&format!(
                "// Bore Twelve stand-in\n(\n    format_version: {BAKE_DESC_FORMAT_VERSION},"
            )),
            "{migrated}"
        );
        assert_eq!(bake_desc_version(&migrated), BAKE_DESC_FORMAT_VERSION);
        assert_eq!(
            migrate_bake_ron(&migrated).expect("current"),
            migrated,
            "idempotent"
        );
        let (v1, found) = GiBakeDescription::parse_versioned(&migrated).expect("current");
        assert_eq!(found, BAKE_DESC_FORMAT_VERSION);
        let a = v0.build(&dir).expect("v0 job");
        let b = v1.build(&dir).expect("current job");
        assert_eq!(a.layout, b.layout);
        assert_eq!(a.settings, b.settings);
        assert_eq!(a.scene.content_hash(), b.scene.content_hash());
        assert_eq!(a.plan.format_version, BAKE_DESC_FORMAT_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
        let newer = GiBakeDescription::parse_versioned(&format!(
            "(format_version: {}, layout: ())",
            BAKE_DESC_FORMAT_VERSION + 1
        ))
        .expect_err("newer");
        assert_eq!(newer.code, "bake_desc_newer");
        assert_eq!(bake_desc_migration_sources(), [0, 1, 2]);
    }

    struct Boxes;

    impl BakeMeshLoader for Boxes {
        fn load(&mut self, path: &Path) -> Result<BakeMeshSource, String> {
            if path.ends_with("missing.gltf") {
                return Err("no such file".into());
            }
            Ok(BakeMeshSource {
                parts: vec![BakeMeshPart::plain(
                    crate::scenes::uv_sphere(1.0, 8, 4, false),
                    [0.4; 3],
                )],
            })
        }

        fn companions(&self, _path: &Path) -> Vec<PathBuf> {
            Vec::new()
        }
    }

    const V1: &str = r#"(
        format_version: 1,
        root: Some("bundle"),
        sky: Some((1.0, 1.0, 1.0)),
        default_albedo: (0.45, 0.45, 0.45),
        meshes: [
            (id: Some("rock"), source: "rock.gltf", layer: Some("scatter"),
             instances: Some([
                (1.0, 0.0, 0.0, 0.0,  0.0, 1.0, 0.0, 0.0,  0.0, 0.0, 1.0, 0.0,  10.0, 0.0, 0.0, 1.0),
                (1.0, 0.0, 0.0, 0.0,  0.0, 1.0, 0.0, 0.0,  0.0, 0.0, 1.0, 0.0,  11.5, 0.0, 0.0, 1.0),
             ]),
             material: Some((albedo: Some((0.2, 0.2, 0.2))))),
            (id: Some("hab"), source: "hab.gltf", layer: Some("setpiece"),
             transform: (-1.0, 0.0, 0.0, 0.0,  0.0, 1.0, 0.0, 0.0,  0.0, 0.0, 1.0, 0.0,  -40.0, 2.0, 0.0, 1.0)),
            (id: Some("crate"), source: "crate.gltf", static: false),
        ],
        interiors: [(id: "room", min: (-44.0, -2.0, -4.0), max: (-36.0, 6.0, 4.0))],
        layout: (
            bricks: [(min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), spacing: 0.5)],
            auto_bricks: Some((spacing: 1.0, margin: 1.0, max_probes: 100000)),
            moment_resolution: 8,
        ),
        bake: (rays_per_probe: 64, samples: 2, seed: 9),
    )"#;

    /// A v1 description round trips through the parser with every new field, and builds: the
    /// rock instances cluster into one brick, the mirrored hab into another, the room gets an
    /// interior brick tagged INTERIOR, the non-static crate is skipped.
    #[test]
    fn a_v1_description_round_trips_and_builds() {
        let (desc, found) = GiBakeDescription::parse_versioned(V1).expect("v1");
        assert_eq!(found, 1);
        assert_eq!(desc.root.as_deref(), Some(Path::new("bundle")));
        assert_eq!(desc.meshes.len(), 3);
        assert_eq!(desc.meshes[0].instances.as_ref().map(Vec::len), Some(2));
        assert!(!desc.meshes[2].is_static);
        assert_eq!(desc.meshes[1].transform[0], -1.0);
        assert_eq!(
            desc.meshes[0].material.as_ref().and_then(|m| m.albedo),
            Some([0.2; 3])
        );
        // Migration to v2 only moves the version, and it parses back to the same thing.
        let again = migrate_bake_ron(V1).expect("v1 -> v2");
        assert_eq!(
            again,
            V1.replacen(
                "format_version: 1,",
                &format!("format_version: {BAKE_DESC_FORMAT_VERSION},"),
                1
            )
        );
        let (back, _) = GiBakeDescription::parse_versioned(&again).expect("again");
        assert_eq!(back.meshes, desc.meshes);
        assert_eq!(back.interiors, desc.interiors);

        let job = desc.build_with(Path::new("/maps"), &mut Boxes).expect("build");
        let plan = &job.plan;
        assert_eq!(plan.meshes.placements, 3);
        assert_eq!(plan.meshes.mirrored, 1);
        assert_eq!(plan.meshes.skipped, 1);
        assert_eq!(plan.meshes.sources, 2);
        // Explicit, interior, then the clusters sorted by box (hab at x = -40 before the rocks).
        assert_eq!(plan.bricks.len(), 4);
        assert_eq!(plan.bricks[0].source, BrickSource::Explicit { index: 0 });
        assert_eq!(plan.bricks[1].source, BrickSource::Interior { id: "room".into() });
        assert!(plan.bricks[1].interior_flag);
        assert!(job.layout.bricks[1].is_interior());
        for (brick, planned) in job.layout.bricks.iter().zip(&plan.bricks) {
            assert_eq!(brick.is_interior(), planned.interior_flag);
        }
        // The plan's flag reaches the baked layout's brick (the explicit brick stays outdoor).
        assert_eq!(job.layout.bricks[1].flags, BrickFlags::INTERIOR);
        assert_eq!(job.layout.bricks[0].flags, 0);
        assert_eq!(
            plan.bricks[2].source,
            BrickSource::Cluster {
                meshes: vec!["hab".into()],
                placements: 1
            }
        );
        // The hab's cluster box (grown by 1 m) lies inside the room: tagged too.
        assert_eq!(plan.bricks[2].interior.as_deref(), Some("room"));
        assert_eq!(
            plan.bricks[3].source,
            BrickSource::Cluster {
                meshes: vec!["rock".into()],
                placements: 2
            }
        );
        assert!(!plan.bricks[3].interior_flag);
        let auto = plan.auto_bricks.as_ref().expect("auto");
        assert_eq!((auto.clusters, auto.interiors, auto.placements), (2, 1, 3));
        assert_eq!(job.layout.bricks.len(), 4);
        // The plan's INTERIOR decision reaches the volume's brick flags (ADR-365): the room's
        // brick and the hab's cluster inside it, not the explicit brick or the rocks.
        let interior: Vec<bool> = job.layout.bricks.iter().map(DetailBrick::is_interior).collect();
        assert_eq!(interior, [false, true, true, false]);
        assert_eq!(job.layout.moment_resolution, 8);
        assert_eq!(job.settings.trace.seed, 9);
        // Rocks: radius 1 at x = 10 and 11.5 grown by 1 m → one brick from x = 8 to 13.5.
        let rocks = &job.layout.bricks[3];
        assert!((rocks.origin[0] - 8.0).abs() < 1e-4, "{:?}", rocks.origin);
        // Materials: the override reaches the rocks.
        let rock_material = job.scene.instances[0].material;
        assert_eq!(
            job.scene.materials[rock_material].base_color,
            [0.2, 0.2, 0.2, 1.0]
        );
    }

    /// `regia bake-gi --dry-run` reports a bad key by its path.
    #[test]
    fn invalid_descriptions_report_the_key_path() {
        let cases = [
            (
                V1.replace("spacing: 1.0, margin", "spacing: -1.0, margin"),
                "layout.auto_bricks.spacing",
            ),
            (
                V1.replace("max_probes: 100000", "max_probes: 0"),
                "layout.auto_bricks.max_probes",
            ),
            (
                V1.replace("spacing: 0.5", "spacing: 0.0"),
                "layout.bricks[0].spacing",
            ),
            (
                V1.replace("max: (-36.0, 6.0, 4.0)", "max: (-46.0, 6.0, 4.0)"),
                "interiors[0].max",
            ),
            (
                V1.replace("(0.2, 0.2, 0.2)", "(2.0, 0.2, 0.2)"),
                "meshes[0].material.albedo",
            ),
            (
                V1.replace("moment_resolution: 8", "moment_resolution: 0"),
                "layout.moment_resolution",
            ),
        ];
        for (text, key) in cases {
            let desc = GiBakeDescription::from_ron(&text).expect(key);
            let err = desc.build_with(Path::new("/maps"), &mut Boxes).expect_err(key);
            assert_eq!(err.code, "bake_desc_invalid", "{err}");
            assert_eq!(err.path.as_deref(), Some(key), "{err}");
            assert!(err.to_string().starts_with(key), "{err}");
        }
        let desc = GiBakeDescription::from_ron(&V1.replace("hab.gltf", "missing.gltf")).expect("parse");
        let err = desc.build_with(Path::new("/maps"), &mut Boxes).expect_err("load");
        assert_eq!(err.code, "bake_mesh_load");
        assert_eq!(err.path.as_deref(), Some("meshes[1].source"));
        let err = GiBakeDescription::parse_versioned(&V1.replace("default_albedo", "default_albedoo"))
            .expect_err("unknown field");
        assert_eq!(err.code, "bake_desc_parse");
        assert!(err.message.contains("default_albedoo"), "{err}");
        // Cooked meshes only without the CLI's loader.
        let err = GiBakeDescription::from_ron(V1)
            .expect("parse")
            .build_with(Path::new("/maps"), &mut CookedMeshLoader)
            .expect_err("gltf");
        assert_eq!(err.code, "bake_mesh_load");
    }

    /// The DO27 bundle path: `static_meshes.json` + `interiors.json` under the root, checked
    /// against `MANIFEST.sha256`; a sky-open interior's brick is tagged but not INTERIOR; the
    /// terrain mesh becomes the heightfield and is not placed again.
    #[test]
    fn a_bundle_description_reads_the_do27_files() {
        let dir = regia_test_utils::unique_temp_dir("regia_bake_desc_bundle");
        let bundle = dir.join("bundle");
        std::fs::create_dir_all(&bundle).expect("dir");
        let json = r#"{"header": {"version": 1, "units": "metres", "up": "+Y", "handedness": "right"},
            "meshes": [
              {"id": "terrain", "transform": [1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1], "source": "terrain.gltf",
               "static": true, "layer": "terrain"},
              {"id": "shaft", "transform": [-1,0,0,0, 0,1,0,0, 0,0,1,0, 0,-5,0,1], "source": "shaft.gltf",
               "static": true, "layer": "setpiece", "mirror_x": true},
              {"id": "stones", "transform": [1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1], "source": "s.gltf",
               "static": true, "layer": "scatter", "instances": null, "placement": {"seed": 1}}
            ]}"#;
        let interiors = r#"{"format": "do27-bake-interiors/1", "interiors": [
            {"id": "shaft", "aabb_min": [-3, -9, -3], "aabb_max": [3, 1, 3], "sky_open": true}]}"#;
        std::fs::write(bundle.join("static_meshes.json"), json).expect("write");
        std::fs::write(bundle.join("interiors.json"), interiors).expect("write");
        let hash = |p: &str| crate::sha256::file_hex(&bundle.join(p)).expect("hash");
        let manifest = format!(
            "{}  static_meshes.json\n{}  interiors.json\n",
            hash("static_meshes.json"),
            hash("interiors.json")
        );
        std::fs::write(bundle.join("MANIFEST.sha256"), &manifest).expect("write");

        struct Grid;
        impl BakeMeshLoader for Grid {
            fn load(&mut self, path: &Path) -> Result<BakeMeshSource, String> {
                let mesh = if path.ends_with("terrain.gltf") {
                    // A 20 m flat square at y = 0 (two triangles).
                    PtMesh {
                        positions: vec![
                            [-10.0, 0.0, -10.0],
                            [10.0, 0.0, -10.0],
                            [10.0, 0.0, 10.0],
                            [-10.0, 0.0, 10.0],
                        ],
                        normals: vec![[0.0, 1.0, 0.0]; 4],
                        indices: vec![0, 2, 1, 0, 3, 2],
                        ..PtMesh::default()
                    }
                } else {
                    crate::scenes::uv_sphere(2.0, 8, 4, false)
                };
                Ok(BakeMeshSource {
                    parts: vec![BakeMeshPart::plain(mesh, [0.5; 3])],
                })
            }
            fn companions(&self, _path: &Path) -> Vec<PathBuf> {
                Vec::new()
            }
        }

        let text = r#"(
            format_version: 1,
            root: Some("bundle"),
            manifest: Some("MANIFEST.sha256"),
            terrain_mesh: Some((source: "terrain.gltf", spacing: 1.0)),
            static_meshes: Some("static_meshes.json"),
            interiors_file: Some("interiors.json"),
            layout: (terrain_spacing: Some((4.0, 4.0)), levels: [1.0, 2.0],
                     auto_bricks: Some((spacing: 1.0, margin: 0.5, interiors: false))),
        )"#;
        let desc = GiBakeDescription::from_ron(text).expect("parse");
        // The glTF sources are not in the manifest: refused.
        let err = desc.build_with(&dir, &mut Grid).expect_err("unlisted");
        assert_eq!(err.code, "bake_manifest_missing_entry");
        assert_eq!(err.path.as_deref(), Some("terrain_mesh.source"));
        std::fs::write(bundle.join("terrain.gltf"), "{}").expect("write");
        std::fs::write(bundle.join("shaft.gltf"), "{}").expect("write");
        let manifest = format!(
            "{manifest}{}  terrain.gltf\n{}  shaft.gltf\n",
            hash("terrain.gltf"),
            hash("shaft.gltf")
        );
        std::fs::write(bundle.join("MANIFEST.sha256"), &manifest).expect("write");
        let job = desc.build_with(&dir, &mut Grid).expect("build");
        let plan = &job.plan;
        assert!(plan.terrain_from_mesh);
        assert_eq!(plan.manifest_verified, 4);
        assert_eq!(plan.interiors, 1);
        assert_eq!(
            plan.meshes.placements, 1,
            "terrain is the heightfield, stones are generated"
        );
        assert_eq!(plan.meshes.mirrored, 1);
        assert!(
            plan.skipped
                .contains(&("terrain".to_string(), "terrain_mesh".to_string()))
        );
        assert!(
            plan.skipped
                .contains(&("stones".to_string(), "generated_placement".to_string()))
        );
        let hf = job.scene.terrain.as_ref().expect("heightfield");
        assert_eq!((hf.width, hf.depth), (21, 21));
        assert_eq!(job.layout.terrain.as_ref().map(|t| t.counts), Some([5, 5]));
        // The shaft cluster brick (sphere r 2 at y = -5, grown 0.5 m) is inside the sky-open
        // interior: tagged, not INTERIOR.
        let shaft = plan.bricks.last().expect("brick");
        assert_eq!(shaft.interior.as_deref(), Some("shaft"));
        assert!(shaft.sky_open && !shaft.interior_flag);
        assert!(
            job.layout.bricks.iter().all(|b| !b.is_interior()),
            "sky-open: no INTERIOR"
        );
        // A tampered file fails the check.
        std::fs::write(bundle.join("interiors.json"), interiors.replace("-9", "-8")).expect("write");
        let err = desc.build_with(&dir, &mut Grid).expect_err("tampered");
        assert_eq!(err.code, "bake_manifest_mismatch");
        assert_eq!(err.path.as_deref(), Some("interiors_file"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The example in `docs/formats/bake-ron.md` parses and validates.
    #[test]
    fn the_format_doc_example_parses() {
        let doc = include_str!("../../../docs/formats/bake-ron.md");
        let start = doc.find("```ron\n").expect("ron fence") + "```ron\n".len();
        let len = doc[start..].find("```").expect("fence end");
        let (desc, found) = GiBakeDescription::parse_versioned(&doc[start..start + len]).expect("example");
        assert_eq!(found, BAKE_DESC_FORMAT_VERSION);
        desc.validate().expect("valid");
        assert!(desc.layout.auto_bricks.is_some());
        assert!(desc.bake.sample_groups >= 3 && desc.bake.firefly_threshold > 1.0);
    }

    /// v1 → v2 rewrites the root's version number in place: comments, a `format_version` in a
    /// comment or a string, and every other byte stay. The v2 file bakes with the v1 settings.
    /// A description of `placements` meshes (each with a full transform, as a level's export
    /// writes them), `format_version` first or after the meshes.
    fn many_placements(placements: usize, version_last: bool) -> String {
        let meshes: Vec<String> = (0..placements)
            .map(|k| {
                let x = k as f32 * 1.25;
                format!(
                    "(id: Some(\"prop_{k}\"), source: \"assets/meshes/prop.gltf\", transform: (1.5, 0.0, 0.0, \
                     0.0, 0.0, 2.25, 0.0, 0.0, 0.0, 0.0, 1.5, 0.0, {x}, 1.125, -{x}, 1.0), layer: Some(\"default\"))"
                )
            })
            .collect();
        let version = format!("format_version: {BAKE_DESC_FORMAT_VERSION},");
        let (first, last) = if version_last {
            ("", version.as_str())
        } else {
            (version.as_str(), "")
        };
        format!(
            "(\n    {first}\n    root: Some(\"\"),\n    meshes: [\n        {}\n    ],\n    sky: Some((1.0, 1.0, 1.0)),\n    \
             layout: (levels: [1.0, 2.0]),\n    {last}\n)\n",
            meshes.join(",\n        ")
        )
    }

    /// The version peek reads the root struct up to `format_version`, not the whole text through
    /// a `ron` value parse (ADR-3121): with the field first the bytes read are the same for 16 or
    /// 4096 placements, and with it last they are at most the text, once. The full parse agrees.
    #[test]
    fn the_version_peek_reads_each_byte_at_most_once_on_any_number_of_placements() {
        let (_, first_read) = version_peek(&many_placements(16, false));
        for placements in [16, 256, 4096] {
            let text = many_placements(placements, false);
            let (version, read) = version_peek(&text);
            assert_eq!(version, BAKE_DESC_FORMAT_VERSION);
            assert_eq!(read, first_read, "{placements} placements");
            assert!(read < 64, "{read} bytes for the first field");

            let text = many_placements(placements, true);
            let (version, read) = version_peek(&text);
            assert_eq!(version, BAKE_DESC_FORMAT_VERSION);
            assert!(read <= text.len(), "{read} of {}", text.len());
            assert_eq!(bake_desc_version(&text), BAKE_DESC_FORMAT_VERSION);
        }
        let text = many_placements(64, true);
        let (desc, found) = GiBakeDescription::parse_versioned(&text).expect("parses");
        assert_eq!(found, BAKE_DESC_FORMAT_VERSION);
        assert_eq!(desc.meshes.len(), 64);
        // No field, or no root struct: version 0, as the old peek read them.
        assert_eq!(version_peek("(root: Some(\"\"), layout: (levels: [1.0]))").0, 0);
        assert_eq!(version_peek("not ron at all").0, 0);
    }

    #[test]
    fn a_v1_file_migrates_to_v2_in_place() {
        let template = "// a v1 description\n(\n    // format_version: 1 in a comment stays\n    \
                        format_version: {v},\n    root: Some(\"maps (format_version: 1)\"),\n    \
                        sky: Some((1.0, 1.0, 1.0)),\n    layout: (bricks: [(min: (0.0, 0.0, 0.0), \
                        max: (1.0, 1.0, 1.0), spacing: 0.5)]),\n    bake: (samples: 4, max_bounces: 3),\n)\n";
        let v1 = template.replace("{v}", "1");
        assert_eq!(bake_desc_version(&v1), 1);
        let migrated = migrate_bake_ron(&v1).expect("migrate");
        assert_eq!(
            migrated,
            template.replace("{v}", &BAKE_DESC_FORMAT_VERSION.to_string())
        );
        assert_eq!(
            migrate_bake_ron(&migrated).expect("current"),
            migrated,
            "idempotent"
        );
        let (a, found_a) = GiBakeDescription::parse_versioned(&v1).expect("v1");
        let (b, found_b) = GiBakeDescription::parse_versioned(&migrated).expect("v2");
        assert_eq!(
            (found_a, found_b, a.format_version),
            (1, BAKE_DESC_FORMAT_VERSION, BAKE_DESC_FORMAT_VERSION)
        );
        assert_eq!(a.bake.settings(), b.bake.settings());
        // The v2 defaults bake as v1 did: no clamp, one sample group, no suppression.
        let t = b.bake.settings().trace;
        assert_eq!(
            (
                t.clamp_direct,
                t.clamp_indirect,
                t.sample_groups,
                t.firefly_threshold
            ),
            (0.0, 0.0, 1, 0.0)
        );
        assert_eq!(t.rr_start, ProbeTraceSettings::default().rr_start);
        // A nested `format_version` (not the root's) is not a version field: the v0 path stamps.
        let root_version_digits = |t: &str| version_number_span(t, root_open_paren(t)?);
        assert_eq!(root_version_digits("(layout: (format_version: 1))"), None);
        assert_eq!(
            root_version_digits("(format_version: 1, layout: ())"),
            Some((17, 18))
        );
    }

    const V2_BAKE_CONTROLS: &str = r#"(
        format_version: 2,
        sky: Some((1.0, 1.0, 1.0)),
        layout: (bricks: [(min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), spacing: 0.5)]),
        bake: (rays_per_probe: 64, samples: 16, max_bounces: 12, rr_start: 5, clamp_direct: 100000.0,
               clamp_indirect: 2000.0, sample_groups: 8, firefly_threshold: 6.0),
    )"#;

    /// The v2 bake controls reach the trace settings.
    #[test]
    fn v2_bake_controls_reach_the_settings() {
        let (desc, found) = GiBakeDescription::parse_versioned(V2_BAKE_CONTROLS).expect("v2");
        assert_eq!(found, 2);
        desc.validate().expect("valid");
        let t = desc.bake.settings().trace;
        assert_eq!((t.max_bounces, t.rr_start, t.sample_groups), (12, 5, 8));
        assert_eq!(
            (t.clamp_direct, t.clamp_indirect, t.firefly_threshold),
            (100_000.0, 2000.0, 6.0)
        );
        let job = desc.build(Path::new("/maps")).expect("build");
        assert_eq!(job.settings.trace.sample_groups, 8);
        assert_eq!(job.plan.format_version, BAKE_DESC_FORMAT_VERSION);
    }

    /// Each bad bake control fails validation with its key.
    #[test]
    fn invalid_bake_controls_report_the_key() {
        let cases = [
            ("max_bounces: 12", "max_bounces: 0", "bake.max_bounces"),
            ("max_bounces: 12", "max_bounces: 65", "bake.max_bounces"),
            ("rr_start: 5", "rr_start: 0", "bake.rr_start"),
            (
                "clamp_direct: 100000.0",
                "clamp_direct: -1.0",
                "bake.clamp_direct",
            ),
            (
                "clamp_indirect: 2000.0",
                "clamp_indirect: -0.5",
                "bake.clamp_indirect",
            ),
            ("sample_groups: 8", "sample_groups: 0", "bake.sample_groups"),
            ("sample_groups: 8", "sample_groups: 17", "bake.sample_groups"),
            (
                "firefly_threshold: 6.0",
                "firefly_threshold: 1.0",
                "bake.firefly_threshold",
            ),
            (
                "firefly_threshold: 6.0",
                "firefly_threshold: 0.5",
                "bake.firefly_threshold",
            ),
            ("sample_groups: 8", "sample_groups: 2", "bake.sample_groups"),
        ];
        for (from, to, key) in cases {
            assert!(V2_BAKE_CONTROLS.contains(from), "{from}");
            let desc = GiBakeDescription::from_ron(&V2_BAKE_CONTROLS.replace(from, to)).expect(to);
            let err = desc.validate().expect_err(to);
            assert_eq!(err.code, "bake_desc_invalid", "{to}: {err}");
            assert_eq!(err.path.as_deref(), Some(key), "{to}: {err}");
        }
        // Off is valid with any group count; 64 groups is the most.
        for (from, to) in [
            ("firefly_threshold: 6.0", "firefly_threshold: 0.0"),
            (
                "sample_groups: 8, firefly_threshold: 6.0",
                "sample_groups: 1, firefly_threshold: 0.0",
            ),
            ("clamp_direct: 100000.0", "clamp_direct: 0.0"),
        ] {
            let desc = GiBakeDescription::from_ron(&V2_BAKE_CONTROLS.replace(from, to)).expect(to);
            desc.validate().expect(to);
        }
        let mut s = GiBakeSettings::default();
        s.trace.samples = 100;
        s.trace.sample_groups = 64;
        validate_bake_settings(&s).expect("64 groups");
        s.trace.sample_groups = 65;
        assert_eq!(
            validate_bake_settings(&s).expect_err("65").path.as_deref(),
            Some("bake.sample_groups")
        );
        validate_bake_settings(&GiBakeSettings::default()).expect("defaults");
    }

    /// `rays_per_probe` outside 1..=4096 is not a validation error. Both tracers clamp it
    /// when they trace; the stored value is what the summary prints.
    #[test]
    fn rays_per_probe_above_4096_still_validates() {
        let mut s = GiBakeSettings::default();
        s.trace.rays_per_probe = 8192;
        validate_bake_settings(&s).expect("8192 is stored and traced as 4096");
        s.trace.rays_per_probe = 4096;
        validate_bake_settings(&s).expect("4096");
        s.trace.rays_per_probe = 0;
        validate_bake_settings(&s).expect("0 is traced as 1");
    }

    #[test]
    fn the_schema_fingerprint_lists_the_v1_fields() {
        let f = bake_desc_schema_fingerprint();
        for field in [
            "format_version",
            "meshes",
            "static",
            "auto_bricks",
            "max_probes",
            "sky_open",
            "terrain_mesh",
            "rr_start",
            "clamp_direct",
            "clamp_indirect",
            "sample_groups",
            "firefly_threshold",
        ] {
            assert!(f.contains(field), "{field}: {f}");
        }
    }

    fn build_v1(text: &str) -> GiBakeJob {
        GiBakeDescription::from_ron(text)
            .expect("parse")
            .build_with(Path::new("/maps"), &mut Boxes)
            .expect("build")
    }

    /// A volume of `job`'s layout, lights and settings with placeholder probes, through the
    /// `.fgiv` bytes.
    fn encode(job: &GiBakeJob) -> BakedGiVolume {
        use regia_asset::baked_gi::{ProbeData, ProbeFlags, ProbeMeta};
        let l = &job.layout;
        let far = l.max_distance;
        let probes: Vec<ProbeData> = (0..l.probe_count())
            .map(|_| ProbeData {
                sh: [[[0.0; 4]; 3]; regia_asset::baked_gi::GI_COMPONENTS],
                moments: vec![[far, far * far]; (l.moment_resolution * l.moment_resolution) as usize],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        let lights = crate::probe::bake_lights(&job.scene);
        let info = crate::probe::bake_info(&job.scene, &job.settings);
        let mut v = BakedGiVolume::from_probes(l, lights, info, &probes).expect("encode");
        v.emitters = Some(crate::probe::baked_emitters(&job.scene));
        BakedGiVolume::from_bytes(&v.to_bytes().expect("bytes")).expect("decode")
    }

    /// Review baked-light #1: the plan's INTERIOR tag reaches the layout's bricks and the
    /// written `.fgiv` (it used to stay in the plan, so every bake shipped flags 0). The room's
    /// brick and the hab cluster inside it are interior; the explicit brick and the rocks
    /// outdoors are not.
    #[test]
    fn interior_tags_become_brick_flags_in_the_volume() {
        let job = build_v1(V1);
        let flags: Vec<bool> = job.layout.bricks.iter().map(DetailBrick::is_interior).collect();
        assert_eq!(flags, [false, true, true, false]);
        for (b, p) in job.layout.bricks.iter().zip(&job.plan.bricks) {
            assert_eq!(b.is_interior(), p.interior_flag, "{p:?}");
        }
        let volume = encode(&job);
        assert_eq!(volume.interior_brick_count(), 2);
        assert_eq!(volume.layout(), job.layout);
    }

    /// The flag needs the whole brick box inside the interior, not its centre: a cluster brick
    /// centred in the room whose box reaches out over the ground outside stays outdoor (the
    /// centre test of `VolumeLayout::flag_interior_bricks` would flag it). A `sky_open`
    /// interior flags nothing; one with `openings` is INTERIOR as a whole (ADR-279).
    #[test]
    fn interior_flags_need_the_whole_box_and_respect_sky_open() {
        // The hab at x = -37: its cluster box (-39..-35) crosses the room's wall at x = -36.
        let straddling = build_v1(&V1.replace("-40.0, 2.0, 0.0, 1.0", "-37.0, 2.0, 0.0, 1.0"));
        let hab = &straddling.layout.bricks[2];
        assert_eq!(
            straddling.plan.bricks[2].source,
            BrickSource::Cluster {
                meshes: vec!["hab".into()],
                placements: 1
            }
        );
        let c = hab.centre();
        assert!(c[0] > -44.0 && c[0] < -36.0, "centre in the room: {c:?}");
        assert!(!hab.is_interior());
        assert!(straddling.layout.bricks[1].is_interior(), "the room's own brick");
        let mut centre_test = straddling.layout.clone();
        assert_eq!(
            centre_test.flag_interior_bricks(&[([-44.0, -2.0, -4.0], [-36.0, 6.0, 4.0])]),
            2,
            "the centre test would have flagged the straddling brick"
        );

        let open = build_v1(&V1.replace(
            "max: (-36.0, 6.0, 4.0))",
            "max: (-36.0, 6.0, 4.0), sky_open: true)",
        ));
        assert!(open.layout.bricks.iter().all(|b| !b.is_interior()));
        assert_eq!(open.plan.bricks[1].interior.as_deref(), Some("room"));
        let bay = build_v1(&V1.replace(
            "max: (-36.0, 6.0, 4.0))",
            "max: (-36.0, 6.0, 4.0), openings: true)",
        ));
        assert!(bay.layout.bricks[1].is_interior());
    }

    /// Review baked-light #4: the plan reports the runtime binding size, and a layout above the
    /// portable 128 MiB fails at build time with `bake_volume_too_large` instead of baking a
    /// volume the renderer drops on load. A Bore Twelve-sized brick set (262,144 probes at 8 × 8
    /// moments, about 86 MB) still fits.
    #[test]
    fn oversized_volumes_fail_with_a_clear_error() {
        let job = build_v1(V1);
        assert_eq!(job.plan.volume_bytes, job.layout.runtime_buffer_bytes());
        assert!(job.plan.volume_bytes > 0);
        let big = |side: f32, res: u32| {
            format!(
                "(format_version: 1, layout: (bricks: [(min: (0.0, 0.0, 0.0), \
                 max: ({side:?}, {side:?}, {side:?}), spacing: 1.0)], \
                 moment_resolution: {res}, max_distance: Some(8.0)))"
            )
        };
        // 64³ probes at 8 × 8 texels: 262,144 × 328 + 48 bytes, plus the brick lookup grid the
        // renderer uploads behind the brick table (ADR-1835): one brick in 2 × 2 × 2 cells is
        // 8 header + 2 × 8 cell + 8 list words, 128 bytes.
        let fits = build_v1(&big(63.0, 8));
        assert_eq!(fits.layout.probe_count(), 1 << 18);
        assert_eq!(fits.plan.volume_bytes, (1u64 << 18) * 328 + 48 + 128);
        assert!(fits.plan.volume_bytes <= PORTABLE_VOLUME_BYTES);
        // The same brick at 32 × 32 texels needs about 1.0 GiB.
        let err = GiBakeDescription::from_ron(&big(63.0, 32))
            .expect("parse")
            .build_with(Path::new("/maps"), &mut Boxes)
            .expect_err("too large");
        assert_eq!(err.code, "bake_volume_too_large");
        assert_eq!(err.path.as_deref(), Some("layout"));
        assert!(err.message.contains("262144 probes"), "{err}");
        assert!(err.message.contains("128 MiB"), "{err}");
        // Built-in scenes report their size too.
        let furnace = builtin("furnace").expect("furnace");
        assert_eq!(furnace.plan.volume_bytes, furnace.layout.runtime_buffer_bytes());
    }

    /// ADR-2618: a layout past one portable binding plans a split volume instead of failing:
    /// three 64³ bricks at 8 × 8 texels (about 246 MiB) bake as three parts of at most 128 MiB,
    /// and the plan lists each part's bytes. One volume that fits lists none.
    #[test]
    fn layouts_past_one_binding_plan_a_split() {
        let brick = |x: f32| {
            format!(
                "(min: ({x:?}, 0.0, 0.0), max: ({:?}, 63.0, 63.0), spacing: 1.0)",
                x + 63.0
            )
        };
        let ron = format!(
            "(format_version: 1, layout: (bricks: [{}, {}, {}], moment_resolution: 8, max_distance: Some(8.0)))",
            brick(0.0),
            brick(100.0),
            brick(200.0)
        );
        let job = build_v1(&ron);
        assert!(
            job.plan.volume_bytes > PORTABLE_VOLUME_BYTES,
            "{}",
            job.plan.volume_bytes
        );
        assert_eq!(job.plan.split_parts.len(), 3, "{:?}", job.plan.split_parts);
        assert!(job.plan.split_parts.iter().all(|b| *b <= PORTABLE_VOLUME_BYTES));
        let fits = build_v1(&format!(
            "(format_version: 1, layout: (bricks: [{}], moment_resolution: 8, max_distance: Some(8.0)))",
            brick(0.0)
        ));
        assert!(fits.plan.split_parts.is_empty());
    }

    /// Review baked-light #6: a volume is stale against its description when a mesh, an
    /// interior, the sky or a bake setting changed since the bake (`regia bake-gi --check`),
    /// and each change names what moved.
    #[test]
    fn a_volume_goes_stale_when_its_description_changes() {
        let job = build_v1(V1);
        let volume = encode(&job);
        assert!(
            job.stale_reasons(&volume).is_empty(),
            "{:?}",
            job.stale_reasons(&volume)
        );
        // The hab moves 1 m: the traced scene and its cluster brick change.
        let moved = build_v1(&V1.replace("-40.0, 2.0, 0.0, 1.0", "-39.0, 2.0, 0.0, 1.0"));
        assert_eq!(moved.stale_reasons(&volume), ["scene", "layout"]);
        // The room's box grows: only its brick changes (interiors are not traced).
        let room = build_v1(&V1.replace("min: (-44.0, -2.0, -4.0)", "min: (-45.0, -2.0, -4.0)"));
        assert_eq!(room.stale_reasons(&volume), ["layout"]);
        // A brighter sky: the scene and the baked light levels.
        let sky = build_v1(&V1.replace("sky: Some((1.0, 1.0, 1.0))", "sky: Some((2.0, 2.0, 2.0))"));
        assert_eq!(sky.stale_reasons(&volume), ["scene", "lights"]);
        let seed = build_v1(&V1.replace("seed: 9", "seed: 10"));
        assert_eq!(seed.stale_reasons(&volume), ["settings"]);
        // The interior flag is part of the layout: a volume baked before the flag was written
        // (every brick 0, review #1) is stale.
        let mut old_layout = job.layout.clone();
        for b in &mut old_layout.bricks {
            b.flags = 0;
        }
        let old = GiBakeJob {
            layout: old_layout,
            ..job.clone()
        };
        assert_eq!(job.stale_reasons(&encode(&old)), ["layout"]);
        // ADR-1386: a volume that does not name its emitters (format v1/v2) is stale only when
        // the scene has emitters.
        let mut unnamed = encode(&job);
        unnamed.emitters = None;
        assert!(job.stale_reasons(&unnamed).is_empty(), "no emitters in the scene");
        let lit = build_v1(&V1.replace(
            "material: Some((albedo: Some((0.2, 0.2, 0.2))))",
            "material: Some((albedo: Some((0.2, 0.2, 0.2)), emissive: Some((50.0, 40.0, 30.0))))",
        ));
        let named = encode(&lit);
        assert_eq!(named.emitters.as_ref().map(Vec::len), Some(2), "both rocks emit");
        assert!(lit.stale_reasons(&named).is_empty());
        let mut unnamed = named;
        unnamed.emitters = None;
        assert_eq!(lit.stale_reasons(&unnamed), ["emitters"]);
    }

    /// ADR-2618: the parts of a split bake are fresh against the job that baked them
    /// (`regia bake-gi --check-volume` read only part 0 and compared it with the whole layout,
    /// so every split bake was `layout` stale), and a changed job names the same reasons as for
    /// a whole volume. The cap is lowered to split a small layout.
    #[test]
    fn a_split_volume_is_fresh_against_its_job() {
        // V1 with two explicit bricks 27 m apart and no interior or auto bricks (V1's own
        // bricks overlap at the hab, so no cut separates them).
        let spread = V1
            .replace(
                r#"interiors: [(id: "room", min: (-44.0, -2.0, -4.0), max: (-36.0, 6.0, 4.0))],"#,
                "interiors: [],",
            )
            .replace(
                "bricks: [(min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), spacing: 0.5)],\n            auto_bricks: Some((spacing: 1.0, margin: 1.0, max_probes: 100000)),",
                "bricks: [(min: (0.0, 0.0, 0.0), max: (3.0, 3.0, 3.0), spacing: 1.0), (min: (30.0, 0.0, 0.0), max: (33.0, 3.0, 3.0), spacing: 1.0)],",
            );
        assert!(spread.contains("(30.0, 0.0, 0.0)") && spread.contains("interiors: []"));
        let job = build_v1(&spread);
        assert_eq!(job.layout.bricks.len(), 2);
        let volume = encode(&job);
        let cap = volume.layout().runtime_buffer_bytes() * 3 / 4;
        let parts = volume
            .split_parts(cap)
            .expect("the volume splits at 3/4 of its size");
        assert!(parts.len() >= 2, "{} parts", parts.len());
        let fresh = job.stale_reasons_of_set(&parts);
        assert!(fresh.is_empty(), "{fresh:?}");
        assert!(job.stale_reasons_of_set(std::slice::from_ref(&volume)).is_empty());
        // One part alone, or a set missing a part, is not the bake.
        assert_eq!(job.stale_reasons(&parts[0]), ["layout"]);
        assert_eq!(job.stale_reasons_of_set(&parts[..1]), ["layout"]);
        let grown = build_v1(&spread.replace("max: (33.0, 3.0, 3.0)", "max: (34.0, 3.0, 3.0)"));
        assert_eq!(grown.stale_reasons_of_set(&parts), ["layout"]);
        let seed = build_v1(&spread.replace("seed: 9", "seed: 10"));
        assert_eq!(seed.stale_reasons_of_set(&parts), ["settings"]);
        let sky = build_v1(&spread.replace("sky: Some((1.0, 1.0, 1.0))", "sky: Some((2.0, 2.0, 2.0))"));
        assert_eq!(sky.stale_reasons_of_set(&parts), ["scene", "lights"]);
    }

    /// v1 → v2 rewrites the root's version number only: comments, strings and nested
    /// `format_version` keys elsewhere stay byte for byte, and the result is idempotent.
    #[test]
    fn a_v1_file_migrates_to_v2_by_rewriting_its_number() {
        let text = "// format_version: 1 here is a comment\n(\n    /* format_version: 1 */ format_version : 1, // keep\n    sky: Some((1.0, 1.0, 1.0)),\n    layout: (bricks: [(min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), spacing: 1.0)]),\n)\n";
        let migrated = migrate_bake_ron(text).expect("migrate");
        assert_eq!(
            migrated,
            text.replacen(
                "format_version : 1,",
                &format!("format_version : {BAKE_DESC_FORMAT_VERSION},"),
                1
            )
        );
        assert_eq!(bake_desc_version(&migrated), BAKE_DESC_FORMAT_VERSION);
        assert_eq!(migrate_bake_ron(&migrated).expect("again"), migrated);
        let (a, _) = GiBakeDescription::parse_versioned(text).expect("v1");
        let (b, _) = GiBakeDescription::parse_versioned(&migrated).expect("v2");
        assert_eq!(a.layout.bricks.len(), b.layout.bricks.len());
        assert_eq!(a.sky, b.sky);
    }

    fn write_hdri(dir: &Path, name: &str) -> PathBuf {
        let map = crate::env_map::EnvMap::from_fn(128, 64, |d| {
            if Vec3::from_array(d).dot(Vec3::new(0.0, 0.8, 0.6)) > 4.0f32.to_radians().cos() {
                [5000.0; 3]
            } else if d[1] > 0.0 {
                [0.5, 0.7, 1.0]
            } else {
                [0.2; 3]
            }
        });
        let path = dir.join(name);
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, crate::hdr::encode_flat(map.width, map.height, &map.rgb)).expect("write");
        std::fs::rename(&tmp, &path).expect("rename");
        path
    }

    const V2: &str = r#"(
        format_version: 2,
        sky_hdri: Some((source: "sky.hdr", rotation: 0.5, irradiance: Some(20000.0), extract_sun: true)),
        emitters: [
            (id: Some("panel"), shape: Quad(corner: (-0.5, 3.0, -0.5), edge_u: (0.0, 0.0, 1.0), edge_v: (1.0, 0.0, 0.0)),
             power: Some(2000.0), color: (1.0, 0.9, 0.8)),
            (shape: Disc(center: (4.0, 2.0, 0.0), normal: (-1.0, 0.0, 0.0), radius: 0.25), radiance: Some((50.0, 50.0, 50.0)),
             two_sided: true),
            (shape: Sphere(center: (0.0, 1.0, 5.0), radius: 0.1, segments: 16), radiance: Some((1000.0, 800.0, 600.0))),
        ],
        layout: (bricks: [(min: (-1.0, 0.0, -1.0), max: (1.0, 2.0, 1.0), spacing: 1.0)]),
    )"#;

    /// A v2 description: the HDRI loads (sun lifted, scaled to the target irradiance), the three
    /// emitters are placed as black emissive meshes, and `plan.lights` reports all of it.
    #[test]
    fn a_v2_description_with_an_hdri_and_emitters_builds() {
        let dir = regia_test_utils::unique_temp_dir("regia_bake_desc_v2");
        write_hdri(&dir, "sky.hdr");
        let (desc, found) = GiBakeDescription::parse_versioned(V2).expect("v2");
        assert_eq!(found, 2);
        assert_eq!(desc.emitters.len(), 3);
        let job = desc.build(&dir).expect("build");
        assert!(matches!(job.scene.sky, PtSky::Equirect { .. }));
        let lights = &job.plan.lights;
        assert_eq!(lights.sky.kind, "equirect");
        assert_eq!(lights.sky.source.as_deref(), Some("sky.hdr"));
        let sun = lights.sun.as_ref().expect("the extracted sun");
        assert!(sun.from_hdri);
        assert!(lights.sky.sun_extraction.is_some());
        let up = 0.2126 * lights.sky.upward_irradiance[0]
            + 0.7152 * lights.sky.upward_irradiance[1]
            + 0.0722 * lights.sky.upward_irradiance[2]
            + sun.illuminance * sun.toward[1].max(0.0);
        assert!((up - 20_000.0).abs() < 0.02 * 20_000.0, "{up}");
        assert_eq!(lights.emitters.len(), 3);
        assert_eq!(lights.emitters[0].id, "panel");
        assert!((lights.emitters[0].power_lm - 2000.0).abs() < 0.5);
        assert_eq!(lights.emitters[1].id, "emitters[1]");
        assert!(lights.emitters[1].two_sided);
        assert_eq!(lights.emitters[2].shape, "sphere");
        assert!(!lights.emitters[2].two_sided);
        let total: f32 = lights.emitters.iter().map(|e| e.power_lm).sum();
        assert!((lights.emitter_power_lm - total).abs() < 1e-3);
        assert_eq!(lights.emissive_materials, 3);
        assert_eq!(job.scene.instances.len(), 3);
        // The plan serializes for `--dry-run`.
        let json = serde_json::to_value(&job.plan).expect("json");
        assert_eq!(json["lights"]["sky"]["kind"], "equirect");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_inputs_report_bad_keys_and_files() {
        let dir = regia_test_utils::unique_temp_dir("regia_bake_desc_v2_bad");
        write_hdri(&dir, "sky.hdr");
        let cases = [
            (
                V2.replace(
                    "format_version: 2,",
                    "format_version: 2, sky: Some((1.0, 1.0, 1.0)),",
                ),
                "sky_hdri",
            ),
            (
                V2.replace(
                    "format_version: 2,",
                    "format_version: 2, sun: Some((toward: (0.0, 1.0, 0.0), illuminance: 1.0)),",
                ),
                "sky_hdri.extract_sun",
            ),
            (
                V2.replace("rotation: 0.5,", "rotation: 0.5, scale: Some(2.0),"),
                "sky_hdri.irradiance",
            ),
            (
                V2.replace("radius: 0.25", "radius: 0.0"),
                "emitters[1].shape.radius",
            ),
            (
                V2.replace("power: Some(2000.0)", "power: None"),
                "emitters[0].radiance",
            ),
            (
                V2.replace("segments: 16", "segments: 2"),
                "emitters[2].shape.segments",
            ),
        ];
        for (text, key) in cases {
            let desc = GiBakeDescription::from_ron(&text).expect(key);
            let err = desc.build_with(&dir, &mut CookedMeshLoader).expect_err(key);
            assert_eq!(
                (err.code, err.path.as_deref()),
                ("bake_desc_invalid", Some(key)),
                "{err}"
            );
        }
        let err = GiBakeDescription::from_ron(&V2.replace("sky.hdr", "nope.hdr"))
            .expect("parse")
            .build_with(&dir, &mut CookedMeshLoader)
            .expect_err("missing");
        assert_eq!(
            (err.code, err.path.as_deref()),
            ("bake_source_missing", Some("sky_hdri.source"))
        );
        std::fs::write(dir.join("bad.hdr"), b"#?RADIANCE\n\n-Y 9 +X 9\n").expect("write");
        let err = GiBakeDescription::from_ron(&V2.replace("sky.hdr", "bad.hdr"))
            .expect("parse")
            .build_with(&dir, &mut CookedMeshLoader)
            .expect_err("bad");
        assert_eq!(err.code, "bake_hdri_decode");
        // A region larger than the allowed solid angle: not a sun.
        let err = GiBakeDescription::from_ron(&V2.replace(
            "extract_sun: true",
            "extract_sun: true, sun_max_solid_angle: 0.001",
        ))
        .expect("parse")
        .build_with(&dir, &mut CookedMeshLoader)
        .expect_err("broad");
        assert_eq!(err.code, "bake_hdri_sun");
        let err = GiBakeDescription::parse_versioned(&V2.replace("Disc(", "Disk(")).expect_err("variant");
        assert_eq!(err.code, "bake_desc_parse");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_schema_fingerprint_lists_the_v2_fields() {
        let f = bake_desc_schema_fingerprint();
        for field in [
            "sky_hdri",
            "emitters",
            "extract_sun",
            "irradiance",
            "two_sided",
            "Quad",
            "Disc",
            "Sphere",
        ] {
            assert!(f.contains(field), "{field}: {f}");
        }
    }

    /// `sky_atmosphere`: the runtime's sky-view table for the sun's direction, scaled; it needs
    /// a sun and excludes the other skies.
    #[test]
    fn an_atmosphere_sky_follows_the_sun() {
        let text = r#"(
            format_version: 2,
            sun: Some((toward: (0.3, 0.5, -0.8), illuminance: 100000.0, angular_radius: 0.00465)),
            sky_atmosphere: Some((planet: Earth, height: 2.0, scale: Some(0.5))),
            layout: (bricks: [(min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), spacing: 1.0)]),
        )"#;
        let job = GiBakeDescription::from_ron(text)
            .expect("parse")
            .build(Path::new("."))
            .expect("build");
        let PtSky::Table {
            sun_direction, scale, ..
        } = &job.scene.sky
        else {
            panic!("not a table: {:?}", job.plan.lights.sky.kind);
        };
        assert_eq!(*scale, 0.5);
        let want = Vec3::new(0.3, 0.5, -0.8).normalize();
        assert!(Vec3::from_array(*sun_direction).dot(want) > 0.999);
        let sky = &job.plan.lights.sky;
        assert_eq!(
            (sky.kind.as_str(), sky.source.as_deref()),
            ("table", Some("atmosphere:earth"))
        );
        // A daylight sky: a bright blue upward irradiance.
        assert!(
            sky.upward_irradiance[2] > sky.upward_irradiance[0],
            "{:?}",
            sky.upward_irradiance
        );
        assert!(sky.upward_irradiance[1] > 1000.0, "{:?}", sky.upward_irradiance);
        for (from, to, key) in [
            (
                "sun: Some((toward: (0.3, 0.5, -0.8), illuminance: 100000.0, angular_radius: 0.00465)),",
                "",
                "sky_atmosphere",
            ),
            (
                "format_version: 2,",
                "format_version: 2, sky: Some((1.0, 1.0, 1.0)),",
                "sky_atmosphere",
            ),
            ("height: 2.0", "height: 0.0", "sky_atmosphere.height"),
        ] {
            let err = GiBakeDescription::from_ron(&text.replace(from, to))
                .expect(key)
                .build_with(Path::new("."), &mut CookedMeshLoader)
                .expect_err(key);
            assert_eq!(
                (err.code, err.path.as_deref()),
                ("bake_desc_invalid", Some(key)),
                "{err}"
            );
        }
    }

    /// Built-in scenes report their lights too: the furnace's uniform sky gives π·L upward.
    #[test]
    fn builtin_plans_report_their_lights() {
        let furnace = builtin("furnace").expect("furnace");
        let sky = &furnace.plan.lights.sky;
        assert_eq!(sky.kind, "uniform");
        assert!((sky.upward_irradiance[0] - std::f32::consts::PI).abs() < 0.01);
        assert!((sky.mean_luminance - 1.0).abs() < 0.01);
        let hills = builtin("rolling-hills").expect("hills");
        assert!((hills.plan.lights.sun.as_ref().expect("sun").illuminance - 100_000.0).abs() < 1.0);
        let cornell = builtin("cornell").expect("cornell");
        assert!(cornell.plan.lights.emissive_materials >= 1);
    }

    /// A v2 body with `{sun}` for the root `sun` field (a layout, no meshes).
    fn v2_with_sun(sun: &str) -> String {
        format!(
            "// a v2 description\n(\n    format_version: 2,\n    // the sun: Some((..)) in a comment stays\n    \
             {sun}\n    sky: Some((1.0, 1.0, 1.0)),\n    sky_hdri: None,\n    \
             layout: (bricks: [(min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), spacing: 0.5)]),\n)\n"
        )
    }

    /// v2 → v3 in memory (ADR-7818): a v0-v2 `sun: Some(s)` loads as `suns: [s]`, `sun: None` and
    /// no `sun` as no suns; a v3 file lists its suns in order.
    #[test]
    fn a_v2_sun_loads_as_a_one_element_suns_list() {
        let sun = "sun: Some((toward: (0.3, 0.8, 0.2), illuminance: 1000.0, color: (1.0, 0.9, 0.8))),";
        let (desc, found) = GiBakeDescription::parse_versioned(&v2_with_sun(sun)).expect("v2");
        assert_eq!((found, desc.format_version), (2, BAKE_DESC_FORMAT_VERSION));
        assert_eq!(desc.suns.len(), 1);
        assert_eq!(desc.suns[0].toward, [0.3, 0.8, 0.2]);
        assert_eq!(desc.suns[0].color, [1.0, 0.9, 0.8]);
        assert_eq!(desc.suns[0].angular_radius, 0.0);
        assert!(desc.legacy_sun.is_none(), "moved, not copied");
        for none in ["sun: None,", ""] {
            let (desc, _) = GiBakeDescription::parse_versioned(&v2_with_sun(none)).expect("v2");
            assert!(desc.suns.is_empty(), "{none:?}");
        }
        let (v0, found) = GiBakeDescription::parse_versioned(V0).expect("v0");
        assert_eq!((found, v0.suns.len()), (0, 1));
        let v3 = "(format_version: 3, suns: [(toward: (0.0, 1.0, 0.0), illuminance: 1.0), \
                  (toward: (1.0, 1.0, 0.0), illuminance: 2.0, angular_radius: 0.01)], layout: ())";
        let (desc, found) = GiBakeDescription::parse_versioned(v3).expect("v3");
        assert_eq!(found, 3);
        assert_eq!(
            desc.suns.iter().map(|s| s.illuminance).collect::<Vec<_>>(),
            [1.0, 2.0]
        );
    }

    /// v2 → v3 on disk: the root `sun` becomes `suns` with the sun's own bytes inside the brackets
    /// (`Some(s)` → `[s]`, `None` → `[]`, implicit-some `s` → `[s]`); comments, a `sun` in a comment
    /// or a string, nested `extract_sun` keys and every other byte stay. The result reads back as
    /// the in-memory migration and migrating again changes nothing.
    #[test]
    fn a_v2_sun_migrates_on_disk_to_a_suns_list() {
        let current = format!("format_version: {BAKE_DESC_FORMAT_VERSION},");
        for (sun, want) in [
            (
                "sun: Some((toward: (0.3, 0.8, 0.2), /* lux */ illuminance: 1000.0)), // noon",
                "suns: [(toward: (0.3, 0.8, 0.2), /* lux */ illuminance: 1000.0)], // noon",
            ),
            (
                "sun : Some ( (toward: (0.0, 1.0, 0.0), illuminance: 5.0) ) ,",
                "suns : [ (toward: (0.0, 1.0, 0.0), illuminance: 5.0) ] ,",
            ),
            (
                "sun: Some((\n        toward: (0.0, 1.0, 0.0),\n        illuminance: 5.0,\n    )),",
                "suns: [(\n        toward: (0.0, 1.0, 0.0),\n        illuminance: 5.0,\n    )],",
            ),
            ("sun: None,", "suns: [],"),
            ("", ""),
        ] {
            let text = v2_with_sun(sun);
            let migrated = migrate_bake_ron(&text).expect(sun);
            let expected = v2_with_sun(want).replacen("format_version: 2,", &current, 1);
            assert_eq!(migrated, expected, "{sun:?}");
            assert_eq!(
                migrate_bake_ron(&migrated).expect("again"),
                migrated,
                "idempotent"
            );
            let (a, _) = GiBakeDescription::parse_versioned(&text).expect("v2");
            let (b, found) = GiBakeDescription::parse_versioned(&migrated).expect("v3");
            assert_eq!(found, BAKE_DESC_FORMAT_VERSION);
            assert_eq!(format!("{a:?}"), format!("{b:?}"), "reads back the same: {sun:?}");
        }
        // A last field without a trailing comma, a `sun` inside a string and a nested key.
        let text = "(format_version: 2, root: Some(\"sun: Some((x))\"), layout: (), \
                    sun: Some((toward: (0.0, 1.0, 0.0), illuminance: 5.0)))";
        assert_eq!(
            migrate_bake_ron(text).expect("migrate"),
            format!(
                "(format_version: {BAKE_DESC_FORMAT_VERSION}, root: Some(\"sun: Some((x))\"), layout: (), \
                 suns: [(toward: (0.0, 1.0, 0.0), illuminance: 5.0)])"
            )
        );
        // ron's implicit_some extension: the bare struct becomes the list's one element.
        let implicit = "#![enable(implicit_some)]\n(format_version: 2, sun: (toward: (0.0, 1.0, 0.0), \
                        illuminance: 5.0), layout: ())";
        let migrated = migrate_bake_ron(implicit).expect("implicit some");
        assert!(
            migrated.contains("suns: [(toward: (0.0, 1.0, 0.0), illuminance: 5.0)]"),
            "{migrated}"
        );
        let (desc, _) = GiBakeDescription::parse_versioned(&migrated).expect("parses");
        assert_eq!(desc.suns.len(), 1);
        // The attribute does not hide the version: a v3 file with one keeps its `suns`.
        assert_eq!(bake_desc_version(implicit), 2);
        assert_eq!(bake_desc_version(&migrated), BAKE_DESC_FORMAT_VERSION);
        // v0 gets the stamp and the list.
        let migrated = migrate_bake_ron(V0).expect("v0");
        assert!(
            migrated.contains("suns: [(toward: (0.0, 1.0, 0.0), illuminance: 1000.0)],"),
            "{migrated}"
        );
    }

    /// The sun field belongs to its version: a v3 file with `sun` and a v2 file with `suns` are
    /// refused naming the key; more suns than the bakes take are refused by `validate`.
    #[test]
    fn suns_and_sun_are_refused_in_the_wrong_version_and_past_the_limit() {
        let err = GiBakeDescription::parse_versioned(
            "(format_version: 3, sun: Some((toward: (0.0, 1.0, 0.0), illuminance: 1.0)), layout: ())",
        )
        .expect_err("v3 sun");
        assert_eq!(
            (err.code, err.path.as_deref()),
            ("bake_desc_invalid", Some("sun"))
        );
        let err = GiBakeDescription::parse_versioned(
            "(format_version: 2, suns: [(toward: (0.0, 1.0, 0.0), illuminance: 1.0)], layout: ())",
        )
        .expect_err("v2 suns");
        assert_eq!(
            (err.code, err.path.as_deref()),
            ("bake_desc_invalid", Some("suns"))
        );
        let sun = "(toward: (0.0, 1.0, 0.0), illuminance: 1.0)";
        let with = |n: usize| {
            let list = vec![sun; n].join(", ");
            GiBakeDescription::parse_versioned(&format!(
                "(format_version: 3, suns: [{list}], layout: (bricks: [(min: (0.0, 0.0, 0.0), \
                 max: (1.0, 1.0, 1.0), spacing: 1.0)]))"
            ))
            .expect("parses")
            .0
        };
        assert_eq!(MAX_BAKE_SUNS, 4);
        with(MAX_BAKE_SUNS).validate().expect("the limit is fine");
        let err = with(MAX_BAKE_SUNS + 1).validate().expect_err("one past");
        assert_eq!(
            (err.code, err.path.as_deref()),
            ("bake_desc_invalid", Some("suns")),
            "{err}"
        );
    }

    /// Several suns build into the scene in order: the first is the sun (the atmosphere sky follows
    /// it), the rest are extra suns, and the plan reports them; `extract_sun` still needs no sun.
    #[test]
    fn several_suns_build_the_sun_and_extra_suns() {
        let text = "(format_version: 3, suns: [(toward: (0.3, 0.5, -0.8), illuminance: 100000.0), \
                    (toward: (-0.5, 0.7, 0.4), illuminance: 300.0, color: (0.4, 0.6, 1.0))], \
                    sky_atmosphere: Some((planet: Earth, height: 2.0)), layout: (bricks: [(min: (0.0, 0.0, 0.0), \
                    max: (1.0, 1.0, 1.0), spacing: 1.0)]))";
        let job = GiBakeDescription::from_ron(text)
            .expect("parses")
            .build_with(Path::new("."), &mut CookedMeshLoader)
            .expect("builds");
        let sun = job.scene.sun.expect("sun");
        assert_eq!(sun.illuminance, 100000.0);
        assert_eq!(job.scene.extra_suns.len(), 1);
        assert_eq!(job.scene.extra_suns[0].color, [0.4, 0.6, 1.0]);
        let PtSky::Table { sun_direction, .. } = &job.scene.sky else {
            panic!("an atmosphere table");
        };
        let want = Vec3::new(0.3, 0.5, -0.8).normalize();
        assert!(Vec3::from_array(*sun_direction).dot(want) > 0.999);
        assert_eq!(job.plan.lights.extra_suns.len(), 1);
        assert_eq!(job.plan.lights.extra_suns[0].illuminance, 300.0);
        let one = GiBakeDescription::from_ron(&text.replace(
            ", (toward: (-0.5, 0.7, 0.4), illuminance: 300.0, color: (0.4, 0.6, 1.0))",
            "",
        ))
        .expect("parses")
        .build_with(Path::new("."), &mut CookedMeshLoader)
        .expect("builds");
        assert!(one.scene.extra_suns.is_empty());
        assert_ne!(
            one.scene.content_hash(),
            job.scene.content_hash(),
            "the extra sun is in the hash"
        );
        let json = serde_json::to_value(&one.plan.lights).expect("json");
        assert!(
            json.get("extra_suns").is_none(),
            "one sun: the plan's JSON is unchanged"
        );
    }
}
