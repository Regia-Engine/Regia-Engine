// SPDX-License-Identifier: MPL-2.0
//! Per-cell lightmap bakes of a streamed world (ADR-3318, `book/agents/lightmaps.md` "Per-cell
//! bakes"): the pieces `regia bake-lightmaps --cells` builds on.
//!
//! - [`scene_subset`]: a cell's traced scene, the cell's instances plus its neighbor ring and the
//!   always-loaded set. The baker's receiver list ([`super::bake_lightmap`]'s `receivers`) is the
//!   occluder-only input: an instance in the scene but not in the list casts shadows and bounces
//!   light, and gets no texels.
//! - [`stitch_cell_border`]: the seam pass between two cells' files. Along every receiver edge that
//!   lies on the shared border (a wall or a floor cut by the cell line, one part in each cell) the
//!   two files' bilinear lookups are made to agree with the least-squares solve of the in-file seam
//!   stitch (ADR-2404, `stitch.rs`), run over the texels of both files at once. Direction texels
//!   use those same rows when both files have a direction section (ADR-4559).
//! - [`CellBakeIndex`]: the `cells.json` index beside the cell files (`format_version` 1): each
//!   cell's key, file and input hash, so an unchanged cell is skipped.
//! - [`CELL_LIGHTMAP_EXPOSURE`]: every cell file is written at one exposure, so a runtime stack of
//!   cells copies texels and never re-encodes one cell because another loaded.

use std::collections::HashMap;

use regia_asset::baked_gi::{from_rgb9e5, rgb9e5};
use regia_asset::lightmap::{Coverage, DEFAULT_HARD_EDGE_DEG, Lightmap, decode_direction, encode_direction};
use serde::{Deserialize, Serialize};

use super::stitch::{
    STITCH_REGULARIZATION, bilinear_taps, clamp_direction_component, conjugate_gradients, solve_seam_channels,
};
use crate::PtScene;

/// The exposure every cell file is written at (a power of two: stored = irradiance × exposure).
/// RGB9E5 keeps 9 bits of mantissa at any scale, so a fixed exposure loses nothing; 1/8 holds up to
/// about 523 000 lux, past any sun.
pub const CELL_LIGHTMAP_EXPOSURE: f32 = 0.125;

/// The version of the cell index (`cells.json`).
pub const CELL_BAKE_INDEX_VERSION: u32 = 1;

/// The file name of the cell index inside a cell directory.
pub const CELL_BAKE_INDEX_FILE: &str = "cells.json";

/// World positions closer than this, meters, are one point on a border.
const BORDER_EPSILON: f32 = 1e-3;
/// Most sample points along one border edge.
const MAX_BORDER_SAMPLES: usize = 64;
/// A face whose normal is this close to the border's axis lies in the border plane (a hidden end
/// cap where two parts meet): its edges are not seams.
const IN_PLANE_NORMAL: f32 = 0.7;
/// The smallest luminance floor of the relative border step, lux.
const STEP_FLOOR: f32 = 1e-3;
/// The relative border step's floor as a share of the border's mean luminance.
const STEP_FLOOR_SHARE: f32 = 0.01;

/// One cell of the index.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CellBakeRecord {
    /// The cell, `"<x>_<z>"`.
    pub key: String,
    /// Cell index along X.
    pub x: i64,
    /// Cell index along Z.
    pub z: i64,
    /// The cell's file, relative to the index.
    pub file: String,
    /// BLAKE3 hex of what the cell's bake read (its receivers, the sun, the sky, the settings).
    pub input_hash: String,
    /// Entries in the file.
    pub entries: usize,
    /// Atlas width, texels.
    pub width: u32,
    /// Atlas height, texels.
    pub height: u32,
}

/// The index of a cell bake (`<dir>/cells.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CellBakeIndex {
    /// [`CELL_BAKE_INDEX_VERSION`].
    pub format_version: u32,
    /// The level the cells came from, as given on the command line.
    pub level: String,
    /// The level's cell side, meters.
    pub cell_size_m: f64,
    /// Neighbor ring width the cells were baked with, cells.
    pub ring: u32,
    /// The exposure every file was written at ([`CELL_LIGHTMAP_EXPOSURE`]).
    pub exposure: f32,
    /// BLAKE3 hex of what the primary file's bake read (the always-loaded set's receivers).
    pub primary_hash: String,
    /// The cells, sorted by `(x, z)`.
    pub cells: Vec<CellBakeRecord>,
}

/// Why a cell index does not read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct CellBakeIndexError {
    /// `lightmap_cells_index` (not JSON or not the shape) or `lightmap_cells_version` (a newer
    /// version).
    pub code: &'static str,
    /// What is wrong.
    pub message: String,
}

impl CellBakeIndex {
    /// Parses an index.
    ///
    /// # Errors
    ///
    /// `lightmap_cells_index` for text that is not an index, `lightmap_cells_version` for a version
    /// this build does not read.
    pub fn parse(text: &str) -> Result<Self, CellBakeIndexError> {
        let version = cell_bake_index_version(text.as_bytes()).ok_or_else(|| CellBakeIndexError {
            code: "lightmap_cells_index",
            message: "no format_version: not a cell index".into(),
        })?;
        if version > CELL_BAKE_INDEX_VERSION || version == 0 {
            return Err(CellBakeIndexError {
                code: "lightmap_cells_version",
                message: format!(
                    "cell index version {version}; this build reads 1..={CELL_BAKE_INDEX_VERSION}"
                ),
            });
        }
        let index: Self = serde_json::from_str(text).map_err(|e| CellBakeIndexError {
            code: "lightmap_cells_index",
            message: e.to_string(),
        })?;
        for c in &index.cells {
            // A plain file name on every platform: no parent, no separator of any OS, no drive.
            if c.key != format!("{}_{}", c.x, c.z)
                || c.file.contains("..")
                || c.file.contains(['/', '\\', ':'])
            {
                return Err(CellBakeIndexError {
                    code: "lightmap_cells_index",
                    message: format!("cell `{}`: key or file does not name the cell", c.key),
                });
            }
        }
        Ok(index)
    }

    /// The index as pretty JSON.
    #[must_use]
    pub fn to_text(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// The record of cell `(x, z)`.
    #[must_use]
    pub fn cell(&self, x: i64, z: i64) -> Option<&CellBakeRecord> {
        self.cells.iter().find(|c| c.x == x && c.z == z)
    }
}

/// The `format_version` of a cell index's bytes, without parsing the rest; `None` when the text is
/// not a JSON object with an unsigned `format_version` (for `regia upgrade-project`).
#[must_use]
pub fn cell_bake_index_version(bytes: &[u8]) -> Option<u32> {
    #[derive(Deserialize)]
    struct Peek {
        format_version: u32,
    }
    serde_json::from_slice::<Peek>(bytes)
        .ok()
        .map(|p| p.format_version)
}

/// The version of a file `regia upgrade-project` found under the `cells.json` name: `Ok(None)` for
/// a file that is not a cell index (another tool's `cells.json`), else its `format_version`.
///
/// # Errors
///
/// Never: a file that does not read as an index is skipped. The `Result` is the registry's shape.
pub fn cell_bake_index_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Ok(None);
    };
    let is_index = ["cell_size_m", "ring", "cells", "exposure"]
        .iter()
        .all(|k| value.get(k).is_some());
    Ok(value
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|_| is_index))
}

/// The schema fingerprint `cargo xtask check-formats` hashes.
#[must_use]
pub fn cell_bake_index_schema_fingerprint() -> String {
    "lightmap-cells: json { format_version:u32 level:string cell_size_m:f64 ring:u32 exposure:f32 \
     primary_hash:string cells:[{ key:string x:i64 z:i64 file:string input_hash:string entries:usize width:u32 height:u32 }] }"
        .to_string()
}

/// The world-space box of instance `k` of `scene` (`None`: no such instance, or no vertex).
#[must_use]
pub fn instance_world_bounds(scene: &PtScene, k: usize) -> Option<([f32; 3], [f32; 3])> {
    let inst = scene.instances.get(k)?;
    let mesh = scene.meshes.get(inst.mesh)?;
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for p in &mesh.positions {
        let w = inst
            .transform
            .transform_point3(regia_core::glam::Vec3::from_array(*p));
        for (a, v) in w.to_array().into_iter().enumerate() {
            lo[a] = lo[a].min(v);
            hi[a] = hi[a].max(v);
        }
    }
    lo[0].is_finite().then_some((lo, hi))
}

/// The scene holding only the instances `keep` lists (in that order, each once), with the meshes
/// they use; lights, sun, sky, terrain and media are kept whole. Instance `keep[i]` is instance `i`
/// of the result (a missing index is left out, so later ones move up).
#[must_use]
pub fn scene_subset(scene: &PtScene, keep: &[usize]) -> PtScene {
    let _span = tracing::info_span!("lightmap_scene_subset", instances = keep.len()).entered();
    let mut mesh_of: HashMap<usize, usize> = HashMap::new();
    let mut meshes = Vec::new();
    let mut instances = Vec::with_capacity(keep.len());
    for &k in keep {
        let Some(inst) = scene.instances.get(k) else {
            continue;
        };
        let Some(mesh) = scene.meshes.get(inst.mesh) else {
            continue;
        };
        let m = *mesh_of.entry(inst.mesh).or_insert_with(|| {
            meshes.push(mesh.clone());
            meshes.len() - 1
        });
        let mut copy = *inst;
        copy.mesh = m;
        instances.push(copy);
    }
    PtScene {
        meshes,
        instances,
        materials: scene.materials.clone(),
        textures: scene.textures.clone(),
        lights: scene.lights.clone(),
        sun: scene.sun,
        // Every cell is lit by all of the scene's suns (ADR-7254), as it is by its sun.
        extra_suns: scene.extra_suns.clone(),
        sky: scene.sky.clone(),
        terrain: scene.terrain.clone(),
        terrain_view_albedo: scene.terrain_view_albedo.clone(),
        sky_bodies: scene.sky_bodies.clone(),
        stars: scene.stars,
        media: scene.media.clone(),
        clouds: scene.clouds,
        camera: scene.camera,
        light_profiles: scene.light_profiles.clone(),
        brdf_energy: scene.brdf_energy,
        surface_graphs: scene.surface_graphs.clone(),
        subsurface: scene.subsurface.clone(),
        decals: scene.decals.clone(),
    }
}

/// The axis a cell border is perpendicular to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BorderAxis {
    /// A border at `x = at` (cells `(x - 1, z)` and `(x, z)`).
    X,
    /// A border at `z = at`.
    Z,
}

impl BorderAxis {
    fn index(self) -> usize {
        match self {
            Self::X => 0,
            Self::Z => 2,
        }
    }
}

/// The line between two edge-adjacent cells.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct CellBorder {
    /// The axis the border is perpendicular to.
    pub axis: BorderAxis,
    /// Its coordinate on that axis, meters.
    pub at: f32,
}

/// The step in the light across a border: at each sample point the two files' bilinear lookups,
/// `|a − b| / max((a + b) / 2, floor)` on luminance, `floor` 1 % of the mean over the border's samples
/// (at least 10⁻³ lux), so near-black faces (the underside of a slab) do not read as a full step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct BorderStep {
    /// Sample points.
    pub samples: usize,
    /// Mean relative step.
    pub mean: f32,
    /// 95th percentile.
    pub p95: f32,
    /// Largest.
    pub max: f32,
}

/// What [`stitch_cell_border`] did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CellSeamReport {
    /// Border edge pairs that share a stretch of the border.
    pub edges: usize,
    /// The step before the solve.
    pub before: BorderStep,
    /// The step after the solve (on the stored, re-encoded texels).
    pub after: BorderStep,
    /// Texels of the first file the solve changed (atlas indices, sorted).
    pub texels_a: Vec<usize>,
    /// Texels of the second file the solve changed.
    pub texels_b: Vec<usize>,
    /// Direction texels of the first file the solve changed (atlas indices, sorted). Empty when
    /// either file has no direction section: that file is left unchanged.
    pub direction_texels_a: Vec<usize>,
    /// Direction texels of the second file the solve changed.
    pub direction_texels_b: Vec<usize>,
}

/// One receiver edge on a border.
#[derive(Clone, Copy)]
struct BorderEdge {
    entry: usize,
    uv: [[f32; 2]; 2],
    world: [[f32; 3]; 2],
    normal: [f32; 3],
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(a: [f32; 3]) -> [f32; 3] {
    let l = dot(a, a).sqrt();
    if l > 0.0 { a.map(|v| v / l) } else { [0.0; 3] }
}

/// Every receiver edge of `map` on `border`, in entry and triangle order. `positions[e]` holds the
/// world positions of entry `e`'s source vertices (empty: the entry has none on the border).
fn border_edges(map: &Lightmap, positions: &[&[[f32; 3]]], border: CellBorder) -> Vec<BorderEdge> {
    let axis = border.axis.index();
    let on = |p: [f32; 3]| (p[axis] - border.at).abs() <= BORDER_EPSILON;
    let mut out = Vec::new();
    for (e, unwrap) in map.unwraps.iter().enumerate() {
        let Some(source) = positions.get(e).copied() else {
            continue;
        };
        let world = |i: u32| {
            unwrap
                .xref
                .get(i as usize)
                .and_then(|&s| source.get(s as usize))
                .copied()
        };
        for tri in unwrap.indices.as_chunks::<3>().0 {
            let (Some(p0), Some(p1), Some(p2)) = (world(tri[0]), world(tri[1]), world(tri[2])) else {
                continue;
            };
            let normal = normalize(cross(sub(p1, p0), sub(p2, p0)));
            if normal[axis].abs() > IN_PLANE_NORMAL {
                continue;
            }
            let ps = [p0, p1, p2];
            for k in 0..3 {
                let (i, j) = (k, (k + 1) % 3);
                if !(on(ps[i]) && on(ps[j])) {
                    continue;
                }
                let d = sub(ps[j], ps[i]);
                if dot(d, d).sqrt() <= BORDER_EPSILON {
                    continue;
                }
                let uv = |v: u32| unwrap.uv.get(v as usize).copied().unwrap_or([0.0; 2]);
                out.push(BorderEdge {
                    entry: e,
                    uv: [uv(tri[i]), uv(tri[j])],
                    world: [ps[i], ps[j]],
                    normal,
                });
            }
        }
    }
    out
}

/// Where `q` falls along the segment `p` (0 at `p[0]`, 1 at `p[1]`), when it lies on its line.
fn param_on(p: [[f32; 3]; 2], q: [f32; 3]) -> Option<f32> {
    let d = sub(p[1], p[0]);
    let len2 = dot(d, d);
    if len2 <= 0.0 {
        return None;
    }
    let t = dot(sub(q, p[0]), d) / len2;
    let foot = [p[0][0] + d[0] * t, p[0][1] + d[1] * t, p[0][2] + d[2] * t];
    let off = sub(q, foot);
    (dot(off, off).sqrt() <= BORDER_EPSILON).then_some(t)
}

/// One sample point: the bilinear taps of both files.
struct Pair {
    a: Vec<(usize, f64)>,
    b: Vec<(usize, f64)>,
}

/// The sample pairs of two files' edges on one border: for each pair of edges sharing a stretch of
/// it with normals within `DEFAULT_HARD_EDGE_DEG`, points at `k + 0.5` of `n`, `n` twice the
/// stretch's length in texels of the denser side (2 to 64).
fn border_pairs(a: &Lightmap, ea: &[BorderEdge], b: &Lightmap, eb: &[BorderEdge]) -> (usize, Vec<Pair>) {
    let cos_limit = DEFAULT_HARD_EDGE_DEG.to_radians().cos();
    let (aw, ah) = (a.width as usize, a.height as usize);
    let (bw, bh) = (b.width as usize, b.height as usize);
    let atlas = |map: &Lightmap, e: usize, uv: [f32; 2]| {
        let s = map.entries[e].scale_offset;
        [uv[0] * s[0] + s[2], uv[1] * s[1] + s[3]]
    };
    let lerp2 = |q: [[f32; 2]; 2], t: f32| {
        [
            q[0][0] + (q[1][0] - q[0][0]) * t,
            q[0][1] + (q[1][1] - q[0][1]) * t,
        ]
    };
    let lerp3 = |q: [[f32; 3]; 2], t: f32| {
        [
            q[0][0] + (q[1][0] - q[0][0]) * t,
            q[0][1] + (q[1][1] - q[0][1]) * t,
            q[0][2] + (q[1][2] - q[0][2]) * t,
        ]
    };
    let texel_len = |map: &Lightmap, e: &BorderEdge| {
        let r = map.entries[e.entry].rect;
        let (dx, dy) = (
            (e.uv[1][0] - e.uv[0][0]) * r[2] as f32,
            (e.uv[1][1] - e.uv[0][1]) * r[3] as f32,
        );
        (dx * dx + dy * dy).sqrt()
    };
    let mut edges = 0;
    let mut pairs = Vec::new();
    for x in ea {
        for y in eb {
            if dot(x.normal, y.normal) < cos_limit {
                continue;
            }
            let (Some(t0), Some(t1)) = (param_on(x.world, y.world[0]), param_on(x.world, y.world[1])) else {
                continue;
            };
            let (lo, hi) = (t0.min(t1).max(0.0), t0.max(t1).min(1.0));
            if hi - lo <= 1e-4 {
                continue;
            }
            edges += 1;
            let span = hi - lo;
            let texels = texel_len(a, x).max(texel_len(b, y)) * span;
            let n = ((2.0 * texels).ceil() as usize).clamp(2, MAX_BORDER_SAMPLES);
            for k in 0..n {
                let t = lo + span * (k as f32 + 0.5) / n as f32;
                let p = lerp3(x.world, t);
                let Some(s) = param_on(y.world, p) else {
                    continue;
                };
                let ta = bilinear_taps(atlas(a, x.entry, lerp2(x.uv, t)), &a.coverage, aw, ah, ah, 0);
                let tb = bilinear_taps(atlas(b, y.entry, lerp2(y.uv, s)), &b.coverage, bw, bh, bh, 0);
                if !ta.is_empty() && !tb.is_empty() {
                    pairs.push(Pair { a: ta, b: tb });
                }
            }
        }
    }
    (edges, pairs)
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn lookup(map: &Lightmap, taps: &[(usize, f64)]) -> [f32; 3] {
    let mut out = [0.0f32; 3];
    for &(i, w) in taps {
        let c = from_rgb9e5(map.texels[i]);
        for (o, v) in out.iter_mut().zip(c) {
            *o += v / map.exposure * w as f32;
        }
    }
    out
}

fn step_of(a: &Lightmap, b: &Lightmap, pairs: &[Pair]) -> BorderStep {
    let sides: Vec<(f32, f32)> = pairs
        .iter()
        .map(|p| (luma(lookup(a, &p.a)), luma(lookup(b, &p.b))))
        .collect();
    let mean = sides.iter().map(|(la, lb)| 0.5 * (la + lb)).sum::<f32>() / sides.len().max(1) as f32;
    let floor = (STEP_FLOOR_SHARE * mean).max(STEP_FLOOR);
    let mut steps: Vec<f32> = sides
        .iter()
        .map(|(la, lb)| (la - lb).abs() / (0.5 * (la + lb)).max(floor))
        .collect();
    if steps.is_empty() {
        return BorderStep::default();
    }
    steps.sort_by(f32::total_cmp);
    let p95 = steps[((steps.len() as f32 * 0.95).ceil() as usize).clamp(1, steps.len()) - 1];
    BorderStep {
        samples: steps.len(),
        mean: steps.iter().sum::<f32>() / steps.len() as f32,
        p95,
        max: steps[steps.len() - 1],
    }
}

/// Measures the border step between two cell files without changing them.
#[must_use]
pub fn measure_cell_border(
    a: &Lightmap,
    a_positions: &[&[[f32; 3]]],
    b: &Lightmap,
    b_positions: &[&[[f32; 3]]],
    border: CellBorder,
) -> BorderStep {
    let ea = border_edges(a, a_positions, border);
    let eb = border_edges(b, b_positions, border);
    let (_, pairs) = border_pairs(a, &ea, b, &eb);
    step_of(a, b, &pairs)
}

/// The seam pass between two edge-adjacent cells (module docs): changes the texels of `a` and `b`
/// the border's lookups read so they agree, weighted toward the baked values, and re-encodes them at
/// each file's own exposure (every other texel keeps its bits). RGB9E5 files only: a file with
/// texel blocks is left alone (the report then has no texels). When both files carry a direction
/// section, the same rows solve the direction texels and write them back in place (the agreement
/// clamped to `0..=1`, as the chart stitch documents). A file with no direction section is left
/// unchanged, and its irradiance matches a directional file's irradiance solve. A block-compressed
/// direction section is stored back as RGBA8 when a direction texel changes; that encoding is
/// already in the `.flmap` header, so the file version stays. Deterministic: edges in entry and
/// triangle order, unknowns numbered as first read.
pub fn stitch_cell_border(
    a: &mut Lightmap,
    a_positions: &[&[[f32; 3]]],
    b: &mut Lightmap,
    b_positions: &[&[[f32; 3]]],
    border: CellBorder,
) -> CellSeamReport {
    let _span = tracing::info_span!("lightmap_cell_seam").entered();
    let ea = border_edges(a, a_positions, border);
    let eb = border_edges(b, b_positions, border);
    let (edges, pairs) = border_pairs(a, &ea, b, &eb);
    let before = step_of(a, b, &pairs);
    let mut report = CellSeamReport {
        edges,
        before,
        after: before,
        ..CellSeamReport::default()
    };
    if pairs.is_empty() || a.blocks.is_some() || b.blocks.is_some() {
        return report;
    }
    // Unknowns: (side, texel), numbered as first read.
    let mut index_of: HashMap<(u8, usize), u32> = HashMap::new();
    let mut unknowns: Vec<(u8, usize)> = Vec::new();
    let mut local = |side: u8, i: usize| {
        let next = unknowns.len() as u32;
        let k = *index_of.entry((side, i)).or_insert(next);
        if k == next {
            unknowns.push((side, i));
        }
        k
    };
    let rows: Vec<Vec<(u32, f64)>> = pairs
        .iter()
        .map(|p| {
            let mut row: Vec<(u32, f64)> = p.a.iter().map(|&(i, w)| (local(0, i), w)).collect();
            for &(i, w) in &p.b {
                row.push((local(1, i), -w));
            }
            row
        })
        .collect();
    let m = unknowns.len();
    let mut diag = vec![STITCH_REGULARIZATION; m];
    for r in &rows {
        for &(i, w) in r {
            diag[i as usize] += w * w;
        }
    }
    let apply = |x: &[f64], out: &mut [f64]| {
        for (o, v) in out.iter_mut().zip(x) {
            *o = STITCH_REGULARIZATION * v;
        }
        for r in &rows {
            let d: f64 = r.iter().map(|&(i, w)| w * x[i as usize]).sum();
            for &(i, w) in r {
                out[i as usize] += w * d;
            }
        }
    };
    let value = |side: u8, i: usize| {
        let map: &Lightmap = if side == 0 { &*a } else { &*b };
        from_rgb9e5(map.texels[i]).map(|v| v / map.exposure)
    };
    let x0: Vec<[f32; 3]> = unknowns.iter().map(|&(s, i)| value(s, i)).collect();
    let solved: Vec<Vec<f64>> = (0..3)
        .map(|c| {
            let start: Vec<f64> = x0.iter().map(|v| f64::from(v[c])).collect();
            let rhs: Vec<f64> = start.iter().map(|v| STITCH_REGULARIZATION * v).collect();
            conjugate_gradients(&apply, &rhs, &start, &diag)
        })
        .collect();
    for (k, &(side, i)) in unknowns.iter().enumerate() {
        let map: &mut Lightmap = if side == 0 { &mut *a } else { &mut *b };
        if map.coverage.get(i).is_none_or(|&c| c == Coverage::EMPTY) {
            continue;
        }
        let v = [0, 1, 2].map(|c| (solved[c][k] as f32).max(0.0));
        if v.iter().any(|x| !x.is_finite()) {
            continue;
        }
        let bits = rgb9e5(v.map(|x| x * map.exposure));
        if bits != map.texels[i] {
            map.texels[i] = bits;
            if side == 0 {
                report.texels_a.push(i);
            } else {
                report.texels_b.push(i);
            }
        }
    }
    report.texels_a.sort_unstable();
    report.texels_b.sort_unstable();
    let (direction_a, direction_b) = stitch_border_directions(a, b, &unknowns, &rows);
    report.direction_texels_a = direction_a;
    report.direction_texels_b = direction_b;
    report.after = step_of(a, b, &pairs);
    report
}

/// Direction texels the border rows read, solved with the chart stitch's rows and written back
/// in place. Empty when either file has no direction section (that bake is unchanged).
fn stitch_border_directions(
    a: &mut Lightmap,
    b: &mut Lightmap,
    unknowns: &[(u8, usize)],
    rows: &[Vec<(u32, f64)>],
) -> (Vec<usize>, Vec<usize>) {
    let fits = |map: &Lightmap| {
        let n = (map.width as usize).saturating_mul(map.height as usize);
        n > 0 && map.directions.len() == n
    };
    if !fits(a) || !fits(b) || unknowns.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let x0: Vec<[f64; 4]> = unknowns
        .iter()
        .map(|&(side, i)| {
            let map = if side == 0 { &*a } else { &*b };
            decode_direction(map.directions[i]).map(f64::from)
        })
        .collect();
    let solved = solve_seam_channels(rows, &x0);
    let mut changed_a = Vec::new();
    let mut changed_b = Vec::new();
    for (k, &(side, i)) in unknowns.iter().enumerate() {
        let map = if side == 0 { &mut *a } else { &mut *b };
        let mut direction = [0.0f32; 4];
        let mut finite = true;
        for c in 0..4 {
            let v = clamp_direction_component(c, solved[k][c] as f32);
            if !v.is_finite() {
                finite = false;
                break;
            }
            direction[c] = v;
        }
        if !finite {
            continue;
        }
        let bits = encode_direction(direction);
        if bits != map.directions[i] {
            map.directions[i] = bits;
            // Compressed blocks cannot take a per-texel edit. RGBA8 is the encoding the baker
            // writes and the format already stores, so the stitched texels replace the blocks.
            map.direction_blocks = None;
            if side == 0 {
                changed_a.push(i);
            } else {
                changed_b.push(i);
            }
        }
    }
    changed_a.sort_unstable();
    changed_b.sort_unstable();
    (changed_a, changed_b)
}

/// Re-encodes `map`'s texels at `exposure` (a power of two), so files of one world share it. A
/// file with texel blocks is decoded to RGB9E5 first. The light does not change beyond RGB9E5
/// rounding; with a power-of-two ratio the mantissas are kept.
pub fn set_exposure(map: &mut Lightmap, exposure: f32) {
    if map.exposure == exposure && map.blocks.is_none() {
        return;
    }
    let scale = exposure / map.exposure;
    for t in &mut map.texels {
        *t = rgb9e5(from_rgb9e5(*t).map(|v| v * scale));
    }
    map.exposure = exposure;
    map.blocks = None;
}

#[cfg(test)]
mod tests {
    use super::super::{LightmapBakeSettings, LightmapScene, bake_lightmap};
    use super::*;
    use crate::{PtCamera, PtMaterial, PtMesh, PtScene, PtSky, PtSun};
    use regia_asset::lightmap::{LightmapBakeInfo, LightmapEntry, LightmapLayout, MeshUnwrap};
    use regia_core::glam::Mat4;

    /// Direction texels within one texel of a cell border must agree within this after the seam.
    ///
    /// The chart stitch allows 0.05 (`the_stitch_makes_both_sides_agree_and_leaves_the_rest`).
    /// That covers the RGBA8 quantum (2/255 on xyz, 1/255 on the agreement) and the residual of
    /// the λ = 0.05 least-squares pull. A disagreement of about 1 fails it.
    const DIRECTION_BORDER_EPSILON: f32 = 0.05;

    /// A floor quad from `x0` to `x0 + 4` at y = 0, its whole 8 × 8 atlas, every texel `lux`
    /// (with a ramp of `ramp` lux a texel along X).
    fn quad_map(lux: f32, ramp: f32) -> Lightmap {
        let mut irr = vec![[0.0f32; 3]; 64];
        for y in 0..8 {
            for x in 0..8 {
                irr[y * 8 + x] = [lux + ramp * x as f32; 3];
            }
        }
        let map = Lightmap::from_irradiance(
            LightmapLayout {
                width: 8,
                height: 8,
                density: 2.0,
                padding: 0,
                entries: vec![LightmapEntry {
                    name: "floor".into(),
                    source_vertices: 4,
                    rect: [0, 0, 8, 8],
                    page: 0,
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 16.0,
                    texels_per_metre: 2.0,
                    key: None,
                    seams: None,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2, 3],
                    uv: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
                    indices: vec![0, 2, 1, 0, 3, 2],
                }],
            },
            &irr,
            vec![Coverage::BAKED; 64],
            LightmapBakeInfo::default(),
        );
        let Ok(mut map) = map else {
            panic!("a valid map");
        };
        set_exposure(&mut map, CELL_LIGHTMAP_EXPOSURE);
        map
    }

    fn positions(x0: f32) -> Vec<[f32; 3]> {
        vec![
            [x0, 0.0, 0.0],
            [x0 + 4.0, 0.0, 0.0],
            [x0 + 4.0, 0.0, 4.0],
            [x0, 0.0, 4.0],
        ]
    }

    /// Two floor quads meeting at x = 4 with different light: the pass lowers the step at the
    /// border, changes only texels next to it, and is deterministic.
    #[test]
    fn the_seam_pass_makes_two_files_agree_at_the_border_and_touches_only_border_texels() {
        let mut a = quad_map(1000.0, 0.0);
        let mut b = quad_map(1400.0, 0.0);
        let (pa, pb) = (positions(0.0), positions(4.0));
        let border = CellBorder {
            axis: BorderAxis::X,
            at: 4.0,
        };
        let before = measure_cell_border(&a, &[&pa[..]], &b, &[&pb[..]], border);
        assert!(before.samples >= 8 && before.p95 > 0.3, "{before:?}");
        let (a0, b0) = (a.clone(), b.clone());
        let r = stitch_cell_border(&mut a, &[&pa[..]], &mut b, &[&pb[..]], border);
        assert_eq!(r.before, before);
        assert!(r.edges >= 1, "{r:?}");
        assert!(r.after.p95 < 0.02, "{r:?}");
        // Only the last column of `a` and the first of `b` are read by the border's lookups.
        assert!(
            !r.texels_a.is_empty() && r.texels_a.iter().all(|i| i % 8 == 7),
            "{r:?}"
        );
        assert!(
            !r.texels_b.is_empty() && r.texels_b.iter().all(|i| i % 8 == 0),
            "{r:?}"
        );
        for (i, (x, y)) in a0.texels.iter().zip(&a.texels).enumerate() {
            assert!(x == y || r.texels_a.contains(&i));
        }
        // Again from the same inputs: the same bits.
        let (mut a2, mut b2) = (a0, b0);
        let r2 = stitch_cell_border(&mut a2, &[&pa[..]], &mut b2, &[&pb[..]], border);
        assert_eq!(
            (a2.texels, b2.texels, r2),
            (a.texels.clone(), b.texels.clone(), r)
        );
    }

    /// Edges away from the border, and quads that only touch it at a corner, are not seams.
    #[test]
    fn quads_apart_from_the_border_give_no_rows() {
        let a = quad_map(1000.0, 0.0);
        let b = quad_map(1400.0, 0.0);
        let (pa, pb) = (positions(0.0), positions(5.0));
        let border = CellBorder {
            axis: BorderAxis::X,
            at: 4.0,
        };
        assert_eq!(
            measure_cell_border(&a, &[&pa[..]], &b, &[&pb[..]], border).samples,
            0
        );
        let z = CellBorder {
            axis: BorderAxis::Z,
            at: 4.0,
        };
        assert_eq!(measure_cell_border(&a, &[&pa[..]], &b, &[&pb[..]], z).samples, 0);
    }

    /// A fixed exposure keeps every value: re-encoding at another power of two and back is
    /// lossless for these texels.
    #[test]
    fn set_exposure_keeps_the_light() {
        let mut map = quad_map(1234.0, 17.0);
        let before: Vec<[f32; 3]> = (0..8).map(|x| map.irradiance_at(x, 3)).collect();
        set_exposure(&mut map, 0.5);
        set_exposure(&mut map, CELL_LIGHTMAP_EXPOSURE);
        let after: Vec<[f32; 3]> = (0..8).map(|x| map.irradiance_at(x, 3)).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn the_index_round_trips_and_refuses_bad_input() {
        let index = CellBakeIndex {
            format_version: CELL_BAKE_INDEX_VERSION,
            level: "assets/levels/w.level.ron".into(),
            cell_size_m: 32.0,
            ring: 1,
            exposure: CELL_LIGHTMAP_EXPOSURE,
            primary_hash: "cd".into(),
            cells: vec![CellBakeRecord {
                key: "-1_2".into(),
                x: -1,
                z: 2,
                file: "-1_2.flmap".into(),
                input_hash: "ab".into(),
                entries: 3,
                width: 64,
                height: 32,
            }],
        };
        let text = index.to_text();
        assert_eq!(CellBakeIndex::parse(&text), Ok(index.clone()));
        assert_eq!(cell_bake_index_version(text.as_bytes()), Some(1));
        let newer = text.replace("\"format_version\": 1", "\"format_version\": 9");
        assert_eq!(
            CellBakeIndex::parse(&newer).map_err(|e| e.code),
            Err("lightmap_cells_version")
        );
        let escape = text.replace("\"-1_2.flmap\"", "\"../x.flmap\"");
        assert_eq!(
            CellBakeIndex::parse(&escape).map_err(|e| e.code),
            Err("lightmap_cells_index")
        );
        for junk in ["", "{", "[]", "{\"format_version\": 1}", "\u{0}"] {
            assert!(CellBakeIndex::parse(junk).is_err(), "{junk:?}");
        }
    }

    /// rc1-rev-render-k 4: a cell's `file` is a plain name on every platform: a Windows separator
    /// or a drive (`dir.join("C:\\x")` replaces `dir` there) is refused like `..` and `/`.
    #[test]
    fn the_index_refuses_windows_paths_in_a_cell_file() {
        let index = CellBakeIndex {
            format_version: CELL_BAKE_INDEX_VERSION,
            level: "assets/levels/w.level.ron".into(),
            cell_size_m: 32.0,
            ring: 1,
            exposure: CELL_LIGHTMAP_EXPOSURE,
            primary_hash: "cd".into(),
            cells: vec![CellBakeRecord {
                key: "0_0".into(),
                x: 0,
                z: 0,
                file: "0_0.flmap".into(),
                input_hash: "ab".into(),
                entries: 1,
                width: 8,
                height: 8,
            }],
        };
        let text = index.to_text();
        assert!(CellBakeIndex::parse(&text).is_ok());
        for bad in [
            "C:\\\\evil.flmap",
            "C:evil.flmap",
            "sub\\\\0_0.flmap",
            "\\\\\\\\host\\\\share",
        ] {
            let changed = text.replace("\"0_0.flmap\"", &format!("\"{bad}\""));
            assert_ne!(changed, text, "{bad}");
            assert_eq!(
                CellBakeIndex::parse(&changed).map_err(|e| e.code),
                Err("lightmap_cells_index"),
                "{bad}"
            );
        }
    }

    fn border_at_four() -> (Vec<[f32; 3]>, Vec<[f32; 3]>, CellBorder) {
        (
            positions(0.0),
            positions(4.0),
            CellBorder {
                axis: BorderAxis::X,
                at: 4.0,
            },
        )
    }

    /// Largest absolute component gap between the border columns (one texel off the cut).
    fn border_direction_gap(a: &Lightmap, b: &Lightmap) -> f32 {
        let mut gap = 0.0f32;
        for y in 0..a.height {
            let (Some(da), Some(db)) = (a.direction_at(a.width - 1, y), b.direction_at(0, y)) else {
                continue;
            };
            for (u, v) in da.iter().zip(db) {
                gap = gap.max((u - v).abs());
            }
        }
        gap
    }

    /// The same 1000-vs-1400 floor the irradiance seam test uses. Directions follow the same rows,
    /// the irradiance bits match that test, and a file with no direction section is unchanged.
    #[test]
    fn directions_follow_the_irradiance_rows_and_a_plain_file_is_unchanged() {
        let (pa, pb, border) = border_at_four();
        let mut plain_a = quad_map(1000.0, 0.0);
        let mut plain_b = quad_map(1400.0, 0.0);
        let plain = stitch_cell_border(&mut plain_a, &[&pa[..]], &mut plain_b, &[&pb[..]], border);
        assert!(plain.after.p95 < 0.02, "{plain:?}");
        assert!(plain.direction_texels_a.is_empty() && plain.direction_texels_b.is_empty());
        assert!(plain.texels_a.iter().all(|i| i % 8 == 7) && plain.texels_b.iter().all(|i| i % 8 == 0));

        let mut a = quad_map(1000.0, 0.0);
        let mut b = quad_map(1400.0, 0.0);
        a.set_directions(&vec![[0.0, 1.0, 0.0, 1.0]; 64])
            .expect("directions");
        b.set_directions(&vec![[1.0, 0.0, 0.0, 0.0]; 64])
            .expect("directions");
        let toward_a = a.direction_at(0, 0).expect("direction");
        let toward_b = b.direction_at(0, 0).expect("direction");
        assert!(
            border_direction_gap(&a, &b) > 0.2,
            "the cells disagree before the seam"
        );
        let report = stitch_cell_border(&mut a, &[&pa[..]], &mut b, &[&pb[..]], border);
        assert_eq!(a.texels, plain_a.texels, "irradiance matches the plain seam");
        assert_eq!(b.texels, plain_b.texels);
        assert_eq!(
            (report.before, report.after, &report.texels_a, &report.texels_b),
            (plain.before, plain.after, &plain.texels_a, &plain.texels_b)
        );
        assert!(
            border_direction_gap(&a, &b) <= DIRECTION_BORDER_EPSILON,
            "{report:?}"
        );
        assert!(
            !report.direction_texels_a.is_empty() && report.direction_texels_a.iter().all(|i| i % 8 == 7),
            "{report:?}"
        );
        assert!(
            !report.direction_texels_b.is_empty() && report.direction_texels_b.iter().all(|i| i % 8 == 0),
            "{report:?}"
        );
        for y in 0..8 {
            assert_eq!(a.direction_at(0, y), Some(toward_a), "column 0 stays");
            assert_eq!(b.direction_at(7, y), Some(toward_b), "column 7 stays");
        }

        // One side with a direction section: that section is not invented on the other side,
        // and the side that has one keeps its bits.
        let mut only_a = quad_map(1000.0, 0.0);
        let mut only_b = quad_map(1400.0, 0.0);
        only_a.set_directions(&vec![toward_a; 64]).expect("directions");
        let kept = only_a.directions.clone();
        let one = stitch_cell_border(&mut only_a, &[&pa[..]], &mut only_b, &[&pb[..]], border);
        assert!(one.direction_texels_a.is_empty() && one.direction_texels_b.is_empty());
        assert_eq!(only_a.directions, kept);
        assert!(only_b.directions.is_empty());
        assert_eq!(only_a.texels, plain_a.texels);
        assert_eq!(only_b.texels, plain_b.texels);
    }

    /// A wall cut by the cell line, each half baked under a sun on the lit side of the wall.
    /// The suns differ in azimuth, so the cells disagree before the seam and agree after it.
    fn bake_wall(x0: f32, toward: [f32; 3], sun_blocker: bool) -> (Lightmap, Vec<[f32; 3]>) {
        let positions = vec![
            [x0, 0.0, 0.0],
            [x0 + 2.0, 0.0, 0.0],
            [x0 + 2.0, 2.0, 0.0],
            [x0, 2.0, 0.0],
        ];
        let mut scene = PtScene::new(PtCamera::pinhole([0.0, 1.0, 4.0], [0.0, 1.0, 0.0], 1.0, 4, 4));
        let mesh = scene.add_mesh(PtMesh {
            positions: positions.clone(),
            normals: vec![[0.0, 0.0, 1.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            ..PtMesh::default()
        });
        let material = scene.add_material(PtMaterial::lambert([0.5; 3]));
        scene.add_instance(mesh, material, Mat4::IDENTITY);
        if sun_blocker {
            // The same world-space occluder in both cell scenes. For toward = [0.85, 0.15, 1],
            // rays from the right half meet it at z = 1; rays from the left half pass beside it.
            let blocker = scene.add_mesh(PtMesh {
                positions: vec![
                    [0.85, -1.0, 1.0],
                    [3.0, -1.0, 1.0],
                    [3.0, 3.0, 1.0],
                    [0.85, 3.0, 1.0],
                ],
                normals: vec![[0.0, 0.0, -1.0]; 4],
                indices: vec![0, 2, 1, 0, 3, 2],
                ..PtMesh::default()
            });
            scene.add_instance(blocker, material, Mat4::IDENTITY);
        }
        scene.sky = PtSky::None;
        let len = toward.iter().map(|c| c * c).sum::<f32>().sqrt();
        scene.sun = Some(PtSun {
            toward: toward.map(|c| c / len),
            color: [1.0; 3],
            illuminance: 10_000.0,
            angular_radius: 0.0,
        });
        let baked = LightmapScene::from_pt_scene(&scene).expect("scene");
        let layout = LightmapLayout {
            width: 8,
            height: 8,
            density: 4.0,
            padding: 0,
            entries: vec![LightmapEntry {
                name: "wall".into(),
                source_vertices: 4,
                rect: [0, 0, 8, 8],
                page: 0,
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 4.0,
                texels_per_metre: 4.0,
                key: None,
                seams: None,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2, 3],
                uv: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
                indices: vec![0, 1, 2, 0, 2, 3],
            }],
        };
        let settings = LightmapBakeSettings {
            samples: 4,
            bounces: 1,
            seed: 1,
            direct_sun: true,
            filter: false,
            threads: 2,
            directional: true,
            denoise: None,
            crease_placement: false,
            stitch_seams: false,
            ..LightmapBakeSettings::default()
        };
        let (map, _) = bake_lightmap(&baked, layout, &[0], &settings, |_, _| {}).expect("bake");
        (map, positions)
    }

    /// Two cells, one wall across the border, a sun on the lit side. After the seam, direction
    /// texels within one texel of the border agree, and irradiance matches the seam of the same
    /// files with the direction section removed.
    #[test]
    fn a_wall_across_the_border_stitches_sun_directions() {
        let (mut a, pa) = bake_wall(-2.0, [0.0, 0.15, 1.0], false);
        let (mut b, pb) = bake_wall(0.0, [0.85, 0.15, 1.0], false);
        assert!(a.is_directional() && b.is_directional());
        let border = CellBorder {
            axis: BorderAxis::X,
            at: 0.0,
        };
        assert!(
            border_direction_gap(&a, &b) > 0.2,
            "the two cells keep different sun directions before the seam"
        );
        let (mut plain_a, mut plain_b) = (a.clone(), b.clone());
        plain_a.directions.clear();
        plain_b.directions.clear();
        plain_a.direction_blocks = None;
        plain_b.direction_blocks = None;
        let plain = stitch_cell_border(&mut plain_a, &[&pa[..]], &mut plain_b, &[&pb[..]], border);
        let report = stitch_cell_border(&mut a, &[&pa[..]], &mut b, &[&pb[..]], border);
        assert_eq!(
            a.texels, plain_a.texels,
            "irradiance matches the seam with no direction section"
        );
        assert_eq!(b.texels, plain_b.texels);
        assert_eq!(
            (report.before, report.after, &report.texels_a, &report.texels_b),
            (plain.before, plain.after, &plain.texels_a, &plain.texels_b)
        );
        assert!(plain.direction_texels_a.is_empty() && plain.direction_texels_b.is_empty());
        assert!(report.edges >= 1, "{report:?}");
        assert!(
            border_direction_gap(&a, &b) <= DIRECTION_BORDER_EPSILON,
            "border directions {:?} vs epsilon {DIRECTION_BORDER_EPSILON}",
            border_direction_gap(&a, &b)
        );
        assert!(!report.direction_texels_a.is_empty() && !report.direction_texels_b.is_empty());
        // The border lookup reads only the last column of `a` and the first of `b`.
        assert!(
            report
                .direction_texels_a
                .iter()
                .all(|i| i % a.width as usize == a.width as usize - 1)
        );
        assert!(
            report
                .direction_texels_b
                .iter()
                .all(|i| i % b.width as usize == 0)
        );
    }

    /// One sun and one wall shared by two cells: the same occluder shadows the right half.
    /// Direction texels and irradiance at the cut agree after the seam despite that contrast.
    #[test]
    fn one_sun_and_a_shared_occluder_stitch_the_wall_border() {
        let sun = [0.85, 0.15, 1.0];
        let (mut a, pa) = bake_wall(-2.0, sun, true);
        let (mut b, pb) = bake_wall(0.0, sun, true);
        assert!(a.is_directional() && b.is_directional());
        assert!(
            border_direction_gap(&a, &b) > 0.2,
            "the fixture must exercise a direction seam"
        );
        let report = stitch_cell_border(
            &mut a,
            &[&pa[..]],
            &mut b,
            &[&pb[..]],
            CellBorder {
                axis: BorderAxis::X,
                at: 0.0,
            },
        );
        assert!(report.edges >= 1 && report.before.p95 > 0.2, "{report:?}");
        assert!(report.after.p95 < 0.05, "{report:?}");
        assert!(
            border_direction_gap(&a, &b) <= DIRECTION_BORDER_EPSILON,
            "border direction gap {}: {report:?}",
            border_direction_gap(&a, &b)
        );
        assert!(!report.direction_texels_a.is_empty() && !report.direction_texels_b.is_empty());
    }
}
