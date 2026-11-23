// SPDX-License-Identifier: MIT-0
//! Tunnels armed in a showcase scene (ADR-8825): the scene's mesh layers cooked at load time, the
//! mouths cut through the drawn terrain, the tubes drawn and lit, and the indoor cells handed to the
//! renderer's portal cull.
//!
//! The switch is `tunnels/tunnels.ron` in the scene directory ([`TUNNELS_FILE`]). It names a mesh-layers
//! document (`<map>.meshlayers.ron`, spec 121 § 9) and the passages file that document reads, both inside
//! `tunnels/`. Without the file the scene loads and renders as before.
//!
//! At load ([`load_tunnels`], CPU only, no GPU):
//!
//! 1. The scene heightfield becomes the cook's ground lattice: one count per sample,
//!    `count = (height - ground_offset_m) / ground_step_m`. The heightfield must be square and every
//!    sample must sit on a count (within a quarter step), or the load fails with `tunnels_ground_mismatch`.
//! 2. [`regia_world_geom::cook::cook`] cooks the document: the canonical section (the sim's world
//!    geometry), the presentation section (the tube render mesh), the mouth cut and the indoor cells.
//! 3. Each cooked mouth becomes an open named hole of the terrain's [`HolesMask`] (its lattice rings) and a
//!    [`LayerMouth`] of a [`TerrainMeshLayers`], whose [`TerrainMeshLayers::lattice_hole_cut`] is the
//!    lattice cut the terrain draw discards (ADR-8470): the same cut the sim's ground has.
//! 4. The presentation triangles become one mesh per passage (flat normals, box-projected UVs); the
//!    tube and gallery centrelines get a point light every `lights.spacing_m`.
//!
//! The host ([`crate::app`]) puts the holes and the lattice cut on the terrain draw, keeps the scatter out
//! of the holes, spawns the tube meshes and lights, inserts the cooked [`VisCells`] (so the renderer's
//! portal cull drops the outdoor terrain and scatter seen from inside a sealed tube) and spawns the
//! [`TerrainMeshLayers`] for the agent's `terrain.*` cooked-geometry methods.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_render::viscells::VisCells;
use regia_render::{CpuMesh, LabVertex};
use regia_terrain::{HoleShape, HolesMask, LayerMouth, NamedHole, TerrainMap, TerrainMeshLayers};
use regia_world_geom::WorldGeom;
use regia_world_geom::compat::layout::{PassageKind, SampledLayout};
use regia_world_geom::compat::sampler::{HEIGHT_METERS_PER_COUNT, HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookInputs, GroundInput, cook};
use regia_world_geom::cooked::split_file;
use regia_world_geom::ground::vertex_m;
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::render::{LayerRender, PresentationCooked};
use serde::{Deserialize, Serialize};

use crate::{ErrorCode, ShowcaseError};

/// The switch file, relative to the scene directory.
pub const TUNNELS_FILE: &str = "tunnels/tunnels.ron";
/// The directory the tunnel files live in, relative to the scene directory.
pub const TUNNELS_DIR: &str = "tunnels";
/// Format version of `tunnels/tunnels.ron`.
pub const TUNNELS_FILE_VERSION: u32 = 1;
/// Largest tunnel input file read (the document, the passages file, `tunnels.ron`), bytes.
pub const MAX_TUNNEL_FILE_BYTES: u64 = 16 << 20;
/// Samples a side of the Stage A sampler the layout's opening heights are read from (DO27's 5 m sim grid
/// over a 1280 m map).
pub const SAMPLER_SIDE: usize = 256;
/// The Stage A sampler's raw count of height zero (`TerrainSampler::with_zero_counts`).
const SAMPLER_ZERO_COUNTS: u32 = 16_384;

/// The tube light rig of `tunnels.ron`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TubeLights {
    /// Metres between two lights along a tube or gallery centreline (`0` = no lights).
    pub spacing_m: f32,
    /// Height of a light over the passage floor, metres.
    pub height_m: f32,
    /// Luminous power of one light, lumens.
    pub intensity_lm: f32,
    /// Cutoff radius of one light, metres.
    pub radius_m: f32,
    /// Linear RGB colour.
    pub color: (f32, f32, f32),
}

/// The parsed `tunnels/tunnels.ron`.
///
/// ```text
/// (
///     version: 1,
///     document: "switchbacks.meshlayers.ron",  // the mesh-layers document, inside tunnels/
///     ground_offset_m: -512.0,                 // height of ground count 0
///     ground_step_m: 0.03125,                  // metres per ground count
///     lights: (spacing_m: 40.0, height_m: 9.0, intensity_lm: 60000.0, radius_m: 36.0, color: (1.0, 0.78, 0.55)),
///     rock: (0.42, 0.27, 0.17),                // tube rock albedo (linear)
/// )
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelsFile {
    /// Must be [`TUNNELS_FILE_VERSION`].
    pub version: u32,
    /// The mesh-layers document, relative to `tunnels/`. Its layers' `Passages(path)` are relative to
    /// `tunnels/` too.
    pub document: String,
    /// Height of ground count 0, metres (the cook's `offset_m`).
    pub ground_offset_m: f32,
    /// Metres per ground count (the cook's `step_m`).
    pub ground_step_m: f32,
    /// The tube light rig.
    pub lights: TubeLights,
    /// Tube rock albedo, linear RGB.
    pub rock: (f32, f32, f32),
}

impl TunnelsFile {
    /// Parses and validates `tunnels.ron`.
    ///
    /// # Errors
    /// [`ErrorCode::TunnelsMalformed`]: bad RON, a wrong version, an unsafe path or an out-of-range number.
    pub fn from_ron(text: &str, path: &Path) -> Result<Self, ShowcaseError> {
        let bad = |message: String| ShowcaseError::new(ErrorCode::TunnelsMalformed, path, message);
        let file: Self = ron::from_str(text).map_err(|e| bad(e.to_string()))?;
        if file.version != TUNNELS_FILE_VERSION {
            return Err(bad(format!(
                "version {} (this build reads {TUNNELS_FILE_VERSION})",
                file.version
            )));
        }
        relative(&file.document).ok_or_else(|| {
            bad(format!(
                "`document` {:?} is not a relative path inside tunnels/",
                file.document
            ))
        })?;
        let positive = |v: f32| v.is_finite() && v > 0.0;
        if !file.ground_offset_m.is_finite() || !positive(file.ground_step_m) {
            return Err(bad(
                "`ground_offset_m` must be finite and `ground_step_m` > 0".into()
            ));
        }
        let l = &file.lights;
        let colour = [
            l.color.0,
            l.color.1,
            l.color.2,
            file.rock.0,
            file.rock.1,
            file.rock.2,
        ];
        if !(l.spacing_m == 0.0 || (positive(l.spacing_m) && l.spacing_m >= 4.0))
            || !l.height_m.is_finite()
            || !(l.intensity_lm.is_finite() && l.intensity_lm >= 0.0)
            || !positive(l.radius_m)
            || colour.iter().any(|c| !(c.is_finite() && (0.0..=1.0).contains(c)))
        {
            return Err(bad(
                "lights: spacing_m 0 or >= 4, finite height_m, intensity_lm >= 0, radius_m > 0, colours in 0..=1"
                    .into(),
            ));
        }
        Ok(file)
    }
}

/// `rel` when it is a non-empty relative path without `..`.
fn relative(rel: &str) -> Option<&Path> {
    let path = Path::new(rel);
    let safe = !rel.is_empty()
        && path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    safe.then_some(path)
}

/// One cooked mouth, as the host reports it (`tunnels.state`).
#[derive(Clone, Debug, PartialEq)]
pub struct MouthSummary {
    /// The named hole id (the opening id).
    pub id: String,
    /// Ring loops.
    pub rings: usize,
    /// Ring points over all loops.
    pub ring_points: usize,
    /// Lattice triangles the cook cut.
    pub cut_triangles: u32,
    /// The ring points' centre, world metres.
    pub centre: [f32; 3],
}

/// One tube mesh: the presentation triangles of one passage of one layer.
#[derive(Clone)]
pub struct TubeMesh {
    /// `<layer>/<passage>`.
    pub name: String,
    /// Flat-shaded triangles, world metres.
    pub mesh: Arc<CpuMesh>,
}

/// What [`load_tunnels`] built.
#[derive(Clone)]
pub struct TunnelsPlan {
    /// The parsed `tunnels.ron`.
    pub file: TunnelsFile,
    /// The document path.
    pub document: PathBuf,
    /// The cooked world geometry (canonical section).
    pub geom: Arc<WorldGeom>,
    /// The cooked geometry and its open mouths, for the lattice cut and the agent's `terrain.*` methods.
    pub layers: TerrainMeshLayers,
    /// One open named hole per mouth (its lattice rings).
    pub holes: HolesMask,
    /// The lattice cut of the open mouths on the scene's heightfield (`None` when the cooked lattice is not
    /// the heightfield's grid).
    pub lattice: Option<regia_terrain::LatticeHoleCut>,
    /// Every cooked mouth.
    pub mouths: Vec<MouthSummary>,
    /// The tube meshes.
    pub meshes: Vec<TubeMesh>,
    /// Presentation triangles over every mesh.
    pub triangles: usize,
    /// Tube light positions, world metres.
    pub lights: Vec<[f32; 3]>,
    /// The cooked indoor cells (outdoor cell 0, one per section).
    pub viscells: VisCells,
    /// Bytes of the canonical section (spec 121 § 11's sim-geometry measure).
    pub sim_bytes: u64,
    /// The cooked file's size, bytes.
    pub cooked_bytes: usize,
}

impl std::fmt::Debug for TunnelsPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelsPlan")
            .field("document", &self.document)
            .field("mouths", &self.mouths.len())
            .field("meshes", &self.meshes.len())
            .field("triangles", &self.triangles)
            .field("lights", &self.lights.len())
            .field("cells", &self.viscells.cells.len())
            .field("lattice_cut", &self.lattice.is_some())
            .finish_non_exhaustive()
    }
}

fn read_capped(path: &Path, code: ErrorCode) -> Result<String, ShowcaseError> {
    let len = std::fs::metadata(path)
        .map_err(|e| ShowcaseError::new(code, path, e.to_string()))?
        .len();
    if len > MAX_TUNNEL_FILE_BYTES {
        return Err(ShowcaseError::new(
            code,
            path,
            format!("{len} bytes (at most {MAX_TUNNEL_FILE_BYTES})"),
        ));
    }
    std::fs::read_to_string(path).map_err(|e| ShowcaseError::new(code, path, e.to_string()))
}

/// The cook's ground lattice from the scene heightfield: one count per sample.
///
/// # Errors
/// [`ErrorCode::TunnelsGroundMismatch`] when the heightfield is not square or a sample does not sit on a count.
pub fn ground_input(map: &TerrainMap, file: &TunnelsFile, path: &Path) -> Result<GroundInput, ShowcaseError> {
    let bad = |message: String| ShowcaseError::new(ErrorCode::TunnelsGroundMismatch, path, message);
    let n = map.width();
    if n != map.height() || n < 2 {
        return Err(bad(format!(
            "the heightfield is {}x{}; the ground lattice is square",
            map.width(),
            map.height()
        )));
    }
    let step = f64::from(file.ground_step_m);
    let offset = f64::from(file.ground_offset_m);
    let mut counts = Vec::with_capacity(map.heights().len());
    for (index, &h) in map.heights().iter().enumerate() {
        let exact = (f64::from(h) - offset) / step;
        let count = exact.round();
        if !(0.0..=65535.0).contains(&count) || (exact - count).abs() > 0.25 {
            let (col, row) = (index as u32 % n, index as u32 / n);
            return Err(bad(format!(
                "sample ({col}, {row}) at {h} m is not on a ground count (offset {offset} m, step {step} m)"
            )));
        }
        counts.push(count as u16);
    }
    Ok(GroundInput {
        lattice_n: n,
        width_m: map.world_width(),
        offset_m: file.ground_offset_m,
        step_m: file.ground_step_m,
        counts,
    })
}

/// The Stage A sampler the layout reads its opening heights from: [`SAMPLER_SIDE`]² samples of the scene
/// heightfield at DO27's `i / n` grid, `HEIGHT_METERS_PER_COUNT` a count.
#[must_use]
pub fn stage_a_sampler(map: &TerrainMap) -> TerrainSampler {
    let width = map.world_width();
    let cell = width / SAMPLER_SIDE as f32;
    let mut counts = Vec::with_capacity(SAMPLER_SIDE * SAMPLER_SIDE);
    for row in 0..SAMPLER_SIDE {
        for col in 0..SAMPLER_SIDE {
            let x = -width * 0.5 + col as f32 * cell;
            let z = -width * 0.5 + row as f32 * cell;
            let h = map.sample_bilinear(x, z);
            let count = (h / HEIGHT_METERS_PER_COUNT).round() + SAMPLER_ZERO_COUNTS as f32;
            counts.push(count.clamp(0.0, 65535.0) as u16);
        }
    }
    // `square` refuses only a side under 2 or a wrong length, neither of which happens here.
    let grid = HeightGrid::square(SAMPLER_SIDE, counts).unwrap_or(HeightGrid {
        width: 2,
        height: 2,
        heights: vec![SAMPLER_ZERO_COUNTS as u16; 4],
    });
    TerrainSampler::new(grid, width).with_zero_counts(SAMPLER_ZERO_COUNTS)
}

/// Reads `tunnels/tunnels.ron` when the scene has one and cooks its tunnels on `map`.
///
/// # Errors
/// [`ErrorCode::TunnelsMalformed`] (the switch file, the document or the passages file),
/// [`ErrorCode::TunnelsGroundMismatch`] or [`ErrorCode::TunnelsCookFailed`] (the cook's own code in the
/// message).
pub fn load_tunnels(dir: &Path, map: &TerrainMap) -> Result<Option<TunnelsPlan>, ShowcaseError> {
    let path = dir.join(TUNNELS_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let _span = tracing::info_span!("showcase_load_tunnels").entered();
    let file = TunnelsFile::from_ron(&read_capped(&path, ErrorCode::TunnelsMalformed)?, &path)?;
    let tunnels_dir = dir.join(TUNNELS_DIR);
    let document_path = tunnels_dir.join(&file.document);
    let document = read_capped(&document_path, ErrorCode::TunnelsMalformed)?;
    let ground = ground_input(map, &file, &path)?;
    let sampler = stage_a_sampler(map);
    // The document's inputs resolve inside tunnels/.
    let read_file = |rel: &str| -> Result<Vec<u8>, String> {
        let Some(rel_path) = relative(rel) else {
            return Err(format!("{rel:?} is not a relative path inside tunnels/"));
        };
        read_capped(&tunnels_dir.join(rel_path), ErrorCode::TunnelsMalformed)
            .map(String::into_bytes)
            .map_err(|e| e.message)
    };
    let out = cook(&CookInputs {
        document: document.as_bytes(),
        ground: &ground,
        terrain: Some(&sampler),
        read_file: &read_file,
    })
    .map_err(|e| {
        ShowcaseError::new(
            ErrorCode::TunnelsCookFailed,
            &document_path,
            format!("{} ({})", e, e.code()),
        )
    })?;
    let cook_failed =
        |message: String| ShowcaseError::new(ErrorCode::TunnelsCookFailed, &document_path, message);
    let bytes = out.file.encode().map_err(|e| cook_failed(e.to_string()))?;
    let geom =
        Arc::new(WorldGeom::from_cooked(&bytes).map_err(|e| cook_failed(format!("{e} ({})", e.code())))?);
    let parts = split_file(&bytes).map_err(|e| cook_failed(e.to_string()))?;
    let presentation =
        PresentationCooked::decode(&parts.presentation).map_err(|e| cook_failed(e.to_string()))?;
    let viscells =
        VisCells::from_ron(&out.viscells_ron).map_err(|e| cook_failed(format!("viscells: {e}")))?;

    // Mouths: the holes the terrain draws and the lattice cut.
    let lattice = out.file.canonical.lattice;
    let mut holes = HolesMask::default();
    let mut layers = TerrainMeshLayers::with_geom(Arc::clone(&geom));
    let mut mouths = Vec::new();
    for mouth in &out.report.ground.mouths {
        let loops: Vec<Vec<[f32; 3]>> = mouth
            .rings
            .iter()
            .map(|ring| {
                ring.iter()
                    .map(|&(i, j)| {
                        let (col, row) = (i64::from(i), i64::from(j));
                        [
                            vertex_m(&lattice, i),
                            map.height_at_sample(col, row),
                            vertex_m(&lattice, j),
                        ]
                    })
                    .collect()
            })
            .collect();
        let flat: Vec<Vec<[f32; 2]>> = loops
            .iter()
            .map(|l| l.iter().map(|p| [p[0], p[2]]).collect())
            .collect();
        let shape = HoleShape::lattice_rings(&flat).ok_or_else(|| {
            cook_failed(format!(
                "mouth `{}`: a ring with fewer than three points",
                mouth.id
            ))
        })?;
        holes
            .set_named(NamedHole {
                id: mouth.id.clone(),
                open: true,
                shape,
            })
            .map_err(|e| cook_failed(format!("mouth `{}`: {e}", mouth.id)))?;
        let points: Vec<[f32; 3]> = loops.iter().flatten().copied().collect();
        let count = points.len().max(1) as f32;
        let sum = points
            .iter()
            .fold([0.0f32; 3], |a, p| [a[0] + p[0], a[1] + p[1], a[2] + p[2]]);
        mouths.push(MouthSummary {
            id: mouth.id.clone(),
            rings: loops.len(),
            ring_points: points.len(),
            cut_triangles: mouth.cut_triangles,
            centre: sum.map(|c| c / count),
        });
        layers.push_mouth(LayerMouth {
            hole: mouth.id.clone(),
            rings: loops,
            band: regia_terrain::BandMesh::default(),
            collar: Vec::new(),
            throat: Vec::new(),
            body: Vec::new(),
            open: true,
        });
    }
    let lattice_cut = layers.lattice_hole_cut(map);

    // Tube meshes and the light rig.
    let mut meshes = Vec::new();
    let mut triangles = 0;
    for layer in &presentation.layers {
        for (name, mesh) in passage_meshes(layer) {
            triangles += mesh.indices.len() / 3;
            meshes.push(TubeMesh {
                name,
                mesh: Arc::new(mesh),
            });
        }
    }
    let mut lights = Vec::new();
    if file.lights.spacing_m > 0.0 {
        let mut documents = Vec::new();
        let doc = regia_world_geom::MeshLayersDocument::from_ron(&document)
            .map_err(|e| cook_failed(format!("{e} ({})", e.code())))?;
        for layer in &doc.layers {
            if let regia_world_geom::meshlayers::LayerSource::Passages { path } = &layer.source {
                let text = read_file(path).map_err(cook_failed)?;
                let text = String::from_utf8(text).map_err(|e| cook_failed(e.to_string()))?;
                let passages = PassagesDocument::from_ron(&text).map_err(|e| {
                    ShowcaseError::new(
                        ErrorCode::TunnelsMalformed,
                        &tunnels_dir.join(path),
                        e.to_string(),
                    )
                })?;
                documents.push(passages);
            }
        }
        for passages in &documents {
            let sampled = passages.layout.sample(Some(&sampler));
            lights.extend(light_rig(&sampled, &file.lights));
        }
    }
    Ok(Some(TunnelsPlan {
        file,
        document: document_path,
        geom,
        layers,
        holes,
        lattice: lattice_cut,
        mouths,
        meshes,
        triangles,
        lights,
        viscells,
        sim_bytes: out.report.ground.sim_bytes,
        cooked_bytes: bytes.len(),
    }))
}

/// A light every `spacing_m` of arc length along each covered tube and gallery centreline, `height_m` over
/// the floor; none on ramps (they open to the sky).
#[must_use]
pub fn light_rig(sampled: &SampledLayout, rig: &TubeLights) -> Vec<[f32; 3]> {
    let mut out = Vec::new();
    if rig.spacing_m <= 0.0 {
        return out;
    }
    for passage in &sampled.passages {
        if !matches!(passage.kind, PassageKind::Tube | PassageKind::Gallery) {
            continue;
        }
        let mut next = rig.spacing_m * 0.5;
        for (k, pair) in passage.samples.windows(2).enumerate() {
            let [a, b] = [&pair[0], &pair[1]];
            if !passage.covered.get(k).copied().unwrap_or(true) {
                next = next.max(b.s);
                continue;
            }
            while next <= b.s {
                let t = if b.s > a.s {
                    (next - a.s) / (b.s - a.s)
                } else {
                    0.0
                };
                let t = t.clamp(0.0, 1.0);
                let p = [
                    a.p[0] + (b.p[0] - a.p[0]) * t,
                    a.p[1] + (b.p[1] - a.p[1]) * t + rig.height_m,
                    a.p[2] + (b.p[2] - a.p[2]) * t,
                ];
                out.push(p);
                next += rig.spacing_m;
            }
        }
    }
    out
}

fn m(p: [i32; 3]) -> [f32; 3] {
    p.map(|c| c as f32 / 1000.0)
}

/// Box projection: the two axes across the face's dominant normal axis, in UV tiles.
fn box_uv(p: [f32; 3], n: [f32; 3], tile: f32) -> [f32; 2] {
    let (ax, ay, az) = (n[0].abs(), n[1].abs(), n[2].abs());
    if ay >= ax && ay >= az {
        [p[0] / tile, p[2] / tile]
    } else if ax >= az {
        [p[2] / tile, p[1] / tile]
    } else {
        [p[0] / tile, p[1] / tile]
    }
}

/// A deterministic 0..1 hash of a millimetre position (a light per-vertex tint over the rock).
fn hash01(p: [i32; 3]) -> f32 {
    let mut h: u32 = 0x811c_9dc5;
    for c in p {
        for b in c.to_le_bytes() {
            h ^= u32::from(b);
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// One flat-shaded mesh per passage range of `layer` (`<layer>/<passage>`); triangles outside every range
/// (none in a valid section) go to `<layer>/rest`.
#[must_use]
pub fn passage_meshes(layer: &LayerRender) -> Vec<(String, CpuMesh)> {
    let tile = f32::from_bits(layer.uv_tile_m_bits);
    let tile = if tile.is_finite() && tile > 0.0 { tile } else { 4.0 };
    let triangles = layer.triangles_mm();
    let mut covered = vec![false; triangles.len()];
    let mut ranges: Vec<(String, std::ops::Range<usize>)> = Vec::new();
    for passage in &layer.passages {
        let start = (passage.first as usize).min(triangles.len());
        let end = (passage.first as usize + passage.count as usize).min(triangles.len());
        covered[start..end].iter_mut().for_each(|c| *c = true);
        ranges.push((format!("{}/{}", layer.id, passage.id), start..end));
    }
    let rest: Vec<usize> = (0..triangles.len()).filter(|i| !covered[*i]).collect();
    let mut out = Vec::new();
    let mut build = |name: String, indices: &mut dyn Iterator<Item = usize>| {
        let mut vertices = Vec::new();
        for i in indices {
            let t = triangles[i];
            let p = t.map(m);
            let a = regia_core::math::Vec3::from_array(p[1]) - regia_core::math::Vec3::from_array(p[0]);
            let b = regia_core::math::Vec3::from_array(p[2]) - regia_core::math::Vec3::from_array(p[0]);
            let n = a.cross(b).normalize_or_zero().to_array();
            for (k, q) in p.iter().enumerate() {
                let mut v = LabVertex::from_pos_normal_uv(*q, n, box_uv(*q, n, tile));
                let s = 0.8 + 0.2 * hash01(t[k]);
                v.color = [s, s, s, 1.0];
                vertices.push(v);
            }
        }
        if !vertices.is_empty() {
            let indices = (0..vertices.len() as u32).collect();
            out.push((name, CpuMesh { vertices, indices }));
        }
    };
    for (name, mut range) in ranges {
        build(name, &mut range);
    }
    build(format!("{}/rest", layer.id), &mut rest.into_iter());
    out
}
