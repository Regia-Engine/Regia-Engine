// SPDX-License-Identifier: MIT-0
//! DO27's exported content in the `look_r2` harness (`REGIA_DO27_CONTENT=<dir>`).
//!
//! The directory is DO27's read-only content drop (`do27-content/`, see its README): the
//! cooked terrain pack, the visual heightfield, the layer stack, the derived `.level.ron`
//! (v3, migrated to v4 in memory) and `.scatter.ron`, the map look (authored sun and
//! exposure), the merged set-piece glTF and the UE plate cameras. Nothing from it is
//! copied into the repository; without the variable the harness keeps its stand-ins.
//!
//! **Handedness.** The sim frame is left-handed (+X east, +Y up, +Z north). The README's
//! rule is one reflection, `render = (−sim.x, sim.y, sim.z)`, applied to every
//! world-space value. The harness applies that same reflection once, at the image: it
//! draws the sim frame unchanged in the right-handed renderer (which shows the mirror
//! image) and flips the finished frame left–right. Positions, directions, the
//! heightfield, the cooked macro maps and the baked set pieces therefore all agree
//! without re-encoding the block-compressed textures. The one difference from the shell:
//! glTF unit meshes (scatter rocks) come out mirrored, which is invisible for rock scans.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regia_core::glam::Vec3;
use regia_render::{LabMaterial, LabMesh, LabTexture, LabVertex};
use regia_scene::level::{LEVEL_SETTINGS_PATH, LevelSettings};
use regia_terrain::{
    CookedMaterialKtx2, HoleShape, HolesMask, MaterialParams, ScatterLibrary, ScatterSet, TerrainLayers,
    TerrainMap,
};

use crate::{LookdevError, Planet};

/// The environment variable naming the content directory.
pub const ENV: &str = "REGIA_DO27_CONTENT";

/// The content directory, when [`ENV`] is set.
#[must_use]
pub fn content_dir() -> Option<PathBuf> {
    std::env::var_os(ENV)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// The map folder of a planet.
#[must_use]
pub fn map_name(planet: Planet) -> &'static str {
    match planet {
        Planet::Europa => "bore_twelve",
        Planet::Mars => "switchbacks",
    }
}

/// The map's authored sun (`map_look.json atmosphere.sun`), in the sim frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Do27Sun {
    /// Unit vector toward the sun.
    pub toward: [f32; 3],
    /// Linear colour.
    pub color: [f32; 3],
    /// Illuminance, lux.
    pub lux: f32,
}

/// The sun of the UE v13 plates (elevation°, azimuth° clockwise from north), found by
/// rendering the DO27 content over an elevation/azimuth grid and keeping the direction whose
/// light and shadow correlate best with the plates (`docs/evidence/parity/2026-10-07`).
/// The Switchbacks plates agree with the map look's azimuth (148°) but not its 14°
/// elevation; the Bore Twelve plates agree with its 24° elevation but not its 78° azimuth.
#[must_use]
pub fn plate_sun(planet: Planet) -> (f32, f32) {
    match planet {
        Planet::Mars => (35.0, 148.0),
        Planet::Europa => (24.0, 185.0),
    }
}

/// Unit vector toward a sun at `el_deg` above the horizon and `az_deg` clockwise from north.
#[must_use]
pub fn sun_toward(el_deg: f32, az_deg: f32) -> [f32; 3] {
    from_el_az(el_deg, az_deg)
}

/// One plate camera in the sim frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Do27Camera {
    /// Eye, metres.
    pub eye: [f32; 3],
    /// Look target, metres.
    pub target: [f32; 3],
    /// Horizontal field of view, degrees.
    pub fov_h_deg: f32,
}

impl Do27Camera {
    /// Vertical field of view (radians) for an aspect ratio.
    #[must_use]
    pub fn fov_y(&self, aspect: f32) -> f32 {
        2.0 * ((self.fov_h_deg.to_radians() * 0.5).tan() / aspect.max(1e-3)).atan()
    }
}

/// Everything a map adds beyond the heightfield, layers and scatter.
pub struct Do27Map {
    /// The derived level settings (v3 → v4 in memory).
    pub level: LevelSettings,
    /// The authored directional light.
    pub sun: Do27Sun,
    /// Jupiter's direction (Bore Twelve), sim frame.
    pub jupiter: Option<[f32; 3]>,
    /// The map's authored fill (`map_look.json match_look.fill_lux`): Jupiter-shine on
    /// Bore Twelve, a directionless fill on the Switchbacks (not used there).
    pub fill_lux: Option<f32>,
    /// Cooked layer and macro textures.
    pub cooked: CookedMaterialKtx2,
    /// Material parameters from the pack index.
    pub params: MaterialParams,
    /// Macro tile grid side.
    pub macro_tiles: u32,
    /// Set pieces (and the Switchbacks tunnels), LOD0, baked at sim positions.
    pub props: Vec<LabMesh>,
    /// Their materials; [`LabMesh::material`] indexes this list after `material_base`.
    pub prop_materials: Vec<LabMaterial>,
    /// The tunnel mouths' open-cut trenches cut through the terrain ([`tunnel_mouths`]).
    pub holes: HolesMask,
}

/// Length of the open-cut trench at a tunnel mouth, back from where its ramp meets the
/// ground (DO27: about 20 m wide × 25 m long per mouth).
pub const MOUTH_TRENCH_M: f32 = 25.0;

/// The tunnel mouths of `tunnels.json` (`do27-tunnels/1`, sim metres) as terrain holes: per
/// opening, a trench as wide as its ramp running [`MOUTH_TRENCH_M`] back down the ramp's last
/// leg from the opening's `top`. Empty when the map has no tunnels.
///
/// # Errors
///
/// The file exists but is not valid tunnel JSON.
pub fn tunnel_mouths(map_dir: &Path) -> Result<HolesMask, LookdevError> {
    let path = map_dir.join("tunnels.json");
    if !path.exists() {
        return Ok(HolesMask::default());
    }
    let json = read_json(&path)?;
    let layout = &json["layout"];
    let passages = layout["passages"].as_array().cloned().unwrap_or_default();
    let mut shapes = Vec::new();
    for opening in layout["openings"].as_array().cloned().unwrap_or_default() {
        let top = vec3(&opening["top"], "opening top")?;
        let ramp_id = opening["ramp"].as_str().unwrap_or_default();
        let Some(ramp) = passages.iter().find(|p| p["id"].as_str() == Some(ramp_id)) else {
            continue;
        };
        let width = num(&ramp["width_m"], "ramp width_m")?;
        let points = ramp["points"].as_array().cloned().unwrap_or_default();
        // The ramp point before the top (the top is the last point in DO27's layout).
        let Some(prev) = points
            .iter()
            .filter_map(|p| vec3(p, "ramp point").ok())
            .rev()
            .find(|p| (p[0] - top[0]).hypot(p[2] - top[2]) > 1.0)
        else {
            continue;
        };
        let (dx, dz) = (prev[0] - top[0], prev[2] - top[2]);
        let len = dx.hypot(dz);
        let back = MOUTH_TRENCH_M.min(len);
        let from = [top[0] + dx / len * back, top[2] + dz / len * back];
        shapes.push(HoleShape::segment(from, [top[0], top[2]], width));
    }
    // Diagnostic: CHASE_HOLE_RASTER=N cuts each trench as an N × N coverage raster over the
    // same rectangle (a host that rasterises its cuts), to time the terrain's per-node hole
    // tests against heap-holding shapes.
    if let Some(n) = std::env::var("CHASE_HOLE_RASTER")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
    {
        for shape in &mut shapes {
            if let HoleShape::Rect {
                center,
                half_extents,
                rotation_radians,
            } = *shape
            {
                *shape = HoleShape::Raster(regia_terrain::HoleRaster {
                    center,
                    half_extents,
                    rotation_radians,
                    width: n,
                    height: n,
                    coverage: vec![255; (n * n) as usize],
                });
            }
        }
    }
    HolesMask::from_shapes(shapes).map_err(|e| LookdevError::Json(format!("{}: {e}", path.display())))
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> LookdevError + '_ {
    move |source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn read_json(path: &Path) -> Result<serde_json::Value, LookdevError> {
    let text = std::fs::read_to_string(path).map_err(io(path))?;
    serde_json::from_str(&text).map_err(|e| LookdevError::Json(format!("{}: {e}", path.display())))
}

fn num(v: &serde_json::Value, what: &str) -> Result<f32, LookdevError> {
    v.as_f64()
        .map(|x| x as f32)
        .ok_or_else(|| LookdevError::Json(format!("{what} is not a number")))
}

fn vec3(v: &serde_json::Value, what: &str) -> Result<[f32; 3], LookdevError> {
    let a = v
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| LookdevError::Json(format!("{what} is not a 3-vector")))?;
    Ok([num(&a[0], what)?, num(&a[1], what)?, num(&a[2], what)?])
}

/// Unit vector for an elevation and an azimuth clockwise from north (+Z) through east (+X).
fn from_el_az(el_deg: f32, az_deg: f32) -> [f32; 3] {
    let (el, az) = (el_deg.to_radians(), az_deg.to_radians());
    [az.sin() * el.cos(), el.sin(), az.cos() * el.cos()]
}

/// The twelve plate cameras by shot name: `ue_camera_spec.json` for the ten landscape
/// shots, and the README's FX camera (eye `d` metres back along sim +Z and 6 m up over
/// the Bore Twelve team-1 start, target 2.5 m up, 60° vertical at 16:9) for
/// `fireball_20m` (d = 20) and `smoke_80m` (d = 80).
///
/// # Errors
///
/// The spec is missing or malformed.
pub fn load_cameras(dir: &Path) -> Result<HashMap<String, Do27Camera>, LookdevError> {
    let spec = read_json(&dir.join("ue_camera_spec.json"))?;
    let mut out = HashMap::new();
    for shot in spec
        .as_array()
        .ok_or_else(|| LookdevError::Json("ue_camera_spec.json is not a list".into()))?
    {
        let name = shot["shot"]
            .as_str()
            .ok_or_else(|| LookdevError::Json("shot without a name".into()))?;
        out.insert(
            name.to_string(),
            Do27Camera {
                eye: vec3(&shot["sim_position_m"], "sim_position_m")?,
                target: vec3(&shot["sim_target_m"], "sim_target_m")?,
                fov_h_deg: num(&shot["fov_horizontal_deg"], "fov_horizontal_deg")?,
            },
        );
    }
    let start = [-550.0_f32, -32.31, 0.0];
    // 60° vertical at 16:9 is 91.5° horizontal.
    let fov_h = 2.0 * ((30.0_f32.to_radians()).tan() * 16.0 / 9.0).atan().to_degrees();
    for (name, d) in [("fireball_20m", 20.0), ("smoke_80m", 80.0)] {
        out.insert(
            name.to_string(),
            Do27Camera {
                eye: [start[0], start[1] + 6.0, start[2] + d],
                target: [start[0], start[1] + 2.5, start[2]],
                fov_h_deg: fov_h,
            },
        );
    }
    Ok(out)
}

/// The 4096² visual heightfield (16-bit PNG, `sample / 32 − 512` metres, row 0 south).
///
/// # Errors
///
/// The PNG or its JSON is missing or not the documented encoding.
pub fn load_heightfield(map_dir: &Path) -> Result<TerrainMap, LookdevError> {
    let meta = read_json(&map_dir.join("visual/heightfield.json"))?;
    // DO27's export names the engine's section with the name from before the rename (ADR-1429).
    let engine = if meta.get("regia").is_some() {
        &meta["regia"]
    } else {
        &meta[regia_core::brand::LEGACY_MAIN_CRATE]
    };
    let world = num(&engine["world_width_m"], "world_width_m")?;
    let centre = num(&engine["center_height_m"], "center_height_m")?;
    let path = map_dir.join("visual/heightfield.png");
    let file = std::fs::File::open(&path).map_err(io(&path))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = decoder
        .read_info()
        .map_err(|e| LookdevError::Png(format!("{}: {e}", path.display())))?;
    let info = reader.info();
    let (w, h) = (info.width, info.height);
    if info.bit_depth != png::BitDepth::Sixteen || info.color_type != png::ColorType::Grayscale {
        return Err(LookdevError::Png(format!(
            "{}: expected 16-bit grayscale, found {:?} {:?}",
            path.display(),
            info.bit_depth,
            info.color_type
        )));
    }
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    reader
        .next_frame(&mut buf)
        .map_err(|e| LookdevError::Png(format!("{}: {e}", path.display())))?;
    let heights: Vec<f32> = buf
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| f32::from(u16::from_be_bytes(*b)) / 32.0 - centre)
        .collect();
    Ok(
        TerrainMap::from_heights(w, h, heights, world, 1.0, 0.0)
            .map_err(regia_terrain::TerrainError::from)?,
    )
}

/// The layer stack (`textures/terrain.layers.ron`, migrated to the current version).
///
/// # Errors
///
/// The file is missing or does not parse.
pub fn load_layers(map_dir: &Path) -> Result<TerrainLayers, LookdevError> {
    let path = map_dir.join("textures/terrain.layers.ron");
    let text = std::fs::read_to_string(&path).map_err(io(&path))?;
    Ok(TerrainLayers::from_ron(&text)?)
}

/// The `.scatter.ron` class summary.
///
/// # Errors
///
/// The file is missing or does not parse.
pub fn load_scatter_set(map_dir: &Path, layers: &TerrainLayers) -> Result<ScatterSet, LookdevError> {
    let path = map_dir.join("scatter.ron");
    let text = std::fs::read_to_string(&path).map_err(io(&path))?;
    Ok(ScatterSet::from_ron(&text, Some(layers))?)
}

/// Procedural stand-ins for every rock path the set names (the scanned rock glTFs are not
/// part of the content drop): the shared palettes ([`regia_terrain::standin_library`]),
/// brown sandstone and basalt darker than the dust on Mars, ice and dark basalt on Europa,
/// 1 m across at scale 1 like DO27's own meshes.
#[must_use]
pub fn rock_library(planet: Planet, set: &ScatterSet) -> ScatterLibrary {
    let palette = match planet {
        Planet::Mars => regia_terrain::RockPalette::Mars,
        Planet::Europa => regia_terrain::RockPalette::Europa,
    };
    regia_terrain::standin_library(set, palette)
}

/// The cooked pack (`cooked/terrain.tpak`, or the file `REGIA_DO27_TPAK` names): `DO27TPK1`,
/// a little-endian u32 index length, the JSON index, then the KTX2 blobs in index order (BC from
/// the desktop cook, or ASTC).
///
/// # Errors
///
/// The pack is missing, truncated or its index is malformed.
pub fn load_cooked(map_dir: &Path) -> Result<(CookedMaterialKtx2, MaterialParams, u32), LookdevError> {
    // `REGIA_DO27_TPAK=<file>` loads another pack from `cooked/` (an Android ASTC transcode,
    // `regia_asset_pipeline`'s `tpak_astc` example) in place of the shell's BC pack.
    let file = std::env::var("REGIA_DO27_TPAK").unwrap_or_else(|_| "terrain.tpak".into());
    let path = map_dir.join("cooked").join(file);
    let bytes = std::fs::read(&path).map_err(io(&path))?;
    let bad = |what: &str| LookdevError::Json(format!("{}: {what}", path.display()));
    if bytes.len() < 12 || &bytes[..8] != b"DO27TPK1" {
        return Err(bad("not a DO27TPK1 pack"));
    }
    let n = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let index_end = 12 + n;
    let index: serde_json::Value =
        serde_json::from_slice(bytes.get(12..index_end).ok_or_else(|| bad("short index"))?)
            .map_err(|e| bad(&e.to_string()))?;
    let mut cooked = CookedMaterialKtx2::default();
    let mut at = index_end;
    for blob in index["blobs"].as_array().ok_or_else(|| bad("no blobs"))? {
        let len = blob["len"].as_u64().ok_or_else(|| bad("blob without len"))? as usize;
        let data = bytes
            .get(at..at + len)
            .ok_or_else(|| bad("truncated blob"))?
            .to_vec();
        at += len;
        match blob["section"].as_str().unwrap_or_default() {
            "layer_albedo" => cooked.layer_albedo.push(data),
            "layer_normal" => cooked.layer_normal.push(data),
            "layer_height" => cooked.layer_height.push(data),
            "layer_orm" => cooked.layer_orm.push(data),
            "macro_albedo_tiles" => cooked.macro_albedo_tiles.push(data),
            "macro_normal_tiles" => cooked.macro_normal_tiles.push(data),
            "macro_coarse_albedo" => cooked.macro_coarse_albedo = Some(data),
            "macro_coarse_normal" => cooked.macro_coarse_normal = Some(data),
            other => return Err(bad(&format!("unknown section {other}"))),
        }
    }
    let p = &index["params"];
    let get = |key: &str, fallback: f32| p[key].as_f64().map_or(fallback, |v| v as f32);
    let d = MaterialParams::default();
    let params = MaterialParams {
        macro_world_size: get("macro_world_size", d.macro_world_size),
        macro_strength: get("macro_strength", d.macro_strength),
        macro_normal_strength: get("macro_normal_strength", d.macro_normal_strength),
        anti_tiling_cell: get("anti_tiling_cell", d.anti_tiling_cell),
        detail_fade_start: get("detail_fade_start", d.detail_fade_start),
        detail_fade_end: get("detail_fade_end", d.detail_fade_end),
        triplanar_auto_slope: get("triplanar_auto_slope", d.triplanar_auto_slope),
    };
    let tiles = index["macro_tiles"].as_u64().unwrap_or(1) as u32;
    Ok((cooked, params, tiles))
}

/// `level.ron` (a v3 `SceneFile`), migrated to the current format in memory.
///
/// # Errors
///
/// The file is missing, does not migrate, or has no level settings.
pub fn load_level(map_dir: &Path) -> Result<LevelSettings, LookdevError> {
    let path = map_dir.join("level.ron");
    let text = std::fs::read_to_string(&path).map_err(io(&path))?;
    let bad = |e: String| LookdevError::Config(format!("{}: {e}", path.display()));
    let mut file = regia_scene::format::SceneFile::from_ron_str(&text).map_err(|e| bad(e.to_string()))?;
    regia_scene::format::migrate_to_latest(&mut file).map_err(|e| bad(e.to_string()))?;
    // Frozen DO27 exports key the resource by the old crate name (`ferrite_scene::...`; rename-keep).
    let raw = file
        .resources
        .iter()
        .find(|(key, _)| regia_core::brand::current_type_path(key) == LEVEL_SETTINGS_PATH)
        .map(|(_, raw)| raw)
        .ok_or_else(|| bad("no LevelSettings resource".into()))?;
    LevelSettings::from_ron(raw.get_ron()).map_err(|e| bad(e.to_string()))
}

/// The authored sun and Jupiter from `map_look.json`.
///
/// # Errors
///
/// The file is missing or lacks the sun block.
pub fn load_sun(map_dir: &Path) -> Result<(Do27Sun, Option<[f32; 3]>), LookdevError> {
    load_look(map_dir).map(|look| (look.sun, look.jupiter))
}

/// The parts of `map_look.json` the harness uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapLook {
    /// The authored sun.
    pub sun: Do27Sun,
    /// Jupiter's direction (shell space reflected into the sim frame).
    pub jupiter: Option<[f32; 3]>,
    /// `match_look.fill_lux`.
    pub fill_lux: Option<f32>,
}

/// The authored sun, Jupiter and fill from `map_look.json`.
///
/// # Errors
///
/// The file is missing or lacks the sun block.
pub fn load_look(map_dir: &Path) -> Result<MapLook, LookdevError> {
    let look = read_json(&map_dir.join("map_look.json"))?;
    let sun = &look["atmosphere"]["sun"];
    let el = num(&sun["elevation_deg"], "sun.elevation_deg")?;
    let az = num(&sun["azimuth_deg"], "sun.azimuth_deg")?;
    let color = vec3(&sun["color_linear"], "sun.color_linear")?;
    let lux = num(&sun["intensity_lux"], "sun.intensity_lux")?;
    let sky = &look["atmosphere"]["sky"];
    // The map look gives Jupiter in shell (render) space; this harness draws the sim frame,
    // so reflect it back (negate east).
    let jupiter = match (
        sky["jupiter_elevation_deg"].as_f64(),
        sky["jupiter_azimuth_deg"].as_f64(),
    ) {
        (Some(el), Some(az)) => {
            let [x, y, z] = from_el_az(el as f32, az as f32);
            Some([-x, y, z])
        }
        _ => None,
    };
    let fill = look["match_look"]["fill_lux"].as_f64().map(|v| v as f32);
    Ok(MapLook {
        sun: Do27Sun {
            toward: from_el_az(el, az),
            color,
            lux,
        },
        jupiter,
        fill_lux: fill,
    })
}

/// The set pieces (LOD0 of the merged glTF) and, on the Switchbacks, the tunnel mesh,
/// at their baked sim positions. Textures are box-filtered to 1024² on load.
///
/// # Errors
///
/// A glTF, its buffers or a referenced texture failed to load.
pub fn load_props(
    map_dir: &Path,
    material_base: usize,
) -> Result<(Vec<LabMesh>, Vec<LabMaterial>), LookdevError> {
    let mut meshes = Vec::new();
    let mut materials = Vec::new();
    let setpieces = std::fs::read_dir(map_dir.join("setpieces"))
        .map_err(io(&map_dir.join("setpieces")))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "gltf"));
    // LOOK_R2_NO_BASE_SETPIECES=1: skip the merged set-piece glTF (a map whose hero set pieces,
    // loaded through LOOK_R2_EXTRA_GLTF, replace it).
    let setpieces = setpieces.filter(|_| std::env::var_os("LOOK_R2_NO_BASE_SETPIECES").is_none());
    let mut sources: Vec<(PathBuf, bool)> = setpieces.into_iter().map(|p| (p, true)).collect();
    let tunnels = map_dir.join("tunnels.gltf");
    if tunnels.exists() && std::env::var_os("LOOK_R2_NO_TUNNELS").is_none() {
        sources.push((tunnels, false));
    }
    for (path, lod0_only) in sources {
        load_gltf(&path, lod0_only, material_base, &mut meshes, &mut materials)?;
    }
    Ok((meshes, materials))
}

/// A minimal glTF 2.0 reader for DO27's exports (JSON + external `.bin`, float positions,
/// normals and UVs, u16/u32 indices, PNG images by URI): no `gltf` crate in a runtime-layer
/// sample. Node transforms other than identity are not used by these files (baked meshes).
/// Every `.gltf` under `dir` (recursively, sorted) except the `_LOD1`.. files: one model each.
#[must_use]
pub fn lod0_gltfs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for path in entries.filter_map(Result::ok).map(|e| e.path()) {
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "gltf")
                && !path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| (1..=9).any(|n| s.contains(&format!("_LOD{n}"))))
            {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Grey stand-in boxes for the base buildings, comm towers and parked vehicles listed in
/// `placements.json` `bases` (DO27's meshes for them live in its repository, not in the
/// drop). Sizes follow the role: footprint-wide halls, a tall mast for the comm tower,
/// hull-sized boxes for vehicles. Material index `material` (a neutral hull grey).
///
/// # Errors
///
/// `placements.json` is missing or malformed.
pub fn load_base_standins(map_dir: &Path, material: usize) -> Result<Vec<LabMesh>, LookdevError> {
    let doc = read_json(&map_dir.join("placements.json"))?;
    let mut out = Vec::new();
    let empty = Vec::new();
    for base in doc["bases"].as_array().unwrap_or(&empty) {
        let items = base["buildings"]
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .chain(base["vehicles"].as_array().unwrap_or(&empty));
        for item in items {
            let at = vec3(&item["position_m"], "position_m")?;
            let footprint = item["footprint_m"].as_f64().unwrap_or(10.0) as f32;
            let half = match item["role"].as_str().unwrap_or_default() {
                "commtower" => [1.5, 12.0, 1.5],
                "silo" => [4.0, 7.0, 4.0],
                "guntower" => [3.0, 6.0, 3.0],
                "reactor" => [5.0, 4.0, 5.0],
                "warhull" => [7.0, 2.5, 3.5],
                "raider" => [4.5, 1.6, 2.5],
                "skirmisher" => [3.0, 1.2, 2.0],
                _ => [footprint * 0.5, footprint * 0.22, footprint * 0.35],
            };
            let (vertices, indices) = regia_render::box_mesh(half);
            let model = regia_core::glam::Mat4::from_translation(Vec3::new(at[0], at[1] + half[1], at[2]));
            out.push(LabMesh {
                vertices,
                indices,
                material,
                model: model.to_cols_array_2d(),
                ..LabMesh::unskinned()
            });
        }
    }
    Ok(out)
}

/// Appends the meshes and materials of one of DO27's glTF exports (the minimal reader: JSON +
/// external `.bin`, float attributes, u16/u32 indices, PNG textures box-filtered to 1024²).
/// `lod0_only` keeps only `_LOD0` nodes; material indices start at `material_base` plus the
/// materials already in `materials`.
///
/// # Errors
///
/// The glTF, a buffer or a texture failed to load or is malformed.
pub fn load_gltf(
    path: &Path,
    lod0_only: bool,
    material_base: usize,
    meshes: &mut Vec<LabMesh>,
    materials: &mut Vec<LabMaterial>,
) -> Result<(), LookdevError> {
    load_gltf_capped(
        path,
        lod0_only,
        material_base,
        meshes,
        materials,
        set_piece_texture_max(),
    )
}

/// [`load_gltf`] with the largest texture edge given (`texture_max`, texels) instead of
/// [`set_piece_texture_max`]: small scatter rocks need far fewer texels than a set piece.
///
/// # Errors
///
/// As [`load_gltf`].
pub fn load_gltf_capped(
    path: &Path,
    lod0_only: bool,
    material_base: usize,
    meshes: &mut Vec<LabMesh>,
    materials: &mut Vec<LabMaterial>,
    texture_max: u32,
) -> Result<(), LookdevError> {
    let bad = |e: &str| LookdevError::Json(format!("{}: {e}", path.display()));
    let doc = read_json(path)?;
    let base = path.parent().unwrap_or(Path::new("."));
    let empty = Vec::new();
    let list = |key: &str| doc[key].as_array().unwrap_or(&empty);
    let buffers: Vec<Vec<u8>> = list("buffers")
        .iter()
        .map(|b| {
            let uri = b["uri"]
                .as_str()
                .ok_or_else(|| bad("embedded buffers are not supported"))?;
            let p = base.join(uri);
            std::fs::read(&p).map_err(io(&p))
        })
        .collect::<Result<_, _>>()?;
    let first_material = material_base + materials.len();
    let images = list("images");
    let textures = list("textures");
    // Every image the materials name, decoded on all cores up front: the decode of DO27's
    // 4 K set-piece PNGs is most of the level load when it runs one image at a time.
    // LOOK_R2_COOKED_MAPS=<dir>: images with a cooked `<dir>/<stem>.ktx2` (regia_asset_pipeline
    // `cook_material_maps`) upload block-compressed and are not decoded at all.
    let cooked_dir = std::env::var_os("LOOK_R2_COOKED_MAPS").map(PathBuf::from);
    let cooked_path = |source: usize| -> Option<PathBuf> {
        let dir = cooked_dir.as_ref()?;
        let uri = images.get(source)?["uri"].as_str()?;
        let stem = Path::new(uri).file_stem()?.to_str()?;
        Some(dir.join(format!("{stem}.ktx2"))).filter(|p| p.exists())
    };
    let source_of = |info: &serde_json::Value| -> Option<usize> {
        info["index"]
            .as_u64()
            .and_then(|t| textures.get(t as usize))
            .and_then(|tex| tex["source"].as_u64())
            .map(|v| v as usize)
    };
    let mut cooked: HashMap<usize, regia_render::CompressedTexture> = HashMap::new();
    let mut wanted: Vec<(usize, PathBuf)> = Vec::new();
    for mat in list("materials") {
        let pbr = &mat["pbrMetallicRoughness"];
        for info in [
            &pbr["baseColorTexture"],
            &mat["normalTexture"],
            &mat["occlusionTexture"],
        ] {
            let source = info["index"]
                .as_u64()
                .and_then(|t| textures.get(t as usize))
                .and_then(|tex| tex["source"].as_u64())
                .map(|v| v as usize);
            if let Some(source) = source
                && !cooked.contains_key(&source)
                && let Some(path) = cooked_path(source)
            {
                let bytes = std::fs::read(&path).map_err(io(&path))?;
                let map = regia_render::CompressedTexture::from_ktx2(&bytes)
                    .map_err(|e| LookdevError::Config(format!("{}: {e}", path.display())))?;
                cooked.insert(source, map);
                continue;
            }
            if let Some(source) = source
                && !cooked.contains_key(&source)
                && !wanted.iter().any(|(s, _)| *s == source)
                && let Some(uri) = images.get(source).and_then(|i| i["uri"].as_str())
            {
                wanted.push((source, base.join(uri)));
            }
        }
    }
    let mut cache = load_pngs_parallel(&wanted, texture_max)?;
    let mut texture = |info: &serde_json::Value| -> Result<Option<LabTexture>, LookdevError> {
        let Some(tex) = info["index"].as_u64().and_then(|t| textures.get(t as usize)) else {
            return Ok(None);
        };
        let Some(source) = tex["source"].as_u64().map(|v| v as usize) else {
            return Ok(None);
        };
        if cooked.contains_key(&source) {
            return Ok(None);
        }
        if let Some(t) = cache.get(&source) {
            return Ok(Some(t.clone()));
        }
        let Some(uri) = images.get(source).and_then(|i| i["uri"].as_str()) else {
            return Ok(None);
        };
        let t = load_png_scaled(&base.join(uri), texture_max.min(1024))?;
        cache.insert(source, t.clone());
        Ok(Some(t))
    };
    for mat in list("materials") {
        let pbr = &mat["pbrMetallicRoughness"];
        let c = pbr["baseColorFactor"].as_array().map_or([1.0; 4], |a| {
            let f = |i: usize| a.get(i).and_then(serde_json::Value::as_f64).unwrap_or(1.0) as f32;
            [f(0), f(1), f(2), f(3)]
        });
        // DO27 packs occlusion, roughness and metal in one ORM image that both glTF slots
        // point at. Then the shader must scale the factors by its G and B (glTF 2.0
        // § 3.9.2, `LabMaterial::orm_packed`): without it the scalar factors alone applied,
        // and a material with no `metallicFactor` (glTF default 1.0) drew fully metallic —
        // the Bore Twelve hangar printed as a black slab under the airless sky.
        let source = |info: &serde_json::Value| {
            info["index"]
                .as_u64()
                .and_then(|t| textures.get(t as usize))
                .and_then(|t| t["source"].as_u64())
        };
        let orm_packed = source(&mat["occlusionTexture"]).is_some()
            && source(&mat["occlusionTexture"]) == source(&pbr["metallicRoughnessTexture"]);
        // As a shell's `from_authored_resolved` does: images fitted to the tier's resident cap
        // (pixels above it are never uploaded, so not kept either).
        let base = regia_render::hard_surface::ResolvedBaseMaps {
            albedo: texture(&pbr["baseColorTexture"])?,
            normal: texture(&mat["normalTexture"])?,
            occlusion: texture(&mat["occlusionTexture"])?,
            emissive: None,
        }
        .fit_to_resident_cap();
        let material = LabMaterial {
            base_color: c,
            // LOOK_R2_PREVIEW_DO27=1: DO27's announced fix for the hangar (concrete, metallic 0).
            metallic: if std::env::var_os("LOOK_R2_PREVIEW_DO27").is_some()
                && mat["name"].as_str() == Some("M_Hangar")
            {
                0.0
            } else {
                pbr["metallicFactor"].as_f64().unwrap_or(1.0) as f32
            },
            roughness: pbr["roughnessFactor"].as_f64().unwrap_or(1.0) as f32,
            albedo: base.albedo,
            normal_map: base.normal,
            occlusion_map: base.occlusion,
            compressed: compressed_maps(
                &cooked,
                [
                    source_of(&pbr["baseColorTexture"]),
                    source_of(&mat["normalTexture"]),
                    source_of(&mat["occlusionTexture"]),
                ],
            ),
            orm_packed,
            ..LabMaterial::default()
        };
        // LOOK_R2_DUMP_MATERIAL=<name>: the material as uploaded (factors, maps, packing).
        if std::env::var("LOOK_R2_DUMP_MATERIAL").ok().as_deref() == mat["name"].as_str() {
            let mean = |t: &Option<LabTexture>| {
                t.as_ref().map(|t| {
                    let n = (t.rgba.len() / 4).max(1) as f64;
                    let mut m = [0.0f64; 4];
                    for px in t.rgba.as_chunks::<4>().0 {
                        for k in 0..4 {
                            m[k] += f64::from(px[k]) / 255.0;
                        }
                    }
                    (t.width, t.height, m.map(|v| (v / n * 1000.0).round() / 1000.0))
                })
            };
            eprintln!(
                "material {}: base_color {:?} metallic {} roughness {} normal_scale {} occlusion_strength {} orm_packed {} hard_surface {:?} albedo(sRGB bytes, mean) {:?} orm(linear, mean) {:?} normal {:?}",
                mat["name"],
                material.base_color,
                material.metallic,
                material.roughness,
                material.normal_scale,
                material.occlusion_strength,
                material.orm_packed,
                material.hard_surface,
                mean(&material.albedo),
                mean(&material.occlusion_map),
                mean(&material.normal_map)
            );
        }
        // As a shell's `from_authored_resolved` does: the texture report names the material.
        if let Some(name) = mat["name"].as_str() {
            regia_render::name_material_textures(&material, name);
        }
        materials.push(material);
    }
    if list("materials").is_empty() {
        materials.push(LabMaterial::default());
    }
    let accessors = list("accessors");
    let views = list("bufferViews");
    // (bytes, count, components, component type, stride)
    let accessor = |index: u64| -> Result<(&[u8], usize, usize, u64, usize), LookdevError> {
        let a = accessors
            .get(index as usize)
            .ok_or_else(|| bad("accessor out of range"))?;
        let view = views
            .get(
                a["bufferView"]
                    .as_u64()
                    .ok_or_else(|| bad("sparse accessors are not supported"))? as usize,
            )
            .ok_or_else(|| bad("bufferView out of range"))?;
        let buffer = buffers
            .get(view["buffer"].as_u64().unwrap_or(0) as usize)
            .ok_or_else(|| bad("buffer out of range"))?;
        let comps = match a["type"].as_str().unwrap_or("SCALAR") {
            "VEC2" => 2,
            "VEC3" => 3,
            "VEC4" => 4,
            _ => 1,
        };
        let ctype = a["componentType"].as_u64().unwrap_or(5126);
        let size = match ctype {
            5121 => 1,
            5123 => 2,
            _ => 4,
        };
        let stride = view["byteStride"].as_u64().map_or(comps * size, |v| v as usize);
        let start =
            (view["byteOffset"].as_u64().unwrap_or(0) + a["byteOffset"].as_u64().unwrap_or(0)) as usize;
        let count = a["count"].as_u64().unwrap_or(0) as usize;
        let end = start + stride * count.saturating_sub(1) + comps * size;
        let bytes = buffer
            .get(start..end.max(start))
            .ok_or_else(|| bad("accessor past its buffer"))?;
        Ok((bytes, count, comps, ctype, stride))
    };
    let floats = |index: u64, want: usize| -> Result<Vec<f32>, LookdevError> {
        let (bytes, count, comps, ctype, stride) = accessor(index)?;
        if ctype != 5126 || comps != want {
            return Err(bad("expected float vertex data"));
        }
        let mut out = Vec::with_capacity(count * comps);
        for i in 0..count {
            for c in 0..comps {
                let o = i * stride + c * 4;
                out.push(f32::from_le_bytes([
                    bytes[o],
                    bytes[o + 1],
                    bytes[o + 2],
                    bytes[o + 3],
                ]));
            }
        }
        Ok(out)
    };
    let mesh_list = list("meshes");
    // LOOK_R2_LOD=<n>: the `_LOD<n>` nodes instead of `_LOD0` (what a runtime draws far away).
    let lod_suffix = format!(
        "_LOD{}",
        std::env::var("LOOK_R2_LOD")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0)
    );
    for node in list("nodes") {
        let Some(mesh_index) = node["mesh"].as_u64() else {
            continue;
        };
        if lod0_only && !node["name"].as_str().is_some_and(|n| n.ends_with(&lod_suffix)) {
            continue;
        }
        let Some(mesh) = mesh_list.get(mesh_index as usize) else {
            continue;
        };
        for prim in mesh["primitives"].as_array().unwrap_or(&empty) {
            let attrs = &prim["attributes"];
            let Some(pos_index) = attrs["POSITION"].as_u64() else {
                continue;
            };
            let positions = floats(pos_index, 3)?;
            let n = positions.len() / 3;
            let normals = match attrs["NORMAL"].as_u64() {
                Some(i) => floats(i, 3)?,
                None => [0.0, 1.0, 0.0].repeat(n),
            };
            let uvs = match attrs["TEXCOORD_0"].as_u64() {
                Some(i) => floats(i, 2)?,
                None => vec![0.0; n * 2],
            };
            let mut indices: Vec<u32> = match prim["indices"].as_u64() {
                Some(i) => {
                    let (bytes, count, _, ctype, stride) = accessor(i)?;
                    (0..count)
                        .map(|k| {
                            let o = k * stride;
                            match ctype {
                                5121 => u32::from(bytes[o]),
                                5123 => u32::from(u16::from_le_bytes([bytes[o], bytes[o + 1]])),
                                _ => u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]),
                            }
                        })
                        .collect()
                }
                None => (0..n as u32).collect(),
            };
            if normals.len() != n * 3 || uvs.len() != n * 2 {
                return Err(bad("attribute counts differ"));
            }
            // LOOK_R2_VERTEX_COLOR=1: COLOR_0 multiplies the base colour (glTF 2.0 § 3.7.2.1),
            // float VEC3 / VEC4, as DO27's shell draws it (Bore Twelve's brown stain between the
            // ice plates is vertex colour). Opt-in so look_ci's DO27 frames stay comparable.
            let colors = match attrs["COLOR_0"].as_u64() {
                Some(i) if std::env::var_os("LOOK_R2_VERTEX_COLOR").is_some() => {
                    let comps = accessor(i)?.2;
                    floats(i, comps)
                        .ok()
                        .filter(|c| comps >= 3 && c.len() == n * comps)
                        .map(|c| (c, comps))
                }
                _ => None,
            };
            let mut vertices: Vec<LabVertex> = (0..n)
                .map(|k| {
                    let mut v = LabVertex::from_pos_normal_uv(
                        [positions[k * 3], positions[k * 3 + 1], positions[k * 3 + 2]],
                        [normals[k * 3], normals[k * 3 + 1], normals[k * 3 + 2]],
                        [uvs[k * 2], uvs[k * 2 + 1]],
                    );
                    if let Some((c, comps)) = &colors {
                        let o = k * comps;
                        let a = if *comps == 4 { c[o + 3] } else { 1.0 };
                        v.color = [c[o], c[o + 1], c[o + 2], a];
                    }
                    v
                })
                .collect();
            // Coincident duplicate triangles z-fight (`regia_render::drop_coincident_triangles`).
            let corners: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
            regia_render::drop_coincident_triangles(&corners, &mut indices);
            compute_tangents(&mut vertices, &indices);
            meshes.push(LabMesh {
                vertices,
                indices,
                material: first_material + prim["material"].as_u64().unwrap_or(0) as usize,
                ..LabMesh::unskinned()
            });
        }
    }
    Ok(())
}

/// Per-vertex MikkTSpace tangents, as glTF 2.0 § 3.7.2.1 requires for a mesh without
/// `TANGENT` (and as the engine's cook and Blender's importer generate them): DO27's normal
/// maps are baked against that basis, and any other basis bends the baked bevels and panel
/// seams. Falls back to [`lengyel_tangents`] if generation fails; `LOOKDEV_LENGYEL_TANGENTS`
/// forces the fallback (diagnostic).
pub(crate) fn compute_tangents(vertices: &mut [LabVertex], indices: &[u32]) {
    if std::env::var_os("LOOKDEV_LENGYEL_TANGENTS").is_none() {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        let mut tangents = vec![[0.0f32; 4]; vertices.len()];
        if regia_render::generate_vertex_tangents(&positions, &normals, &uvs, indices, &mut tangents) {
            for (v, t) in vertices.iter_mut().zip(&tangents) {
                if t[0] != 0.0 || t[1] != 0.0 || t[2] != 0.0 {
                    v.tangent = *t;
                } else {
                    // A vertex no triangle reaches (or a degenerate UV fan): any frame.
                    let n = Vec3::from_array(v.normal).normalize_or_zero();
                    let a = if n == Vec3::ZERO {
                        Vec3::X
                    } else {
                        n.any_orthonormal_vector()
                    };
                    v.tangent = [a.x, a.y, a.z, 1.0];
                }
            }
            return;
        }
    }
    lengyel_tangents(vertices, indices);
}

/// Per-vertex tangents from UV gradients (Lengyel 2001), Gram–Schmidt against the normal.
pub(crate) fn lengyel_tangents(vertices: &mut [LabVertex], indices: &[u32]) {
    let mut tan = vec![Vec3::ZERO; vertices.len()];
    let mut bit = vec![Vec3::ZERO; vertices.len()];
    for tri in indices.as_chunks::<3>().0 {
        let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
        if a >= vertices.len() || b >= vertices.len() || c >= vertices.len() {
            continue;
        }
        let (p0, p1, p2) = (
            Vec3::from_array(vertices[a].position),
            Vec3::from_array(vertices[b].position),
            Vec3::from_array(vertices[c].position),
        );
        let (u0, u1, u2) = (vertices[a].uv, vertices[b].uv, vertices[c].uv);
        let (e1, e2) = (p1 - p0, p2 - p0);
        let (du1, dv1, du2, dv2) = (u1[0] - u0[0], u1[1] - u0[1], u2[0] - u0[0], u2[1] - u0[1]);
        let det = du1 * dv2 - du2 * dv1;
        if det.abs() < 1e-12 {
            continue;
        }
        let r = 1.0 / det;
        let t = (e1 * dv2 - e2 * dv1) * r;
        let s = (e2 * du1 - e1 * du2) * r;
        for i in [a, b, c] {
            tan[i] += t;
            bit[i] += s;
        }
    }
    for (i, v) in vertices.iter_mut().enumerate() {
        let n = Vec3::from_array(v.normal);
        let t = (tan[i] - n * n.dot(tan[i])).normalize_or_zero();
        let t = if t == Vec3::ZERO {
            n.any_orthonormal_vector()
        } else {
            t
        };
        // `bit` follows +V, which runs down the image in glTF; normal textures put +Y up it.
        let w = if n.cross(t).dot(bit[i]) < 0.0 { 1.0 } else { -1.0 };
        v.tangent = [t.x, t.y, t.z, w];
    }
}

/// The cooked maps of a material's base colour, normal and occlusion sources (`None` when
/// none of them was cooked).
fn compressed_maps(
    cooked: &HashMap<usize, regia_render::CompressedTexture>,
    [albedo, normal, occlusion]: [Option<usize>; 3],
) -> Option<std::sync::Arc<regia_render::CompressedMaps>> {
    let get = |s: Option<usize>| s.and_then(|s| cooked.get(&s)).cloned();
    let maps = regia_render::CompressedMaps {
        albedo: get(albedo),
        normal: get(normal),
        occlusion: get(occlusion),
        emissive: None,
        ..regia_render::CompressedMaps::default()
    };
    // As `from_cooked` does: levels above the tier's resident cap are dropped at load.
    let maps = maps.fit_to_resident_cap();
    (maps != regia_render::CompressedMaps::default()).then(|| std::sync::Arc::new(maps))
}

/// Largest set-piece texture edge: 1024 (the harness default), or `LOOK_R2_TEXTURE_MAX`
/// (e.g. 4096 = DO27's authored size, for memory measurements). Under the memory tier
/// (`LOOK_R2_STREAM`) the decode also honours the tier's streaming cap, so a Deck never holds
/// CPU copies of levels it cannot make resident.
#[must_use]
pub fn set_piece_texture_max() -> u32 {
    let max = std::env::var("LOOK_R2_TEXTURE_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);
    let cap = crate::look_r2::tier_settings().profile.texture_streaming.max_px;
    if crate::look_r2::memory_tier() && cap > 0 {
        max.min(cap)
    } else {
        max
    }
}

/// Decode threads: the Steam Deck's 8 hardware threads. Each holds one full-size decoded
/// image (64 MiB for a 4 K RGBA PNG), so this also bounds the load's peak RSS.
pub const DECODE_THREADS: usize = 8;

/// [`load_png_scaled`] for every `(key, path)` on up to [`DECODE_THREADS`] cores (scoped
/// threads pulling from a shared index), keyed like the input.
///
/// # Errors
///
/// The first image that failed to open or decode.
pub(crate) fn load_pngs_parallel(
    paths: &[(usize, PathBuf)],
    max: u32,
) -> Result<HashMap<usize, LabTexture>, LookdevError> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .min(DECODE_THREADS)
        .min(paths.len().max(1));
    let results: Vec<Vec<(usize, Result<LabTexture, LookdevError>)>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((key, path)) = paths.get(i) else {
                            break;
                        };
                        out.push((*key, load_png_scaled(path, max)));
                    }
                    out
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| {
                w.join().unwrap_or_else(|_| {
                    vec![(
                        usize::MAX,
                        Err(LookdevError::Png("decode thread panicked".into())),
                    )]
                })
            })
            .collect()
    });
    let mut out = HashMap::with_capacity(paths.len());
    for (key, texture) in results.into_iter().flatten() {
        out.insert(key, texture?);
    }
    Ok(out)
}

/// An 8-bit RGB/RGBA PNG, box-filtered down to at most `max` texels on a side.
pub(crate) fn load_png_scaled(path: &Path, max: u32) -> Result<LabTexture, LookdevError> {
    let file = std::fs::File::open(path).map_err(io(path))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| LookdevError::Png(format!("{}: {e}", path.display())))?;
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| LookdevError::Png(format!("{}: {e}", path.display())))?;
    let (w, h) = (frame.width, frame.height);
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        _ => 1,
    };
    let step = (w.max(h) / max.max(1)).max(1);
    let (ow, oh) = (w / step, h / step);
    let mut rgba = Vec::with_capacity((ow * oh * 4) as usize);
    for oy in 0..oh {
        for ox in 0..ow {
            let mut acc = [0u32; 4];
            for dy in 0..step {
                for dx in 0..step {
                    let i = (((oy * step + dy) * w + ox * step + dx) * channels) as usize;
                    let px = &buf[i..i + channels as usize];
                    let rgb = match channels {
                        1 | 2 => [px[0]; 3],
                        _ => [px[0], px[1], px[2]],
                    };
                    let a = match channels {
                        4 => px[3],
                        2 => px[1],
                        _ => 255,
                    };
                    for c in 0..3 {
                        acc[c] += u32::from(rgb[c]);
                    }
                    acc[3] += u32::from(a);
                }
            }
            let n = step * step;
            rgba.extend(acc.iter().map(|v| (v / n) as u8));
        }
    }
    Ok(LabTexture {
        width: ow,
        height: oh,
        rgba,
    })
}

/// Flips RGBA8 rows left–right in place: the handedness reflection at the image boundary.
pub fn mirror_columns(pixels: &mut [u8], width: u32) {
    let row = width as usize * 4;
    if row == 0 {
        return;
    }
    for line in pixels.chunks_exact_mut(row) {
        let px = line.len() / 4;
        for x in 0..px / 2 {
            let (a, b) = (x * 4, (px - 1 - x) * 4);
            for c in 0..4 {
                line.swap(a + c, b + c);
            }
        }
    }
}

/// The DO27 shell's current terrain cut on a map: the ellipse chain `holes` of
/// `setpieces/hero/holes.json` (Switchbacks: 42 ellipses over the four tunnel mouths), as
/// `shell-ferrite` passes it to the terrain. `None` when the map has no such file.
///
/// # Errors
///
/// The file exists but is not valid JSON or holds an invalid ellipse.
pub fn shell_holes(map_dir: &Path) -> Result<Option<HolesMask>, LookdevError> {
    let path = map_dir.join("setpieces").join("hero").join("holes.json");
    if !path.exists() {
        return Ok(None);
    }
    let json = read_json(&path)?;
    let mut holes = Vec::new();
    for hole in json["holes"].as_array().cloned().unwrap_or_default() {
        let pair = |v: &serde_json::Value, what: &str| -> Result<[f32; 2], LookdevError> {
            match (v[0].as_f64(), v[1].as_f64()) {
                (Some(a), Some(b)) => Ok([a as f32, b as f32]),
                _ => Err(LookdevError::Json(format!("{}: bad {what}", path.display()))),
            }
        };
        holes.push(regia_terrain::HoleEllipse {
            center: pair(&hole["center"], "center")?,
            radius: pair(&hole["radius"], "radius")?,
            rotation_radians: hole["rotation"].as_f64().unwrap_or(0.0) as f32,
        });
    }
    HolesMask::new(holes)
        .map(Some)
        .map_err(|e| LookdevError::Json(format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heightfield_exported_before_the_rename_loads() {
        let dir = regia_test_utils::unique_temp_dir("lookdev-legacy-heightfield");
        std::fs::create_dir_all(dir.join("visual")).expect("visual dir");
        // The key DO27's exporter writes today (the engine's name before the rename).
        let mut meta = serde_json::Map::new();
        meta.insert(
            regia_core::brand::LEGACY_MAIN_CRATE.to_owned(),
            serde_json::json!({ "world_width_m": 64.0, "center_height_m": 512.0 }),
        );
        std::fs::write(
            dir.join("visual/heightfield.json"),
            serde_json::Value::Object(meta).to_string(),
        )
        .expect("json");
        let file = std::fs::File::create(dir.join("visual/heightfield.png")).expect("png");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 2, 2);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        let mut writer = encoder.write_header().expect("header");
        // 32 per metre, 512 m offset: every sample is 1 m above the centre.
        let sample = ((512 + 1) * 32_u16).to_be_bytes();
        writer.write_image_data(&sample.repeat(4)).expect("pixels");
        writer.finish().expect("finish");
        let map = load_heightfield(&dir).expect("a legacy-keyed heightfield loads");
        assert_eq!(map.world_width(), 64.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn quad(u_flip: bool) -> (Vec<LabVertex>, Vec<u32>) {
        let u = |x: f32| if u_flip { 1.0 - x } else { x };
        let v = [
            ([0.0, 0.0, 0.0], [u(0.0), 1.0]),
            ([1.0, 0.0, 0.0], [u(1.0), 1.0]),
            ([1.0, 1.0, 0.0], [u(1.0), 0.0]),
            ([0.0, 1.0, 0.0], [u(0.0), 0.0]),
        ];
        let verts = v
            .iter()
            .map(|&(p, uv)| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], uv))
            .collect();
        (verts, vec![0, 1, 2, 0, 2, 3])
    }

    #[test]
    fn tangents_follow_u_and_carry_the_mirror_sign() {
        let (mut plain, idx) = quad(false);
        compute_tangents(&mut plain, &idx);
        let (mut mirrored, _) = quad(true);
        compute_tangents(&mut mirrored, &idx);
        for (a, b) in plain.iter().zip(&mirrored) {
            assert!((a.tangent[0] - 1.0).abs() < 1e-4, "{:?}", a.tangent);
            assert!((b.tangent[0] + 1.0).abs() < 1e-4, "{:?}", b.tangent);
            // glTF's V runs down the image (+V is world −Y here) and its normal textures put
            // +Y up the image: the bitangent cross(n, t) · w points along +Y in both (the
            // sign flips with the mirror), for MikkTSpace and the fallback alike.
            let bit = |t: [f32; 4]| Vec3::Z.cross(Vec3::new(t[0], t[1], t[2])) * t[3];
            assert!(bit(a.tangent).y > 0.99 && bit(b.tangent).y > 0.99);
        }
        let (mut fallback, _) = quad(false);
        lengyel_tangents(&mut fallback, &idx);
        for v in &fallback {
            let b = Vec3::Z.cross(Vec3::new(v.tangent[0], v.tangent[1], v.tangent[2])) * v.tangent[3];
            assert!(b.y > 0.99, "{:?}", v.tangent);
        }
    }

    #[test]
    fn mirror_columns_reflects_once_and_is_an_involution() {
        // A known asymmetric image: a red marker on the LEFT, a blue marker on the RIGHT.
        let mut px = vec![
            255, 0, 0, 255, //
            0, 255, 0, 255, //
            0, 0, 255, 255, //
            0, 0, 0, 255, //
        ];
        mirror_columns(&mut px, 4);
        // One reflection (the handedness flip `render_shot_with` already applies for DO27
        // content): the red marker moves to the right, so the output matches the path tracer
        // and the sim frame.
        assert_eq!(&px[12..16], &[255, 0, 0, 255]);
        // A second reflection is the identity: the pt_plates double-mirror undid that flip and
        // wrote `rt_ldr.png` left–right mirrored. Dropping the second call keeps one flip.
        mirror_columns(&mut px, 4);
        assert_eq!(&px[0..4], &[255, 0, 0, 255]);
    }
}
