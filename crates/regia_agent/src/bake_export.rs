// SPDX-License-Identifier: MPL-2.0
//! The level → `.bake.ron` exporter (ADR-310, `book/agents/baked-gi-export.md`), shared by
//! `regia bake-gi --from-scene` (a level spawned headless) and the agent's `level.export_bake`
//! and `level.get_baked_gi` (the running game's world).
//!
//! What it exports:
//! - every `GiMobility::Static` entity with a `MeshSource` as a `.bake.ron` v1 mesh entry: its
//!   mesh path and world transform. The one tagged `terrain` becomes `terrain_mesh` instead.
//!   `Dynamic` entities with a mesh, `Static` ones without a mesh and built-in primitive meshes
//!   are skipped and counted.
//! - the sun, the sky and the sky bodies from the level's `LevelSettings::atmosphere`
//!   ([`level_lights`]).
//! - a layout: the terrain layer when there is a terrain, a coarse brick over the statics'
//!   bounds (plus [`BOUNDS_MARGIN_M`]) when there is none, and automatic detail bricks.
//! - optionally a DO27 bake bundle (`static_meshes.json`, `interiors.json`, `MANIFEST.sha256`),
//!   named in the description and read by the bake itself.
//!
//! The text is deterministic: entities are sorted by guid and written through serde with the
//! canonical RON settings, so an unchanged level exports byte-identical text. Staleness uses a
//! hash of the inputs ([`Export::input_hash`]), not of the text, kept in a sidecar
//! `<name>.bake.stamp.json` ([`Stamp`]); `.bake.ron` v1 has no field for it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use regia_app::prelude::*;
use regia_core::math::{Mat4, Vec3};
use regia_render::{
    AtmosphereParameters, DirectionalLight, MeshSource, TimeOfDay, ambient_irradiance_table,
    build_atmosphere_luts_for, sun_position, transmitted_sun,
};
use regia_scene::bake_query::{GiEntity, gi_entities};
use regia_scene::components::GiMobility;
use regia_scene::level::{AtmospherePreset, LevelSettings};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Version tag of the export rules. Its version is hashed into every input hash (through
/// [`INPUT_HASH_TAG`]): a change of the rules makes every stamp stale.
pub const EXPORT_FORMAT: &str = "regia-bake-export/1";
/// The export rules' tag as it was before ADR-8670 renamed it, and the bytes still hashed into
/// every input hash ([`INPUT_HASH_TAG`]).
pub const LEGACY_EXPORT_FORMAT: &str = "ferrite-bake-export/1"; // rename-keep: hashed bytes (ADR-8670)
/// The tag hashed first into every input hash. It stays [`LEGACY_EXPORT_FORMAT`] after the
/// ADR-8670 rename so stamps written before it stay current (a new name alone would make every
/// project re-export and re-bake); bump the version of both when the rules change.
pub const INPUT_HASH_TAG: &str = LEGACY_EXPORT_FORMAT;
/// Format string of the stamp sidecar this build writes.
pub const STAMP_FORMAT: &str = "regia-bake-stamp/1";
/// Format string of stamp sidecars written before ADR-8670; [`read_stamp`] still accepts it.
pub const LEGACY_STAMP_FORMAT: &str = "ferrite-bake-stamp/1"; // rename-keep: read by read_stamp (ADR-8670)
/// The `.bake.ron` format version the exporter writes (`regia_pt::bake_desc`).
pub const BAKE_RON_VERSION: u32 = 1;
/// Margin around the statics' bounds of the volume brick written for a level without terrain,
/// metres.
pub const BOUNDS_MARGIN_M: f32 = 4.0;
/// Finest probe spacing of that volume brick, metres; it coarsens so no axis holds more than
/// [`VOLUME_MAX_PROBES_PER_AXIS`] probes.
pub const VOLUME_SPACING_M: f32 = 4.0;
/// Most probes along one axis of the volume brick.
pub const VOLUME_MAX_PROBES_PER_AXIS: f32 = 64.0;
/// Terrain layer column spacing written for a level with terrain, metres.
pub const TERRAIN_COLUMN_SPACING_M: f32 = 8.0;
/// Spacing of the automatic detail bricks, metres.
pub const AUTO_BRICK_SPACING_M: f32 = 1.0;
/// Margin of the automatic detail bricks, metres.
pub const AUTO_BRICK_MARGIN_M: f32 = 2.0;
/// Probe budget of the automatic detail bricks.
pub const AUTO_BRICK_MAX_PROBES: u32 = 1 << 18;
/// The tag that makes a static mesh the terrain.
pub const TERRAIN_TAG: &str = "terrain";
/// Layer of a static mesh without a `scatter` tag.
pub const DEFAULT_LAYER: &str = "setpiece";
/// Sun angular radius at 1 AU (radians); scaled by the square root of the illuminance ratio
/// for other distances.
const SUN_RADIUS_1AU: f32 = 0.004_65;
/// Top-of-atmosphere sun illuminance at 1 AU, lux (the Earth preset's).
const SUN_LUX_1AU: f32 = 128_000.0;

/// An export error: a snake_case code, the entity it concerns when there is one, and a message.
/// The codes are listed in `book/agents/baked-gi-export.md`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExportError {
    /// Error code (lowercase ascii, digits and `_`).
    pub code: &'static str,
    /// The entity (`name (guid)`) or file it concerns.
    pub entity: Option<String>,
    /// What is wrong.
    pub message: String,
}

impl ExportError {
    /// An error with `code` about `entity`.
    #[must_use]
    pub fn new(code: &'static str, entity: Option<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            entity,
            message: message.into(),
        }
    }
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.entity {
            Some(e) => write!(f, "{e}: {} [{}]", self.message, self.code),
            None => write!(f, "{} [{}]", self.message, self.code),
        }
    }
}

impl std::error::Error for ExportError {}

/// A static mesh entity to export.
#[derive(Clone, Debug, PartialEq)]
pub struct StaticMesh {
    /// The entity's guid (the mesh entry's `id`).
    pub guid: String,
    /// `name (guid)`, for errors and reports.
    pub label: String,
    /// The mesh path, project-relative with `/` separators.
    pub source: String,
    /// The `MeshSource` material path, if any (hashed; the bake reads the mesh's own materials).
    pub material: Option<String>,
    /// World transform, column-major.
    pub transform: [f32; 16],
    /// Layer tag (`setpiece` or `scatter`).
    pub layer: String,
}

/// The authored directional light of a level (used when the level has no sun clock).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AuthoredSun {
    /// Direction the light travels.
    pub direction: [f32; 3],
    /// Linear color.
    pub color: [f32; 3],
    /// Illuminance, lux.
    pub illuminance: f32,
}

/// What [`collect_statics`] found in a world.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SceneStatics {
    /// Static meshes, sorted by guid.
    pub meshes: Vec<StaticMesh>,
    /// The static mesh tagged `terrain`.
    pub terrain: Option<StaticMesh>,
    /// Entities not exported, `(label, reason)`: `dynamic` (a mesh on a `Dynamic` entity),
    /// `no_mesh` (a `Static` entity without a mesh), `builtin_mesh` (a built-in primitive).
    pub skipped: Vec<(String, &'static str)>,
    /// The first drawn directional light by guid (a light hidden itself or under a hidden group
    /// is skipped, as the live frame skips it).
    pub sun: Option<AuthoredSun>,
}

fn normalize_path(p: &str) -> String {
    let s = p.replace('\\', "/");
    let s = match s.strip_prefix("assets://") {
        Some(rest) => format!("assets/{rest}"),
        None => s.clone(),
    };
    s.trim_start_matches("./").to_string()
}

/// Collects the export's entities from `world` (headless or live). The world is not changed.
///
/// # Errors
///
/// `export_terrain_ambiguous` when two static meshes are tagged `terrain`.
pub fn collect_statics(world: &mut World) -> Result<SceneStatics, ExportError> {
    let entities: Vec<GiEntity> = gi_entities(world);
    let mut out = SceneStatics::default();
    for e in &entities {
        if out.sun.is_none()
            && let Some(light) = world.get::<DirectionalLight>(e.entity)
            && regia_render::visibility::is_drawn(world.get(e.entity))
        {
            out.sun = Some(AuthoredSun {
                direction: light.direction,
                color: light.color,
                illuminance: light.illuminance,
            });
        }
        let mesh = world
            .get::<MeshSource>(e.entity)
            .filter(|m| !m.mesh.trim().is_empty());
        let label = e.label();
        match (e.mobility, mesh) {
            (GiMobility::Dynamic, Some(_)) => out.skipped.push((label, "dynamic")),
            (GiMobility::Dynamic, None) => {}
            (GiMobility::Static, None) => out.skipped.push((label, "no_mesh")),
            (GiMobility::Static, Some(m)) if m.is_builtin() => out.skipped.push((label, "builtin_mesh")),
            (GiMobility::Static, Some(m)) => {
                let entry = StaticMesh {
                    guid: e.guid.to_string(),
                    label: label.clone(),
                    source: normalize_path(&m.mesh),
                    material: Some(normalize_path(&m.material)).filter(|s| !s.is_empty()),
                    transform: e.world,
                    layer: (if e.has_tag("scatter") {
                        "scatter"
                    } else {
                        DEFAULT_LAYER
                    })
                    .to_string(),
                };
                if e.has_tag(TERRAIN_TAG) {
                    if let Some(first) = &out.terrain {
                        return Err(ExportError::new(
                            "export_terrain_ambiguous",
                            Some(label),
                            format!(
                                "a second static mesh is tagged `terrain` (the first is {})",
                                first.label
                            ),
                        ));
                    }
                    out.terrain = Some(entry);
                } else {
                    out.meshes.push(entry);
                }
            }
        }
    }
    Ok(out)
}

/// The sun of the exported description.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct SunOut {
    /// Unit vector toward the sun.
    pub toward: [f32; 3],
    /// Illuminance at normal incidence at the ground, lux.
    pub illuminance: f32,
    /// Linear color.
    pub color: [f32; 3],
    /// Angular radius, radians.
    pub angular_radius: f32,
}

/// A sky body of the exported description.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct SkyBodyOut {
    /// Unit vector toward the body.
    pub direction: [f32; 3],
    /// Angular radius, radians.
    pub angular_radius: f32,
    /// Uniform radiance, cd/m².
    pub radiance: [f32; 3],
    /// Lighting multiplier.
    pub light_scale: f32,
}

/// The lights [`level_lights`] derived from a level.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BakeLights {
    /// The sun (none at night or without a sun).
    pub sun: Option<SunOut>,
    /// Uniform sky radiance standing in for the atmosphere (none when airless).
    pub sky: Option<[f32; 3]>,
    /// Sky bodies (planet-shine).
    pub sky_bodies: Vec<SkyBodyOut>,
    /// Where the sun came from: `time_of_day`, `directional_light` or `none`.
    pub sun_source: &'static str,
}

/// `v` rounded to 5 significant digits: the light values come from transcendental functions
/// whose last bits may differ between platforms' math libraries; rounding keeps the exported
/// text identical across machines.
fn round5(v: f32) -> f32 {
    if v == 0.0 || !v.is_finite() {
        return v;
    }
    let digits = 4 - v.abs().log10().floor() as i32;
    let scale = 10f64.powi(digits);
    ((f64::from(v) * scale).round() / scale) as f32
}

fn round3(v: [f32; 3]) -> [f32; 3] {
    v.map(round5)
}

fn unit(v: [f32; 3]) -> Option<[f32; 3]> {
    let v = Vec3::from_array(v);
    (v.length() > 1.0e-6).then(|| v.normalize().to_array())
}

/// A sun given on the command line (`--sun el,az,lux`): it replaces the level's clock and
/// authored light. Same convention as [`sun_position`]: azimuth clockwise from north, +X east,
/// +Z north, +Y up.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SunOverride {
    /// Degrees above the horizon.
    pub elevation_deg: f32,
    /// Degrees clockwise from north (+Z).
    pub azimuth_deg: f32,
    /// Top-of-atmosphere illuminance, lux.
    pub lux: f32,
}

impl SunOverride {
    /// Unit vector toward the sun.
    #[must_use]
    pub fn toward(&self) -> [f32; 3] {
        let (el, az) = (self.elevation_deg.to_radians(), self.azimuth_deg.to_radians());
        [el.cos() * az.sin(), el.sin(), el.cos() * az.cos()]
    }
}

/// The sun, sky and sky bodies of a level, the way the renderer lights it:
/// - **Sun direction**: `sun_override` when given, else the level's sun clock
///   (`atmosphere.time_of_day`, [`sun_position`]), else the authored directional light, else
///   none. A sun below the horizon is dropped.
/// - **Sun strength**: on a planet with air, the illuminance and color the atmosphere
///   transmits to the ground ([`transmitted_sun`]); airless, the top-of-atmosphere value
///   (the override's `lux`, else `sun_illuminance_lux` or the preset's). An authored light
///   without a clock keeps its own color and illuminance. The angular radius follows the
///   distance implied by the illuminance (`0.00465 · sqrt(E / 128 klx)`).
/// - **Sky**: airless, none; otherwise a uniform radiance equal to the atmosphere's
///   cosine-weighted sky radiance ([`ambient_irradiance_table`]), so the irradiance on an
///   up-facing surface matches. No sun, no sky.
/// - **Sky bodies**: `sky_body` and `moons`, each a uniform disc whose radiance times its solid
///   angle is its planet-shine fill (`SkyBody::fill_illuminance`), tinted by its albedo.
///
/// Values are rounded to 5 significant digits.
#[must_use]
pub fn level_lights(
    settings: &LevelSettings,
    authored: Option<&AuthoredSun>,
    sun_override: Option<&SunOverride>,
) -> BakeLights {
    let atmosphere = &settings.atmosphere;
    let mut params = AtmosphereParameters::from_preset(atmosphere.preset);
    if atmosphere.preset == AtmospherePreset::Earth {
        params.set_aerosol_turbidity(atmosphere.aerosol_turbidity);
    }
    if let Some(lux) = atmosphere.sun_illuminance_lux
        && lux > 0.0
    {
        params.sun_illuminance = lux;
    }
    if let Some(o) = sun_override
        && o.lux > 0.0
    {
        params.sun_illuminance = o.lux;
    }
    let (toward, source) = match (sun_override, atmosphere.time_of_day, authored) {
        (Some(o), _, _) => (unit(o.toward()), "override"),
        (None, Some(clock), _) => (
            Some(
                sun_position(&TimeOfDay {
                    hour: clock.hour,
                    day_of_year: clock.day_of_year,
                    latitude_deg: clock.latitude_deg,
                })
                .direction,
            ),
            "time_of_day",
        ),
        (None, None, Some(light)) => (unit(light.direction.map(|c| -c)), "directional_light"),
        (None, None, None) => (None, "none"),
    };
    let mut lights = BakeLights {
        sun_source: source,
        ..BakeLights::default()
    };
    let Some(toward) = toward else {
        return lights;
    };
    let top_lux = match (source, authored) {
        ("directional_light", Some(light)) => light.illuminance,
        _ => params.sun_illuminance,
    };
    let airless = params.is_airless();
    let (color, lux) = if airless {
        ([1.0; 3], top_lux)
    } else {
        let luts = build_atmosphere_luts_for(&params, toward, 2.0);
        lights.sky = Some(round3(ambient_irradiance_table(&luts)));
        transmitted_sun(&luts, &params)
    };
    let (color, lux) = match (source, authored) {
        ("directional_light", Some(light)) => (light.color, light.illuminance),
        _ => (color, lux),
    };
    if toward[1] > 0.0 && lux > 0.0 {
        lights.sun = Some(SunOut {
            toward: round3(toward),
            illuminance: round5(lux),
            color: round3(color),
            angular_radius: round5(SUN_RADIUS_1AU * (top_lux.max(1.0) / SUN_LUX_1AU).sqrt()),
        });
    }
    for body in atmosphere.sky_body.iter().chain(&atmosphere.moons) {
        let Some(direction) = unit(body.direction) else {
            continue;
        };
        let fill = body.fill_illuminance(top_lux, toward);
        let radius = (body.angular_diameter_deg.clamp(0.0, 180.0) * 0.5).to_radians();
        let solid_angle = std::f32::consts::TAU * (1.0 - radius.cos());
        let t = body.tint;
        let luminance = 0.2126 * t[0] + 0.7152 * t[1] + 0.0722 * t[2];
        if fill <= 0.0 || solid_angle <= 0.0 || luminance <= 0.0 {
            continue;
        }
        let scale = fill / solid_angle / luminance;
        lights.sky_bodies.push(SkyBodyOut {
            direction: round3(direction),
            angular_radius: round5(radius),
            radiance: round3(t.map(|c| c * scale)),
            light_scale: 1.0,
        });
    }
    lights
}

/// A DO27 bake bundle named by the description (`regia bake-gi --from-static-meshes`).
/// The bake reads and checks it (`regia_pt::bake_meshes`); the exporter writes the names and
/// hashes the files.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleRef {
    /// Bundle root, absolute, `/` separators (the description's `root`).
    pub root: String,
    /// `static_meshes.json`, relative to the root.
    pub static_meshes: String,
    /// `interiors.json`, relative to the root.
    #[serde(default)]
    pub interiors_file: Option<String>,
    /// `MANIFEST.sha256`, relative to the root.
    #[serde(default)]
    pub manifest: Option<String>,
    /// The terrain grid glTF, relative to the root (the description's `terrain_mesh`).
    #[serde(default)]
    pub terrain_source: Option<String>,
}

/// How to export (kept in the stamp, so the agent re-derives the same hash).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportOptions {
    /// Terrain mesh rasterization spacing, metres.
    pub terrain_spacing: f32,
    /// Volume bounds overriding the statics' bounds `(min, max)`, world metres.
    #[serde(default)]
    pub bounds: Option<([f32; 3], [f32; 3])>,
    /// The DO27 bundle.
    #[serde(default)]
    pub bundle: Option<BundleRef>,
    /// The terrain hole mask (`<map>.holes.ron`), project-relative.
    #[serde(default)]
    pub terrain_holes: Option<String>,
    /// A sun replacing the level's (a DO27 map's sun lives in its `map_look.json`).
    #[serde(default)]
    pub sun: Option<SunOverride>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            terrain_spacing: 1.0,
            bounds: None,
            bundle: None,
            terrain_holes: None,
            sun: None,
        }
    }
}

/// Object-space bounds and triangle count of a mesh file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeshInfo {
    /// Minimum corner.
    pub min: [f32; 3],
    /// Maximum corner.
    pub max: [f32; 3],
    /// Triangles.
    pub triangles: u64,
}

/// Loads mesh bounds for the export (the CLI loads the files through the bake's loader; the
/// agent passes none and the summary's bounds stay unknown).
pub trait MeshInfoSource {
    /// Bounds of the mesh file at `path` (absolute). `Ok(None)` when unknown.
    ///
    /// # Errors
    ///
    /// The file does not load, as a message.
    fn info(&mut self, path: &Path) -> Result<Option<MeshInfo>, String>;
}

/// A [`MeshInfoSource`] that knows nothing (no bounds, no triangle counts).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoMeshInfo;

impl MeshInfoSource for NoMeshInfo {
    fn info(&mut self, _path: &Path) -> Result<Option<MeshInfo>, String> {
        Ok(None)
    }
}

/// What an export produced (before anything is written).
#[derive(Clone, Debug, PartialEq)]
pub struct Export {
    /// The `.bake.ron` text.
    pub text: String,
    /// blake3 (hex) of the inputs: export rules, options, entities, transforms, mesh and bundle
    /// file contents and the level's atmosphere and authored sun.
    pub input_hash: String,
    /// The summary (`statics`, `skipped`, `triangles`, `bounds`, ...).
    pub summary: Value,
}

#[derive(Serialize)]
struct TerrainMeshOut {
    source: String,
    spacing: f32,
    transform: [f32; 16],
    holes: Option<String>,
}

#[derive(Serialize)]
struct MeshOut {
    id: Option<String>,
    source: String,
    transform: [f32; 16],
    layer: Option<String>,
}

#[derive(Serialize)]
struct BrickOut {
    min: [f32; 3],
    max: [f32; 3],
    spacing: f32,
}

#[derive(Serialize)]
struct AutoBricksOut {
    spacing: f32,
    margin: f32,
    max_probes: u32,
    layers: Vec<String>,
    interiors: bool,
}

#[derive(Serialize)]
struct LayoutOut {
    terrain_spacing: Option<[f32; 2]>,
    levels: Vec<f32>,
    bricks: Vec<BrickOut>,
    auto_bricks: Option<AutoBricksOut>,
    moment_resolution: u32,
    max_distance: Option<f32>,
}

#[derive(Serialize)]
struct DescOut {
    format_version: u32,
    root: Option<String>,
    manifest: Option<String>,
    terrain_mesh: Option<TerrainMeshOut>,
    static_meshes: Option<String>,
    interiors_file: Option<String>,
    meshes: Vec<MeshOut>,
    sun: Option<SunOut>,
    sky: Option<[f32; 3]>,
    sky_bodies: Vec<SkyBodyOut>,
    layout: LayoutOut,
}

/// The header comment of every exported description.
pub const DESC_HEADER: &str = "// Generated from a level by the scene exporter (ADR-310). Regenerate it with\n\
// `regia bake-gi --from-scene <level>` or the agent's `level.export_bake`; edits are overwritten.\n";

/// `path` as a `/`-separated string.
#[must_use]
pub fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// `to` relative to the directory `from` (both absolute or both relative to the same base),
/// `..` steps included; `None` when they share no prefix (another drive).
#[must_use]
pub fn relative_path(from: &Path, to: &Path) -> Option<PathBuf> {
    let from: Vec<Component> = from
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    let to: Vec<Component> = to
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    if let (Some(Component::Prefix(a)), Some(Component::Prefix(b))) = (from.first(), to.first())
        && a != b
    {
        return None;
    }
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to[common..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    Some(out)
}

fn blake_file(hasher: &mut blake3::Hasher, path: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(blake3::hash(&bytes).as_bytes());
    Ok(())
}

fn blake_str(hasher: &mut blake3::Hasher, s: &str) {
    hasher.update(&(s.len() as u64).to_le_bytes());
    hasher.update(s.as_bytes());
}

/// The external buffers a `.gltf` reads (`buffers[].uri` that are not data URIs), resolved next
/// to it. The bake samples no textures, so images are not inputs.
fn gltf_buffers(path: &Path) -> Vec<PathBuf> {
    let is_gltf = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("gltf"));
    if !is_gltf {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let dir = path.parent().unwrap_or(Path::new(""));
    json["buffers"]
        .as_array()
        .map(|buffers| {
            buffers
                .iter()
                .filter_map(|b| b["uri"].as_str())
                .filter(|uri| !uri.starts_with("data:"))
                .map(|uri| dir.join(uri))
                .collect()
        })
        .unwrap_or_default()
}

/// A file the level names, under the project root. These strings are content, not a request: one
/// that left the project made this export a read and hash oracle, and `regia_pt` read it again.
/// A missing file passes, for the read to report it.
fn project_file(project: &Path, rel: &str, label: &str) -> Result<PathBuf, ExportError> {
    regia_core::project_path::resolve_write(project, rel).map_err(|e| {
        ExportError::new(
            "export_path_invalid",
            Some(label.to_string()),
            format!("`{rel}`: {e}"),
        )
    })
}

fn hash_mesh_file(
    hasher: &mut blake3::Hasher,
    project: &Path,
    path: &Path,
    label: &str,
) -> Result<(), ExportError> {
    let missing = |e: std::io::Error| {
        ExportError::new(
            "export_mesh_missing",
            Some(label.to_string()),
            format!("{}: {e}", path.display()),
        )
    };
    blake_file(hasher, path).map_err(missing)?;
    for buffer in gltf_buffers(path) {
        regia_core::project_path::check_under_root(project, &buffer).map_err(|e| {
            ExportError::new(
                "export_path_invalid",
                Some(label.to_string()),
                format!("{}: {e}", buffer.display()),
            )
        })?;
        blake_file(hasher, &buffer).map_err(missing)?;
    }
    Ok(())
}

fn hash_entry(hasher: &mut blake3::Hasher, m: &StaticMesh, project: &Path) -> Result<(), ExportError> {
    blake_str(hasher, &m.guid);
    blake_str(hasher, &m.source);
    blake_str(hasher, m.material.as_deref().unwrap_or(""));
    blake_str(hasher, &m.layer);
    for v in m.transform {
        hasher.update(&v.to_bits().to_le_bytes());
    }
    hash_mesh_file(
        hasher,
        project,
        &project_file(project, &m.source, &m.label)?,
        &m.label,
    )?;
    if let Some(material) = &m.material {
        // A missing material file is not an error (the bake reads the mesh's own materials).
        let _ = blake_file(hasher, &project_file(project, material, &m.label)?);
    }
    Ok(())
}

fn hash_bundle(hasher: &mut blake3::Hasher, bundle: &BundleRef) -> Result<(), ExportError> {
    let root = Path::new(&bundle.root);
    let read = |rel: &str| -> Result<PathBuf, ExportError> {
        // The root is the CLI's, and lives outside the project on purpose; the names under it are
        // not, so they stay under it.
        regia_core::project_path::check_relative(rel).map_err(|e| {
            ExportError::new(
                "export_path_invalid",
                Some(rel.to_string()),
                format!("`{rel}`: {e}"),
            )
        })?;
        let path = root.join(rel);
        if path.is_file() {
            Ok(path)
        } else {
            Err(ExportError::new(
                "export_read_failed",
                Some(rel.to_string()),
                format!("{}: no such file", path.display()),
            ))
        }
    };
    let failed = |path: &Path, e: std::io::Error| {
        ExportError::new("export_read_failed", Some(slash(path)), e.to_string())
    };
    blake_str(hasher, &bundle.root);
    let static_meshes = read(&bundle.static_meshes)?;
    blake_file(hasher, &static_meshes).map_err(|e| failed(&static_meshes, e))?;
    for rel in [&bundle.interiors_file, &bundle.terrain_source]
        .into_iter()
        .flatten()
    {
        let path = read(rel)?;
        blake_file(hasher, &path).map_err(|e| failed(&path, e))?;
    }
    if let Some(manifest) = &bundle.manifest {
        // The manifest lists every file's hash: hashing it covers the whole bundle.
        let path = read(manifest)?;
        blake_file(hasher, &path).map_err(|e| failed(&path, e))?;
        return Ok(());
    }
    // No manifest: hash the sources the bake would load.
    let text = std::fs::read_to_string(&static_meshes).map_err(|e| failed(&static_meshes, e))?;
    let json: Value = serde_json::from_str(&text).map_err(|e| {
        ExportError::new(
            "export_read_failed",
            Some(bundle.static_meshes.clone()),
            e.to_string(),
        )
    })?;
    let mut sources: Vec<&str> = json["meshes"]
        .as_array()
        .map(|m| m.iter().filter_map(|e| e["source"].as_str()).collect())
        .unwrap_or_default();
    sources.sort_unstable();
    sources.dedup();
    for source in sources {
        regia_core::project_path::check_relative(source).map_err(|e| {
            ExportError::new(
                "export_path_invalid",
                Some(source.to_string()),
                format!("`{source}`: {e}"),
            )
        })?;
        let path = root.join(source);
        // A bundle's glTF buffers stay in the bundle, as the project's stay in the project.
        hash_mesh_file(hasher, root, &path, source)?;
    }
    Ok(())
}

fn union(a: Option<([f32; 3], [f32; 3])>, b: ([f32; 3], [f32; 3])) -> ([f32; 3], [f32; 3]) {
    match a {
        None => b,
        Some((min, max)) => (
            std::array::from_fn(|k| min[k].min(b.0[k])),
            std::array::from_fn(|k| max[k].max(b.1[k])),
        ),
    }
}

fn world_bounds(m: &[f32; 16], info: &MeshInfo) -> ([f32; 3], [f32; 3]) {
    let m = Mat4::from_cols_array(m);
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for corner in 0..8 {
        let c = Vec3::new(
            if corner & 1 == 0 { info.min[0] } else { info.max[0] },
            if corner & 2 == 0 { info.min[1] } else { info.max[1] },
            if corner & 4 == 0 { info.min[2] } else { info.max[2] },
        );
        let w = m.transform_point3(c).to_array();
        min = std::array::from_fn(|k| min[k].min(w[k]));
        max = std::array::from_fn(|k| max[k].max(w[k]));
    }
    (min, max)
}

/// The volume brick over `bounds` grown by [`BOUNDS_MARGIN_M`] (none for an override, which is
/// used as given), at [`VOLUME_SPACING_M`] or coarser.
fn volume_brick(bounds: ([f32; 3], [f32; 3]), margin: f32) -> BrickOut {
    let min = bounds.0.map(|v| v - margin);
    let max = bounds.1.map(|v| v + margin);
    let extent = (0..3).map(|k| max[k] - min[k]).fold(0.0f32, f32::max);
    let spacing = (extent / (VOLUME_MAX_PROBES_PER_AXIS - 1.0)).max(VOLUME_SPACING_M);
    BrickOut {
        min: round3(min),
        max: round3(max),
        spacing: round5(spacing),
    }
}

/// Exports `statics` and `lights` as `.bake.ron` text for a description at `desc_path`, mesh
/// paths relative to `project`. Mesh files are read for the input hash; `mesh_info` gives their
/// bounds and triangle counts.
///
/// # Errors
///
/// `export_mesh_missing` (a static mesh's file cannot be read; names the entity),
/// `export_terrain_ambiguous` (a scene terrain and a bundle terrain), `export_read_failed` (a
/// bundle file), `export_mesh_load` (a mesh does not load for its bounds), `export_no_input`
/// (nothing to bake), `export_write_failed` (RON serialization).
pub fn export(
    statics: &SceneStatics,
    settings: &LevelSettings,
    project: &Path,
    desc_path: &Path,
    options: &ExportOptions,
    mesh_info: &mut dyn MeshInfoSource,
) -> Result<Export, ExportError> {
    let _span = tracing::info_span!("bake_export", meshes = statics.meshes.len()).entered();
    let mut hasher = blake3::Hasher::new();
    blake_str(&mut hasher, INPUT_HASH_TAG);
    let options_json = serde_json::to_string(options).unwrap_or_default();
    blake_str(&mut hasher, &options_json);
    blake_str(
        &mut hasher,
        &serde_json::to_string(&settings.atmosphere).unwrap_or_default(),
    );
    if let Some(sun) = &statics.sun {
        for v in sun.direction.iter().chain(&sun.color).chain([&sun.illuminance]) {
            hasher.update(&v.to_bits().to_le_bytes());
        }
    }

    if statics.meshes.is_empty() && statics.terrain.is_none() && options.bundle.is_none() {
        return Err(ExportError::new(
            "export_no_input",
            None,
            "the level has no GiMobility::Static entity with a mesh, and no bundle was given",
        ));
    }
    if statics.terrain.is_some()
        && options
            .bundle
            .as_ref()
            .is_some_and(|b| b.terrain_source.is_some())
    {
        return Err(ExportError::new(
            "export_terrain_ambiguous",
            statics.terrain.as_ref().map(|t| t.label.clone()),
            "the level has a terrain-tagged static mesh and the bundle names a terrain",
        ));
    }

    // Where `root` points and how scene mesh paths are written.
    let desc_dir = desc_path.parent().unwrap_or(Path::new(""));
    let bundle = options.bundle.as_ref();
    let root = match bundle {
        Some(b) => b.root.clone(),
        None => relative_path(desc_dir, project).map_or_else(|| slash(project), |p| slash(&p)),
    };
    let scene_path = |source: &str| -> String {
        match bundle {
            // The root is the bundle: scene meshes are written absolute (outside the bundle, so
            // the manifest does not check them).
            Some(_) => slash(&project.join(source)),
            None => source.to_string(),
        }
    };

    let mut bounds: Option<([f32; 3], [f32; 3])> = None;
    let mut triangles: u64 = 0;
    let mut triangles_known = true;
    let mut meshes = Vec::with_capacity(statics.meshes.len());
    let mut info_cache: BTreeMap<String, Option<MeshInfo>> = BTreeMap::new();
    for m in &statics.meshes {
        hash_entry(&mut hasher, m, project)?;
        let info = match info_cache.get(&m.source) {
            Some(info) => *info,
            None => {
                let info = mesh_info
                    .info(&project.join(&m.source))
                    .map_err(|e| ExportError::new("export_mesh_load", Some(m.label.clone()), e))?;
                info_cache.insert(m.source.clone(), info);
                info
            }
        };
        match info {
            Some(info) => {
                triangles += info.triangles;
                bounds = Some(union(bounds, world_bounds(&m.transform, &info)));
            }
            None => triangles_known = false,
        }
        meshes.push(MeshOut {
            id: Some(m.guid.clone()),
            source: scene_path(&m.source),
            transform: m.transform,
            layer: Some(m.layer.clone()),
        });
    }
    let mut terrain_mesh = None;
    if let Some(t) = &statics.terrain {
        hash_entry(&mut hasher, t, project)?;
        let holes = match &options.terrain_holes {
            Some(h) => {
                let path = project_file(project, h, "terrain_holes")?;
                let _ = blake_file(&mut hasher, &path);
                Some(match bundle {
                    Some(_) => slash(&path),
                    None => h.clone(),
                })
            }
            None => None,
        };
        terrain_mesh = Some(TerrainMeshOut {
            source: scene_path(&t.source),
            spacing: options.terrain_spacing,
            transform: t.transform,
            holes,
        });
    }
    if let Some(b) = bundle {
        hash_bundle(&mut hasher, b)?;
        if let Some(source) = &b.terrain_source {
            terrain_mesh = Some(TerrainMeshOut {
                source: source.clone(),
                spacing: options.terrain_spacing,
                transform: Mat4::IDENTITY.to_cols_array(),
                holes: None,
            });
        }
    }

    let lights = level_lights(settings, statics.sun.as_ref(), options.sun.as_ref());
    let has_terrain = terrain_mesh.is_some();
    let mut bricks = Vec::new();
    match options.bounds {
        Some(b) => bricks.push(volume_brick(b, 0.0)),
        None if !has_terrain => {
            if let Some(b) = bounds {
                bricks.push(volume_brick(b, BOUNDS_MARGIN_M));
            }
        }
        None => {}
    }
    let auto_bricks = (!statics.meshes.is_empty() || bundle.is_some()).then(|| AutoBricksOut {
        spacing: AUTO_BRICK_SPACING_M,
        margin: AUTO_BRICK_MARGIN_M,
        max_probes: AUTO_BRICK_MAX_PROBES,
        layers: vec![DEFAULT_LAYER.to_string()],
        interiors: true,
    });
    let desc = DescOut {
        format_version: BAKE_RON_VERSION,
        root: Some(root),
        manifest: bundle.and_then(|b| b.manifest.clone()),
        terrain_mesh,
        static_meshes: bundle.map(|b| b.static_meshes.clone()),
        interiors_file: bundle.and_then(|b| b.interiors_file.clone()),
        meshes,
        sun: lights.sun,
        sky: lights.sky,
        sky_bodies: lights.sky_bodies.clone(),
        layout: LayoutOut {
            terrain_spacing: has_terrain.then_some([TERRAIN_COLUMN_SPACING_M; 2]),
            levels: vec![1.0, 2.0, 4.0, 8.0, 16.0],
            bricks,
            auto_bricks,
            moment_resolution: 4,
            max_distance: None,
        },
    };
    let body = ron::ser::to_string_pretty(&desc, regia_ecs::serialize::canonical_pretty_config())
        .map_err(|e| ExportError::new("export_write_failed", None, e.to_string()))?;
    let text = format!("{DESC_HEADER}{body}\n");

    let mut skipped_by: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, why) in &statics.skipped {
        *skipped_by.entry(why).or_default() += 1;
    }
    let input_hash = hasher.finalize().to_hex().to_string();
    let summary = json!({
        "statics": statics.meshes.len(),
        "skipped": statics.skipped.len(),
        "skipped_by": skipped_by,
        "skipped_entities": statics.skipped.iter().map(|(e, why)| json!({"entity": e, "reason": why})).collect::<Vec<_>>(),
        "terrain": statics.terrain.as_ref().map(|t| t.source.clone())
            .or_else(|| bundle.and_then(|b| b.terrain_source.clone())),
        "triangles": triangles_known.then_some(triangles),
        "bounds": bounds.map(|(min, max)| json!({"min": min, "max": max})),
        "bounds_override": options.bounds.is_some(),
        "sun_source": lights.sun_source,
        "sun": lights.sun.is_some(),
        "sky": lights.sky.is_some(),
        "sky_bodies": lights.sky_bodies.len(),
        "bundle": bundle.map(|b| b.root.clone()),
        "hash": input_hash,
    });
    Ok(Export {
        text,
        input_hash,
        summary,
    })
}

/// The stamp sidecar of an exported description (`<name>.bake.stamp.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stamp {
    /// [`STAMP_FORMAT`] (or [`LEGACY_STAMP_FORMAT`] in a stamp written before ADR-8670).
    pub format: String,
    /// [`Export::input_hash`] of the export that wrote the description.
    pub input_hash: String,
    /// blake3 (hex) of the description text as written (a hand edit shows as `desc_changed`).
    pub desc_hash: String,
    /// The options of that export.
    pub options: ExportOptions,
    /// The volume baked from it, if `--bake` ran: `(path, input hash it was baked from)`.
    #[serde(default)]
    pub volume: Option<(String, String)>,
}

/// The stamp path of the description at `desc`: `x.bake.ron` → `x.bake.stamp.json`.
#[must_use]
pub fn stamp_path(desc: &Path) -> PathBuf {
    let name = desc
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("volume.bake.ron");
    let stem = name.strip_suffix(".ron").unwrap_or(name);
    let stem = stem.strip_suffix(".bake").unwrap_or(stem);
    desc.with_file_name(format!("{stem}.bake.stamp.json"))
}

/// The default description path of a level whose volume (or, without one, whose file) has the
/// stem `stem`: `<project>/bake/<stem>.bake.ron`, outside `assets/` so it never ships.
#[must_use]
pub fn default_desc_path(project: &Path, stem: &str) -> PathBuf {
    project.join("bake").join(format!("{stem}.bake.ron"))
}

/// The name of `path` up to its first dot (`maps/bore_twelve.fgiv` → `bore_twelve`).
#[must_use]
pub fn file_stem_to_first_dot(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.split('.').next().unwrap_or(name).to_string()
}

/// Reads the stamp at `path` (`None` when absent or unreadable). Accepts [`STAMP_FORMAT`] and
/// the [`LEGACY_STAMP_FORMAT`] of stamps written before ADR-8670.
#[must_use]
pub fn read_stamp(path: &Path) -> Option<Stamp> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Stamp>(&text)
        .ok()
        .filter(|s| s.format == STAMP_FORMAT || s.format == LEGACY_STAMP_FORMAT)
}

/// Whether the description at `desc` is current for `export`, and why not: `desc_missing`,
/// `stamp_missing`, `inputs_changed` or `desc_changed` (edited since it was written).
#[must_use]
pub fn staleness(export: &Export, desc: &Path) -> Option<&'static str> {
    let Ok(text) = std::fs::read(desc) else {
        return Some("desc_missing");
    };
    let Some(stamp) = read_stamp(&stamp_path(desc)) else {
        return Some("stamp_missing");
    };
    if stamp.input_hash != export.input_hash {
        return Some("inputs_changed");
    }
    if stamp.desc_hash != blake3::hash(&text).to_hex().as_str() {
        return Some("desc_changed");
    }
    None
}

/// Writes `bytes` to `path` through a temporary file in the same directory and a rename.
///
/// # Errors
///
/// The directory cannot be created or the file written.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    replace_file(path, bytes)
}

/// Replaces `path` through a temporary file in the same directory and a rename. Unlike
/// [`write_atomic`] it creates no directory: a missing one is an error.
pub(crate) fn replace_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    // ADR-10066: a symlink planted at the temp name is unlinked, never written through.
    crate::req_path::write_new_temp(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Writes the description and its stamp unless both are already current. Returns whether the
/// description was stale before (and so was written). A volume entry of an existing stamp is
/// kept.
///
/// # Errors
///
/// `export_write_failed`.
pub fn write_export(export: &Export, desc: &Path, options: &ExportOptions) -> Result<bool, ExportError> {
    // Input-current but different text (an agent export without mesh bounds, say) is rewritten too.
    let same_text = std::fs::read(desc).ok().as_deref() == Some(export.text.as_bytes());
    let stale = staleness(export, desc).is_some() || !same_text;
    if !stale {
        return Ok(false);
    }
    let failed =
        |p: &Path, e: std::io::Error| ExportError::new("export_write_failed", Some(slash(p)), e.to_string());
    write_atomic(desc, export.text.as_bytes()).map_err(|e| failed(desc, e))?;
    let stamp_file = stamp_path(desc);
    let volume = read_stamp(&stamp_file).and_then(|s| s.volume);
    let stamp = Stamp {
        format: STAMP_FORMAT.to_string(),
        input_hash: export.input_hash.clone(),
        desc_hash: blake3::hash(export.text.as_bytes()).to_hex().to_string(),
        options: options.clone(),
        volume,
    };
    let json = serde_json::to_string_pretty(&stamp)
        .map_err(|e| ExportError::new("export_write_failed", None, e.to_string()))?;
    write_atomic(&stamp_file, format!("{json}\n").as_bytes()).map_err(|e| failed(&stamp_file, e))?;
    Ok(true)
}

/// Records in the stamp next to `desc` that `volume` was baked from `input_hash`.
///
/// # Errors
///
/// `export_write_failed` (also when there is no stamp).
pub fn record_volume(desc: &Path, volume: &str, input_hash: &str) -> Result<(), ExportError> {
    let path = stamp_path(desc);
    let mut stamp = read_stamp(&path).ok_or_else(|| {
        ExportError::new(
            "export_write_failed",
            Some(slash(&path)),
            "no stamp to record the volume in",
        )
    })?;
    stamp.volume = Some((volume.to_string(), input_hash.to_string()));
    let json = serde_json::to_string_pretty(&stamp)
        .map_err(|e| ExportError::new("export_write_failed", None, e.to_string()))?;
    write_atomic(&path, format!("{json}\n").as_bytes())
        .map_err(|e| ExportError::new("export_write_failed", Some(slash(&path)), e.to_string()))
}

// ---------------------------------------------------------------------------
// Agent methods (book/agents/baked-gi-export.md)
// ---------------------------------------------------------------------------

fn agent_error(e: &ExportError) -> RpcError {
    let (code, regia) = match e.code {
        "export_mesh_missing" | "export_no_level" => (protocol::NOT_FOUND, "F0003"),
        "export_path_invalid" | "export_terrain_ambiguous" | "export_no_input" => {
            (protocol::INVALID_PARAMS, "F0002")
        }
        _ => (protocol::INTERNAL_ERROR, "F0001"),
    };
    RpcError {
        code,
        message: e.to_string(),
        data: Some(json!({
            "code": regia,
            "reason": e.code,
            "entity": e.entity,
            "hint": "see book/agents/baked-gi-export.md",
        })),
    }
}

/// The open level's settings and the description path the agent uses: `path` when given
/// (project-relative, no `..`, ending in `.bake.ron`), else the default for the level's volume.
fn agent_target(world: &World, path: Option<&str>) -> Result<(LevelSettings, PathBuf, PathBuf), ExportError> {
    let settings = world
        .get_resource::<LevelSettings>()
        .cloned()
        .ok_or_else(|| ExportError::new("export_no_level", None, "no level is open (no LevelSettings)"))?;
    // ADR-8798: the shared root (asset server, then `AgentSettings::project_dir`, then the working
    // directory only when neither is set).
    let project = crate::req_path::project_root(world);
    let desc = match path {
        Some(p) => {
            let clean = regia_core::project_path::check_relative(p);
            let Some(rel) = clean.ok().filter(|_| p.ends_with(".bake.ron")) else {
                return Err(ExportError::new(
                    "export_path_invalid",
                    Some(p.to_string()),
                    "`path` must be project-relative, without `..`, and end in `.bake.ron`",
                ));
            };
            project.join(rel)
        }
        None => {
            let volume = settings
                .look
                .baked_gi
                .as_ref()
                .map(|b| b.volume.clone())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    ExportError::new(
                        "export_path_invalid",
                        None,
                        "the level names no baked_gi volume: give `path`",
                    )
                })?;
            default_desc_path(&project, &file_stem_to_first_dot(&volume))
        }
    };
    // A symlink must not lead the write out of the project. ADR-8798: the shared check sees a
    // dangling symlink as existing and refuses it, and refuses when the project root cannot be
    // resolved (the old walk used `Path::exists`, which skipped a dangling link, and passed when
    // either side failed to canonicalize).
    if regia_core::project_path::check_under_root(&project, &desc).is_err() {
        return Err(ExportError::new(
            "export_path_invalid",
            path.map(str::to_string),
            "the path leads out of the project",
        ));
    }
    Ok((settings, project, desc))
}

/// The input hash of the open level against the stamp at the level's default description:
/// `{ desc, hash, stale, reason }`, or `None` when the level names no volume. Used by
/// `level.get_baked_gi` (`bake_input_hash`, `bake_input_stale`).
#[must_use]
pub fn bake_input_status(world: &mut World) -> Option<Value> {
    let (settings, project, desc) = agent_target(world, None).ok()?;
    let options = read_stamp(&stamp_path(&desc))
        .map(|s| s.options)
        .unwrap_or_default();
    let result = collect_statics(world)
        .and_then(|statics| export(&statics, &settings, &project, &desc, &options, &mut NoMeshInfo));
    Some(match result {
        Ok(export) => {
            let reason = staleness(&export, &desc);
            json!({
                "desc": slash(desc.strip_prefix(&project).unwrap_or(&desc)),
                "hash": export.input_hash,
                "stale": reason.is_some(),
                "reason": reason,
            })
        }
        Err(e) => json!({
            "desc": slash(desc.strip_prefix(&project).unwrap_or(&desc)),
            "hash": Value::Null,
            "stale": true,
            "reason": e.code,
            "error": e.to_string(),
        }),
    })
}

/// `level.export_bake { path? }`: exports the open level to `path` (default
/// `bake/<volume stem>.bake.ron`) with its stamp. The options of an existing stamp at that path
/// are kept (terrain spacing, bounds, a DO27 bundle the CLI named).
pub(crate) fn export_bake(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = crate::params::opt_str(params, "path")?;
    let (settings, project, desc) = agent_target(world, path).map_err(|e| agent_error(&e))?;
    let options = read_stamp(&stamp_path(&desc))
        .map(|s| s.options)
        .unwrap_or_default();
    let statics = collect_statics(world).map_err(|e| agent_error(&e))?;
    let export = export(&statics, &settings, &project, &desc, &options, &mut NoMeshInfo)
        .map_err(|e| agent_error(&e))?;
    let written = write_export(&export, &desc, &options).map_err(|e| agent_error(&e))?;
    let mut summary = export.summary;
    summary["ok"] = json!(true);
    summary["desc"] = json!(slash(desc.strip_prefix(&project).unwrap_or(&desc)));
    summary["stale"] = json!(written);
    summary["checkpoint"] = volume_checkpoint(&settings, &project);
    Ok(summary)
}

/// The checkpoint a `regia bake-gi --from-scene <level> --bake --checkpoint` of the level's volume
/// would continue (ADR-3095): `{exists, resumable, path, units_done, remaining, eta_seconds, ...}`
/// of `.<volume>.bakeck` beside `assets/<volume>`, `null` when the level names no volume.
fn volume_checkpoint(settings: &LevelSettings, project: &Path) -> Value {
    settings
        .look
        .baked_gi
        .as_ref()
        .filter(|b| !b.volume.is_empty())
        .map_or(Value::Null, |b| {
            let volume = project.join("assets").join(&b.volume);
            json!(regia_asset::bake_checkpoint::checkpoint_status(&volume))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::id::Guid;
    use regia_core::math::Affine3A;
    use regia_scene::components::EntityGuid;
    use regia_scene::level::{AtmospherePreset as ScenePreset, LevelBakedGi, LevelTimeOfDay, SkyBody};
    use regia_scene::transform::GlobalTransform;

    /// A running game's world: a static box, a dynamic box, an airless noon sun and a level
    /// naming `maps/yard.fgiv`, under an asset server rooted at a temp project.
    fn live_world(root: &Path) -> World {
        let mut world = World::new();
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server");
        world.insert_resource(server);
        world.spawn((
            EntityGuid(Guid::from_u128(1)),
            GiMobility::Static,
            MeshSource::new("assets/meshes/box.gltf", ""),
            GlobalTransform(Affine3A::from_translation(Vec3::new(2.0, 0.5, 0.0))),
        ));
        world.spawn((
            EntityGuid(Guid::from_u128(2)),
            GiMobility::Dynamic,
            MeshSource::new("assets/meshes/box.gltf", ""),
        ));
        world
    }

    fn level(world: &mut World) {
        let mut settings = LevelSettings::default();
        settings.atmosphere.preset = ScenePreset::Airless;
        settings.atmosphere.sun_illuminance_lux = Some(4_400.0);
        settings.atmosphere.time_of_day = Some(LevelTimeOfDay::default());
        settings.look.baked_gi = Some(LevelBakedGi {
            volume: "maps/yard.fgiv".into(),
            ..LevelBakedGi::default()
        });
        world.insert_resource(settings);
    }

    fn reason(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .map(str::to_string)
    }

    /// ADR-8798: with no asset server the export resolves against `AgentSettings::project_dir`, not
    /// the working directory, and a dangling symlink at the description path is refused (the old walk
    /// used `Path::exists`, which is false for a dangling link, and skipped the check).
    #[test]
    fn agent_target_uses_the_agent_project_and_refuses_a_dangling_link() {
        let base = regia_test_utils::unique_temp_dir("bake-export-target");
        let (project, outside) = (base.join("project"), base.join("outside"));
        std::fs::create_dir_all(project.join("bake")).expect("project");
        std::fs::create_dir_all(&outside).expect("outside");
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(project.clone()),
        });
        level(&mut world);
        let (_, root, desc) = agent_target(&world, None).expect("default target");
        assert_eq!(root, project);
        assert_eq!(desc, project.join("bake/yard.bake.ron"));
        let (_, _, desc) = agent_target(&world, Some("bake/other.bake.ron")).expect("given path");
        assert_eq!(desc, project.join("bake/other.bake.ron"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("gone.bake.ron"), project.join("bake/yard.bake.ron"))
                .expect("dangling link");
            let error = agent_target(&world, None).expect_err("a dangling link");
            assert_eq!(error.code, "export_path_invalid");
            std::os::unix::fs::symlink(&outside, project.join("out")).expect("dir link");
            let error = agent_target(&world, Some("out/x.bake.ron")).expect_err("a link out");
            assert_eq!(error.code, "export_path_invalid");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// ADR-3206: `replace_file` writes `.{name}.{pid}.tmp` with `std::fs::write`, which follows
    /// a symlink. A link planted at that name (a cloned project can carry one) receives the
    /// bake outside the project. `audio.rs` unlinks the temp and opens it with `create_new`.
    #[cfg(unix)]
    #[test]
    fn replace_file_does_not_follow_a_planted_temp_symlink() {
        let base = regia_test_utils::unique_temp_dir("bake-temp-link");
        let project = base.join("project");
        let outside = base.join("outside");
        std::fs::create_dir_all(&project).expect("project");
        std::fs::create_dir_all(&outside).expect("outside");
        let desc = project.join("yard.bake.ron");
        let victim = outside.join("pwned.bake.ron");
        let tmp = desc.with_file_name(format!(".yard.bake.ron.{}.tmp", std::process::id()));
        std::os::unix::fs::symlink(&victim, &tmp).expect("temp link");
        let result = replace_file(&desc, b"(version: 1)\n");
        assert!(
            !victim.exists(),
            "replace_file followed the temp symlink ({result:?})"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Pathhelper audit M2: mesh, material and bundle names are content. A level that names a file
    /// outside the project got it hashed and written into the description for `regia_pt` to read.
    #[test]
    fn export_refuses_content_paths_that_leave_the_project() {
        let base = regia_test_utils::unique_temp_dir("agent_export_confine");
        let root = base.join("project");
        std::fs::create_dir_all(root.join("assets/meshes")).expect("dir");
        regia_test_utils::write_atomic(&root.join("assets/meshes/box.gltf"), b"box").expect("mesh");
        regia_test_utils::write_atomic(&base.join("secret.gltf"), b"secret").expect("secret");
        let hash = |rel: &str, material: Option<&str>| {
            let m = StaticMesh {
                guid: "g".to_string(),
                label: "bad".to_string(),
                source: rel.to_string(),
                material: material.map(str::to_string),
                layer: "default".to_string(),
                transform: Default::default(),
            };
            hash_entry(&mut blake3::Hasher::new(), &m, &root).map_err(|e| e.code)
        };
        assert!(hash("assets/meshes/box.gltf", None).is_ok());
        let escape = "../secret.gltf".to_string();
        let absolute = base.join("secret.gltf").display().to_string();
        for bad in [&escape, &absolute] {
            assert_eq!(hash(bad, None), Err("export_path_invalid"), "{bad}");
            assert_eq!(
                hash("assets/meshes/box.gltf", Some(bad)),
                Err("export_path_invalid"),
                "material {bad}"
            );
        }
        // A missing file inside the project stays `export_mesh_missing`.
        assert_eq!(hash("assets/gone.gltf", None), Err("export_mesh_missing"));
        // A glTF's buffer is read through the same bound.
        let gltf = root.join("assets/meshes/leak.gltf");
        regia_test_utils::write_atomic(&gltf, br#"{"buffers":[{"uri":"../../../secret.gltf"}]}"#)
            .expect("gltf");
        assert_eq!(
            hash("assets/meshes/leak.gltf", None),
            Err("export_path_invalid"),
            "buffer uri"
        );
        // A bundle's root is outside the project on purpose; its names stay under that root.
        let bundle = BundleRef {
            root: root.display().to_string(),
            static_meshes: "../secret.gltf".to_string(),
            ..BundleRef::default()
        };
        let err = hash_bundle(&mut blake3::Hasher::new(), &bundle).expect_err("escape");
        assert_eq!(err.code, "export_path_invalid");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `level.export_bake` writes the open level's description and stamp under the project root;
    /// `level.get_baked_gi` then reports it current, and stale once a static entity moves.
    #[test]
    fn export_bake_writes_the_description_and_get_baked_gi_reports_staleness() {
        let root = regia_test_utils::unique_temp_dir("agent_export_bake");
        std::fs::create_dir_all(root.join("assets/meshes")).expect("dir");
        regia_test_utils::write_atomic(&root.join("assets/meshes/box.gltf"), b"box").expect("mesh");
        let mut world = live_world(&root);
        let err = export_bake(&mut world, &json!({})).expect_err("no level");
        assert_eq!(reason(&err).as_deref(), Some("export_no_level"));
        assert!(bake_input_status(&mut world).is_none());
        level(&mut world);

        let out = export_bake(&mut world, &json!({})).expect("export");
        // No checkpoint of the level's volume yet (ADR-3095).
        assert_eq!(out["checkpoint"]["exists"], false, "{out}");
        let volume = root.join("assets/maps/yard.fgiv");
        regia_asset::bake_checkpoint::BakeCheckpoint {
            header: regia_asset::bake_checkpoint::BakeCheckpointHeader {
                kind: regia_asset::bake_checkpoint::BakeCheckpointKind::GiGpu,
                stage: 1,
                input_hash: [0; 32],
                units_total: 10,
                units_done: 4,
                batches_done: 2,
                work_seconds: 2.0,
            },
            payload: Vec::new(),
        }
        .write_atomic(&regia_asset::bake_checkpoint::default_checkpoint_path(&volume))
        .expect("checkpoint");
        let with = export_bake(&mut world, &json!({})).expect("export");
        assert_eq!(with["checkpoint"]["resumable"], true, "{with}");
        assert_eq!(with["checkpoint"]["remaining"], 6);
        assert_eq!(with["checkpoint"]["eta_seconds"], 3.0);
        let _ = std::fs::remove_file(regia_asset::bake_checkpoint::default_checkpoint_path(&volume));
        assert_eq!(out["ok"], true);
        assert_eq!(out["desc"], "bake/yard.bake.ron");
        assert_eq!(out["statics"], 1, "{out}");
        assert_eq!(out["skipped"], 1);
        assert!(out["bounds"].is_null() && out["triangles"].is_null());
        assert!(root.join("bake/yard.bake.ron").is_file());
        assert!(root.join("bake/yard.bake.stamp.json").is_file());

        let status = crate::level_baked_gi::get_baked_gi(&mut world).expect("status");
        assert_eq!(status["bake_input_stale"], false, "{status}");
        assert_eq!(status["bake_input_hash"], out["hash"]);

        let mut q = world.query::<(&EntityGuid, &mut GlobalTransform)>();
        for (guid, mut global) in q.iter_mut(&mut world) {
            if guid.0 == Guid::from_u128(1) {
                global.0 = Affine3A::from_translation(Vec3::new(3.0, 0.5, 0.0));
            }
        }
        let status = crate::level_baked_gi::get_baked_gi(&mut world).expect("status");
        assert_eq!(status["bake_input_stale"], true);
        assert_eq!(status["bake_input"]["reason"], "inputs_changed");
        let again = export_bake(&mut world, &json!({ "path": "bake/other.bake.ron" })).expect("export");
        assert_eq!(again["desc"], "bake/other.bake.ron");

        // `NUL`/`C:` are legal literal names on Linux but reserved on Windows: refused on
        // every host via regia_core::project_path::check_relative.
        for bad in [
            "../yard.bake.ron",
            "/tmp/yard.bake.ron",
            "bake/yard.ron",
            "bake/NUL.bake.ron",
            "C:yard.bake.ron",
        ] {
            let err = export_bake(&mut world, &json!({ "path": bad })).expect_err(bad);
            assert_eq!(reason(&err).as_deref(), Some("export_path_invalid"), "{bad}");
        }
        std::fs::remove_file(root.join("assets/meshes/box.gltf")).expect("remove");
        let err = export_bake(&mut world, &json!({})).expect_err("mesh gone");
        assert_eq!(reason(&err).as_deref(), Some("export_mesh_missing"));
        assert!(
            err.message.contains(&Guid::from_u128(1).to_string()),
            "{}",
            err.message
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-8670: a stamp written before the rename (legacy format string) still reads and still
    /// reports its description current; a new write uses the current format string; the input
    /// hash keeps the legacy tag bytes, so old stamps do not all go stale.
    #[test]
    fn stamps_read_both_format_names_and_write_the_current_one() {
        assert_eq!(STAMP_FORMAT, "regia-bake-stamp/1");
        assert_eq!(EXPORT_FORMAT, "regia-bake-export/1");
        let legacy = regia_core::brand::LEGACY_KEY_PREFIX;
        assert_eq!(LEGACY_STAMP_FORMAT, format!("{legacy}-bake-stamp/1"));
        assert_eq!(INPUT_HASH_TAG, format!("{legacy}-bake-export/1"));
        assert!(regia_core::brand::key_matches(LEGACY_STAMP_FORMAT, STAMP_FORMAT));

        let dir = regia_test_utils::unique_temp_dir("agent_stamp_names");
        let desc = dir.join("yard.bake.ron");
        let export = Export {
            text: "(version: 1)\n".to_string(),
            input_hash: "abc123".to_string(),
            summary: json!({}),
        };
        regia_test_utils::write_atomic(&desc, export.text.as_bytes()).expect("desc");
        let write_stamp = |format: &str| {
            let stamp = Stamp {
                format: format.to_string(),
                input_hash: export.input_hash.clone(),
                desc_hash: blake3::hash(export.text.as_bytes()).to_hex().to_string(),
                options: ExportOptions::default(),
                volume: Some(("maps/yard.fgiv".into(), "abc123".into())),
            };
            let json = serde_json::to_string_pretty(&stamp).expect("json");
            regia_test_utils::write_atomic(&stamp_path(&desc), json.as_bytes()).expect("stamp");
        };
        for (format, readable) in [
            (LEGACY_STAMP_FORMAT, true),
            (STAMP_FORMAT, true),
            ("other-bake-stamp/1", false),
        ] {
            write_stamp(format);
            let read = read_stamp(&stamp_path(&desc));
            assert_eq!(read.is_some(), readable, "{format}");
            let want = if readable { None } else { Some("stamp_missing") };
            assert_eq!(staleness(&export, &desc), want, "{format}");
        }

        // A changed export rewrites the stamp in the current format and keeps the volume entry
        // of the old (legacy) stamp.
        write_stamp(LEGACY_STAMP_FORMAT);
        let changed = Export {
            input_hash: "def456".to_string(),
            ..export.clone()
        };
        assert!(write_export(&changed, &desc, &ExportOptions::default()).expect("write"));
        let text = std::fs::read_to_string(stamp_path(&desc)).expect("stamp");
        assert!(text.contains("\"regia-bake-stamp/1\""), "{text}");
        assert!(!text.contains(LEGACY_STAMP_FORMAT), "{text}");
        let read = read_stamp(&stamp_path(&desc)).expect("current stamp");
        assert_eq!(read.volume, Some(("maps/yard.fgiv".into(), "abc123".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relative_paths_and_stamp_names() {
        assert_eq!(
            relative_path(Path::new("/p/bake"), Path::new("/p")),
            Some(PathBuf::from(".."))
        );
        assert_eq!(
            relative_path(Path::new("/p/bake"), Path::new("/p/assets/x.gltf")),
            Some(PathBuf::from("../assets/x.gltf"))
        );
        assert_eq!(
            relative_path(Path::new("/p"), Path::new("/p")),
            Some(PathBuf::from("."))
        );
        assert_eq!(
            stamp_path(Path::new("bake/main.bake.ron")),
            PathBuf::from("bake/main.bake.stamp.json")
        );
        assert_eq!(file_stem_to_first_dot("maps/bore_twelve.fgiv"), "bore_twelve");
        assert_eq!(file_stem_to_first_dot("levels/main.level.ron"), "main");
        assert_eq!(
            normalize_path("assets://props\\box.gltf"),
            "assets/props/box.gltf"
        );
    }

    /// Review finding 5 (w6-post05b): the export took the first directional light by guid whatever
    /// its visibility, so a sun the live frame drops still lit the bake. The lower-guid sun sits
    /// under a hidden group and has no visibility component of its own; the scene's real
    /// propagation resolves it.
    #[test]
    fn collect_statics_skips_a_sun_under_a_hidden_group() {
        use regia_scene::components::Visibility;
        let sun = |lux: f32| DirectionalLight {
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            illuminance: lux,
            cast_shadows: true,
            static_shadow: false,
        };
        let mut world = World::new();
        let group = world
            .spawn((
                EntityGuid(Guid::from_u128(1)),
                regia_scene::Transform::default(),
                Visibility::Hidden,
            ))
            .id();
        world
            .spawn((
                EntityGuid(Guid::from_u128(2)),
                regia_scene::Transform::default(),
                sun(99_000.0),
            ))
            .insert(ChildOf(group));
        regia_scene::propagate::propagate_transforms(&mut world);
        let statics = collect_statics(&mut world).expect("collect");
        assert!(statics.sun.is_none(), "the hidden sun does not light the bake");

        world.spawn((EntityGuid(Guid::from_u128(3)), sun(5_000.0)));
        let statics = collect_statics(&mut world).expect("collect");
        assert_eq!(statics.sun.map(|s| s.illuminance), Some(5_000.0));

        world.entity_mut(group).insert(Visibility::Inherited);
        regia_scene::propagate::propagate_transforms(&mut world);
        let statics = collect_statics(&mut world).expect("collect");
        assert_eq!(
            statics.sun.map(|s| s.illuminance),
            Some(99_000.0),
            "shown again, the lower-guid sun is the bake's sun"
        );
    }

    #[test]
    fn rounding_keeps_five_significant_digits() {
        assert_eq!(round5(123_456.0), 123_460.0);
        assert_eq!(round5(0.000_123_456), 0.000_123_46);
        assert_eq!(round5(0.0), 0.0);
        assert_eq!(round5(-1.234_567), -1.234_6);
    }

    /// Airless: a sun and no sky; Earth: a transmitted sun and a uniform sky; a sky body gets a
    /// radiance whose disc gives its fill; a night sun is dropped; no clock and no light, no sun.
    #[test]
    fn level_lights_follow_the_atmosphere() {
        let mut settings = LevelSettings::default();
        settings.atmosphere.preset = ScenePreset::Airless;
        settings.atmosphere.sun_illuminance_lux = Some(4_400.0);
        settings.atmosphere.time_of_day = Some(LevelTimeOfDay::default());
        settings.atmosphere.sky_body = Some(SkyBody {
            fill_lux: Some(30.0),
            ..SkyBody::jupiter_from_europa()
        });
        let airless = level_lights(&settings, None, None);
        assert_eq!(airless.sun_source, "time_of_day");
        let sun = airless.sun.expect("noon sun");
        assert_eq!(sun.illuminance, 4_400.0);
        assert!(sun.angular_radius < 0.001, "{}", sun.angular_radius);
        assert!(airless.sky.is_none());
        let body = airless.sky_bodies[0];
        let omega = std::f32::consts::TAU * (1.0 - body.angular_radius.cos());
        let t = SkyBody::jupiter_from_europa().tint;
        let lum = 0.2126 * body.radiance[0] + 0.7152 * body.radiance[1] + 0.0722 * body.radiance[2];
        assert!((lum * omega - 30.0).abs() < 0.1, "{}", lum * omega);
        assert!((body.radiance[0] / body.radiance[1] - t[0] / t[1]).abs() < 1e-3);

        settings.atmosphere.preset = ScenePreset::Earth;
        settings.atmosphere.sun_illuminance_lux = None;
        let earth = level_lights(&settings, None, None);
        let sun = earth.sun.expect("sun");
        assert!(
            sun.illuminance > 10_000.0 && sun.illuminance < 128_000.0,
            "{}",
            sun.illuminance
        );
        assert!(earth.sky.is_some_and(|s| s.iter().all(|c| *c > 0.0)));

        settings.atmosphere.time_of_day = Some(LevelTimeOfDay {
            hour: 0.0,
            ..LevelTimeOfDay::default()
        });
        assert!(level_lights(&settings, None, None).sun.is_none(), "midnight");

        settings.atmosphere.time_of_day = None;
        let none = level_lights(&settings, None, None);
        assert_eq!(none.sun_source, "none");
        assert!(none.sun.is_none() && none.sky.is_none());
        let authored = AuthoredSun {
            direction: [0.0, -1.0, 0.0],
            color: [1.0, 0.9, 0.8],
            illuminance: 50_000.0,
        };
        let lit = level_lights(&settings, Some(&authored), None);
        assert_eq!(lit.sun_source, "directional_light");
        let sun = lit.sun.expect("authored sun");
        assert_eq!(sun.toward, [0.0, 1.0, 0.0]);
        assert_eq!(sun.illuminance, 50_000.0);
        assert_eq!(sun.color, [1.0, 0.9, 0.8]);

        // An override wins over the authored light; 90° elevation points straight up.
        settings.atmosphere.preset = ScenePreset::Airless;
        let over = SunOverride {
            elevation_deg: 90.0,
            azimuth_deg: 0.0,
            lux: 4_400.0,
        };
        let lit = level_lights(&settings, Some(&authored), Some(&over));
        assert_eq!(lit.sun_source, "override");
        let sun = lit.sun.expect("override sun");
        assert_eq!(sun.illuminance, 4_400.0);
        assert!((sun.toward[1] - 1.0).abs() < 1e-4, "{:?}", sun.toward);
        let east = SunOverride {
            elevation_deg: 0.0,
            azimuth_deg: 90.0,
            lux: 1.0,
        }
        .toward();
        assert!((east[0] - 1.0).abs() < 1e-6 && east[2].abs() < 1e-6, "{east:?}");
    }
}
