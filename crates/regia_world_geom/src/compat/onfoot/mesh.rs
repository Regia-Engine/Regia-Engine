// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pilot_mesh.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-578, mark: adapt (Stage A: unchanged); tests 579-747 (asserts unchanged; the three that read the
// game's asset store stay the source's, see the switch note in spec 121 § 4.2).
// Edits: module paths, doc comments (DO27 file, map, plan and engine names dropped, missing docs added); the
// spec 121 § 4.2 seams: the file reads behind `GltfGridSource` (load time only), the set-piece and building
// lists are the host's (`BUILDING_MESHES`, the map directory walk and the rock props stay the source's),
// `OnFootParams`; `eprintln!` is `tracing::warn!`; float min/max through `regia_detsim::float::det_*`
// (ADR-8166); slice-to-array `unwrap`s written as byte arrays; the span-cap count merges with the grids;
// the `load_gltf_grid_keep` filter seam (W-30, ADR-8323: a drawn triangle filter, no arithmetic change).

//! The on-foot grid from authored glTF, without a glTF crate (load time only).
//!
//! Set pieces and hero shells ship as glTF + a binary buffer. The drawn mesh is not a collider. This
//! loader keeps the steep faces and the raised floors on the coarse [`super::CELL_M`] grid the on-foot
//! capsule tests. Vehicles never see that grid. Only the chosen LOD is read out of the buffer, so a
//! 200 MB portal file does not have to sit in memory.
//!
//! T3: on a map with mesh layers a drawn floor within [`super::FLOOR_MATCH_M`] of any sim floor (the
//! heightfield, or a tunnel floor under it) is not stamped, so a walker stands on the one surface the
//! sim answers there instead of a 0.75 m staircase of the drawn lining floor.
//!
//! The bytes come through [`GltfGridSource`]: [`GltfFile`] reads a glTF and its buffer from disk,
//! [`GltfMemory`] serves them from memory. No query path reads a file.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use regia_detsim::float::{det_max, det_min};

use super::{OnFootParams, PilotGrid, PilotSolids};
use crate::compat::sampler::TerrainSampler;
use crate::compat::state::TunnelsState;

/// Largest LOD bucket (triangles) still used when a coarser one exists.
/// A merged LOD2 of about 86k fits; a next LOD over 110k does not.
const LOD_BUDGET: usize = 100_000;
/// A chosen shell above this is sampled. A portal's LOD2 sits under it.
const STAMP_CAP: usize = 250_000;

/// Where one glTF's bytes come from (spec 121 § 4.2 seam for the source's `std::fs` reads). Load
/// time only: the grid is built once per map, never on a tick.
pub trait GltfGridSource: Sync {
    /// A name for log lines (a path, an asset id).
    fn label(&self) -> String;
    /// The glTF JSON text.
    fn gltf_text(&self) -> Result<String, String>;
    /// `len` bytes at byte offset `start` of the buffer named `uri` (the glTF's `buffers[0].uri`).
    fn buffer_range(&self, uri: &str, start: u64, len: usize) -> Result<Vec<u8>, String>;
}

/// A glTF on disk; its buffer is the file `uri` beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GltfFile {
    /// Path of the `.gltf` file.
    pub path: PathBuf,
}

impl GltfFile {
    /// The glTF at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl GltfGridSource for GltfFile {
    fn label(&self) -> String {
        self.path.display().to_string()
    }

    fn gltf_text(&self) -> Result<String, String> {
        std::fs::read_to_string(&self.path).map_err(|err| err.to_string())
    }

    fn buffer_range(&self, uri: &str, start: u64, len: usize) -> Result<Vec<u8>, String> {
        let bin_path = self.path.parent().unwrap_or(&self.path).join(uri);
        let mut file = File::open(&bin_path).map_err(|err| format!("{}: {err}", bin_path.display()))?;
        file.seek(SeekFrom::Start(start)).map_err(|err| err.to_string())?;
        let mut bytes = vec![0u8; len];
        file.read_exact(&mut bytes).map_err(|err| err.to_string())?;
        Ok(bytes)
    }
}

/// A glTF and its buffers in memory (a host that streams assets, or a test fixture).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GltfMemory {
    /// A name for log lines.
    pub label: String,
    /// The glTF JSON text.
    pub text: String,
    /// Buffers by `uri`.
    pub buffers: BTreeMap<String, Vec<u8>>,
}

impl GltfMemory {
    /// A glTF named `label` with JSON `text` and no buffers yet.
    pub fn new(label: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            text: text.into(),
            buffers: BTreeMap::new(),
        }
    }

    /// Add the buffer `uri`.
    #[must_use]
    pub fn with_buffer(mut self, uri: impl Into<String>, bytes: Vec<u8>) -> Self {
        self.buffers.insert(uri.into(), bytes);
        self
    }
}

impl GltfGridSource for GltfMemory {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn gltf_text(&self) -> Result<String, String> {
        Ok(self.text.clone())
    }

    fn buffer_range(&self, uri: &str, start: u64, len: usize) -> Result<Vec<u8>, String> {
        let bytes = self
            .buffers
            .get(uri)
            .ok_or_else(|| format!("{uri}: no such buffer"))?;
        let start = usize::try_from(start).map_err(|err| err.to_string())?;
        let end = start.checked_add(len).ok_or("buffer range overflows")?;
        bytes
            .get(start..end)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| format!("{uri}: range {start}..{end} past {} bytes", bytes.len()))
    }
}

/// Build the on-foot grids for one map: every set piece into the world grid (world space, with the
/// T3 floor rule against `terrain` and `tunnels`) and every building art into its own local grid
/// under its key ([`super::building_key`]). A source that fails to load is skipped with a warning;
/// a building whose grid is empty is left out, so the host falls back to the door ring.
///
/// The host lists the sources (its map directory walk, [`gltfs_in`] and [`usable_gltf`] help) and
/// keeps its rock props behind [`super::OnFootSolids`].
pub fn load<S: GltfGridSource, N: Send + Sync>(
    params: &OnFootParams,
    set_pieces: &[S],
    buildings: &[(String, S)],
    terrain: Option<&TerrainSampler>,
    tunnels: Option<&TunnelsState<N>>,
) -> PilotSolids {
    let mut solids = PilotSolids::default();
    load_with(params, set_pieces, buildings, terrain, tunnels, &mut solids);
    solids
}

/// [`load`] into an existing [`PilotSolids`] (grids merge cell by cell, span by span).
pub fn load_with<S: GltfGridSource, N: Send + Sync>(
    params: &OnFootParams,
    set_pieces: &[S],
    buildings: &[(String, S)],
    terrain: Option<&TerrainSampler>,
    tunnels: Option<&TunnelsState<N>>,
    solids: &mut PilotSolids,
) {
    let _span = tracing::info_span!(
        "onfoot_grid_load",
        set_pieces = set_pieces.len(),
        buildings = buildings.len()
    )
    .entered();
    if !set_pieces.is_empty() {
        let files = set_pieces;
        // Tunnel linings are tens of heavy hero shells: stamp them on worker threads (load time only).
        // The merge keeps the list order, and it is order-free anyway.
        let workers = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 8)
            .min(files.len().max(1));
        let next = std::sync::atomic::AtomicUsize::new(0);
        let mut grids: Vec<Option<Result<PilotGrid, String>>> = (0..files.len()).map(|_| None).collect();
        let done = std::sync::Mutex::new(&mut grids);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(source) = files.get(i) else { break };
                        let grid = load_gltf_grid_on(params, source, true, terrain, tunnels);
                        if let Ok(mut slots) = done.lock() {
                            slots[i] = Some(grid);
                        }
                    }
                });
            }
        });
        for (source, grid) in files.iter().zip(grids) {
            match grid {
                Some(Ok(grid)) => merge_grid(&mut solids.world, grid),
                Some(Err(err)) => tracing::warn!("on-foot collision: {} ({err})", source.label()),
                None => {}
            }
        }
    }
    for (key, source) in buildings {
        match load_gltf_grid(params, source, false, None) {
            Ok(grid) => {
                if !grid.cells.is_empty() {
                    solids.buildings.insert(key.clone(), grid);
                }
            }
            Err(err) => tracing::warn!("on-foot collision: {} ({err})", source.label()),
        }
    }
    if solids.world.span_cap_hits > 0 {
        tracing::info!(
            hits = solids.world.span_cap_hits,
            "on-foot grid: wall stamps at the span cap (closest spans merged)"
        );
    }
}

/// A set-piece glTF the grid stamps: a `.gltf` whose path names no damage stage, no local-space kit
/// source (`portal_kit`) and no far backdrop (visual only, kilometres wide: stamping one made a grid
/// of a hundred million cells, and no walker can reach it).
pub fn usable_gltf(path: &Path) -> bool {
    if path.extension().and_then(|ext| ext.to_str()) != Some("gltf") {
        return false;
    }
    let text = path.to_string_lossy().to_ascii_lowercase();
    !text.contains("damage") && !text.contains("portal_kit") && !text.contains("backdrop")
}

/// Push every [`usable_gltf`] in `dir` (not recursive) onto `out` (load time; the host's walk).
pub fn gltfs_in(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if usable_gltf(&path) {
            out.push(path);
        }
    }
}

/// Merge `from` into `into` cell by cell: the higher floor wins, and the wall spans are added span by
/// span, so a slab end face under one shell's floor and a roof facet of the next shell over the same
/// cell stay two spans.
pub fn merge_grid(into: &mut PilotGrid, from: PilotGrid) {
    into.span_cap_hits += from.span_cap_hits;
    for (key, cell) in from.cells {
        let slot = into.cells.entry(key).or_default();
        if cell.has_floor && (!slot.has_floor || cell.floor_y > slot.floor_y) {
            slot.floor_y = cell.floor_y;
            slot.has_floor = true;
        }
        for span in cell.spans() {
            if slot.add_wall(span.lo, span.hi) {
                into.span_cap_hits += 1;
            }
        }
    }
}

/// One glTF's grid. `world_space` selects the heightfield reject for set-piece floors.
pub fn load_gltf_grid(
    params: &OnFootParams,
    source: &dyn GltfGridSource,
    world_space: bool,
    terrain: Option<&TerrainSampler>,
) -> Result<PilotGrid, String> {
    load_gltf_grid_on::<()>(params, source, world_space, terrain, None)
}

/// [`load_gltf_grid`] on a map with mesh layers: set-piece floors are rejected against every sim
/// floor at their XZ (T3), not the heightfield alone.
pub fn load_gltf_grid_on<N>(
    params: &OnFootParams,
    source: &dyn GltfGridSource,
    world_space: bool,
    terrain: Option<&TerrainSampler>,
    tunnels: Option<&TunnelsState<N>>,
) -> Result<PilotGrid, String> {
    load_gltf_grid_keep(params, source, world_space, terrain, tunnels, &|_, _, _| true)
}

/// A drawn triangle filter for [`load_gltf_grid_keep`]: world-space vertices in, `false` drops the
/// triangle before it is stamped.
pub type KeepTriangle<'a> = &'a dyn Fn([f32; 3], [f32; 3], [f32; 3]) -> bool;

/// [`load_gltf_grid_on`] with a filter seam (lane W-30, ADR-8323; spec 121 § 4.2): `keep` sees each
/// drawn triangle in world space and drops it when it returns `false`. The Stage B dressing grid
/// (`regia_world_geom::mover::dressing_grid`) drops the triangles that are the canonical tunnel
/// shell. With a filter that keeps everything this is [`load_gltf_grid_on`] bit for bit.
pub fn load_gltf_grid_keep<N>(
    params: &OnFootParams,
    source: &dyn GltfGridSource,
    world_space: bool,
    terrain: Option<&TerrainSampler>,
    tunnels: Option<&TunnelsState<N>>,
    keep: KeepTriangle<'_>,
) -> Result<PilotGrid, String> {
    let text = source.gltf_text()?;
    let doc: serde_json::Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
    let bin_name = doc["buffers"][0]["uri"].as_str().unwrap_or("");
    if bin_name.is_empty() {
        return Err("no buffer uri".into());
    }
    let file = Buffer {
        source,
        uri: bin_name,
    };
    let meshes = doc["meshes"].as_array().ok_or("no meshes")?;
    let chosen = choose_meshes(&doc, meshes);
    let stamp_tris: usize = chosen.iter().map(|index| mesh_tris(&doc, &meshes[*index])).sum();
    let stride = if stamp_tris > STAMP_CAP {
        (stamp_tris / STAMP_CAP).max(2)
    } else {
        1
    };
    let mut grid = PilotGrid::default();
    let nodes = doc["nodes"].as_array();
    for mesh_index in chosen {
        let mesh = &meshes[mesh_index];
        let name = mesh["name"].as_str().unwrap_or("");
        if is_door_name(name) {
            continue;
        }
        let xform = node_matrix_for_mesh(nodes, mesh_index);
        let Some(prims) = mesh["primitives"].as_array() else {
            continue;
        };
        for prim in prims {
            let Some(pos_index) = prim["attributes"]["POSITION"].as_u64() else {
                continue;
            };
            let Some(idx_index) = prim["indices"].as_u64() else {
                continue;
            };
            let positions = read_vec3(&doc, &file, pos_index as usize)?;
            let indices = read_indices(&doc, &file, idx_index as usize)?;
            let mut tri = 0usize;
            let mut nth = 0usize;
            while tri + 2 < indices.len() {
                let ia = indices[tri] as usize;
                let ib = indices[tri + 1] as usize;
                let ic = indices[tri + 2] as usize;
                tri += 3;
                let take = nth.is_multiple_of(stride);
                nth += 1;
                if !take {
                    continue;
                }
                if ia >= positions.len() || ib >= positions.len() || ic >= positions.len() {
                    continue;
                }
                let a = mul_point(xform, positions[ia]);
                let b = mul_point(xform, positions[ib]);
                let c = mul_point(xform, positions[ic]);
                if !keep(a, b, c) {
                    continue;
                }
                if world_space && let Some(terrain) = terrain {
                    // The sim floor nearest the drawn one: the baked ground (grid and drawn patches)
                    // or, on a map with mesh layers, any span's floor.
                    let sim_floor = |x: f32, z: f32, y: f32| {
                        let ground = terrain.height_at_world(x, z);
                        match tunnels {
                            Some(tunnels) => {
                                let span = tunnels.nearest_floor(Some(terrain), x, z, y);
                                if (span - y).abs() < (ground - y).abs() {
                                    span
                                } else {
                                    ground
                                }
                            }
                            None => ground,
                        }
                    };
                    grid.stamp_triangle(a, b, c, Some(&sim_floor));
                    continue;
                }
                grid.stamp_triangle(a, b, c, None);
            }
        }
    }
    carve_door_mouths(params, &doc, meshes, nodes, &file, &mut grid)?;
    Ok(grid)
}

/// The glTF's buffer, read by range through the source (the source's open file handle).
struct Buffer<'a> {
    source: &'a dyn GltfGridSource,
    uri: &'a str,
}

/// A door leaf is a person-height slab. The shell around it is not.
fn door_slab(min: [f32; 3], max: [f32; 3]) -> bool {
    let height = max[1] - min[1];
    let dx = max[0] - min[0];
    let dz = max[2] - min[2];
    let thin = det_min(dx, dz);
    let wide = det_max(dx, dz);
    (0.9..=3.3).contains(&height) && thin < 0.85 && (0.55..=4.5).contains(&wide)
}

/// The drawn door leaf fills the opening on a 0.75 m grid. Cut that mouth
/// back out so the capsule can walk through, and leave the jambs.
fn carve_door_mouths(
    params: &OnFootParams,
    doc: &serde_json::Value,
    meshes: &[serde_json::Value],
    nodes: Option<&Vec<serde_json::Value>>,
    file: &Buffer<'_>,
    grid: &mut PilotGrid,
) -> Result<(), String> {
    for (mesh_index, mesh) in meshes.iter().enumerate() {
        let name = mesh["name"].as_str().unwrap_or("");
        if !is_door_name(name) {
            continue;
        }
        let xform = node_matrix_for_mesh(nodes, mesh_index);
        let Some(prims) = mesh["primitives"].as_array() else {
            continue;
        };
        for prim in prims {
            let Some(pos_index) = prim["attributes"]["POSITION"].as_u64() else {
                continue;
            };
            let positions = read_vec3(doc, file, pos_index as usize)?;
            if positions.is_empty() {
                continue;
            }
            let mut min = [f32::MAX; 3];
            let mut max = [f32::MIN; 3];
            for point in &positions {
                let world = mul_point(xform, *point);
                for axis in 0..3 {
                    min[axis] = det_min(min[axis], world[axis]);
                    max[axis] = det_max(max[axis], world[axis]);
                }
            }
            if door_slab(min, max) {
                grid.clear_door_mouth(params, min, max);
            }
        }
    }
    Ok(())
}

fn is_door_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // `Moving` is a shelter's sliding door leaf. The opening is the way in.
    lower.contains("door") || lower.contains("socket") || lower.contains("moving")
}

/// `(major, minor)` from `_LOD2` or `_LOD0.001`. Higher numbers are coarser.
fn lod_rank(name: &str) -> Option<(u16, u16)> {
    let lower = name.to_ascii_lowercase();
    let at = lower.rfind("_lod")?;
    let bytes = &lower.as_bytes()[at + 4..];
    let mut i = 0;
    let mut major = 0u16;
    let mut saw = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        saw = true;
        major = major
            .saturating_mul(10)
            .saturating_add(u16::from(bytes[i] - b'0'));
        i += 1;
    }
    if !saw {
        return None;
    }
    let mut minor = 0u16;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            minor = minor
                .saturating_mul(10)
                .saturating_add(u16::from(bytes[i] - b'0'));
            i += 1;
        }
    }
    Some((major, minor))
}

fn choose_meshes(doc: &serde_json::Value, meshes: &[serde_json::Value]) -> Vec<usize> {
    let mut buckets: BTreeMap<(u16, u16), Vec<usize>> = BTreeMap::new();
    let mut other = Vec::new();
    for (index, mesh) in meshes.iter().enumerate() {
        let name = mesh["name"].as_str().unwrap_or("");
        if is_door_name(name) {
            continue;
        }
        match lod_rank(name) {
            Some(rank) => buckets.entry(rank).or_default().push(index),
            None => other.push(index),
        }
    }
    if buckets.is_empty() {
        return coarsest_untagged(doc, meshes, &other);
    }
    let counts: Vec<((u16, u16), usize)> = buckets
        .iter()
        .map(|(rank, indices)| {
            let tris = indices.iter().map(|index| mesh_tris(doc, &meshes[*index])).sum();
            (*rank, tris)
        })
        .collect();
    let under = counts
        .iter()
        .filter(|(_, tris)| *tris <= LOD_BUDGET && *tris > 0)
        .max_by_key(|(_, tris)| *tris);
    let rank = if let Some((rank, _)) = under {
        *rank
    } else {
        counts
            .iter()
            .filter(|(_, tris)| *tris > 0)
            .min_by_key(|(_, tris)| *tris)
            .map(|(rank, _)| *rank)
            .unwrap_or((0, 0))
    };
    buckets.remove(&rank).unwrap_or(other)
}

/// Several meshes and no LOD tag: a shell shipped as three copies of itself.
/// Keep the coarsest. One mesh is kept as-is.
fn coarsest_untagged(doc: &serde_json::Value, meshes: &[serde_json::Value], indices: &[usize]) -> Vec<usize> {
    if indices.len() < 2 {
        return indices.to_vec();
    }
    let counts: Vec<(usize, usize)> = indices
        .iter()
        .map(|index| (*index, mesh_tris(doc, &meshes[*index])))
        .filter(|(_, tris)| *tris > 0)
        .collect();
    let Some(min) = counts.iter().map(|(_, tris)| *tris).min() else {
        return indices.to_vec();
    };
    let max = counts.iter().map(|(_, tris)| *tris).max().unwrap_or(0);
    if max > min.saturating_mul(4)
        && let Some((index, _)) = counts.iter().min_by_key(|(_, tris)| *tris)
    {
        return vec![*index];
    }
    counts.into_iter().map(|(index, _)| index).collect()
}

fn mesh_tris(doc: &serde_json::Value, mesh: &serde_json::Value) -> usize {
    let Some(prims) = mesh["primitives"].as_array() else {
        return 0;
    };
    let mut tris = 0usize;
    for prim in prims {
        let Some(index) = prim["indices"].as_u64() else {
            continue;
        };
        let count = doc["accessors"][index as usize]["count"].as_u64().unwrap_or(0);
        tris += (count as usize) / 3;
    }
    tris
}

fn read_vec3(doc: &serde_json::Value, file: &Buffer<'_>, accessor: usize) -> Result<Vec<[f32; 3]>, String> {
    let acc = &doc["accessors"][accessor];
    if acc["type"].as_str() != Some("VEC3") || acc["componentType"].as_u64() != Some(5126) {
        return Err("position accessor is not f32 vec3".into());
    }
    let count = acc["count"].as_u64().unwrap_or(0) as usize;
    let (bytes, stride) = accessor_bytes(doc, file, acc)?;
    let stride = stride.max(12);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = i * stride;
        if at + 12 > bytes.len() {
            break;
        }
        let le = |o: usize| f32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
        out.push([le(at), le(at + 4), le(at + 8)]);
    }
    Ok(out)
}

fn read_indices(doc: &serde_json::Value, file: &Buffer<'_>, accessor: usize) -> Result<Vec<u32>, String> {
    let acc = &doc["accessors"][accessor];
    let count = acc["count"].as_u64().unwrap_or(0) as usize;
    let kind = acc["componentType"].as_u64().unwrap_or(0);
    let (bytes, stride) = accessor_bytes(doc, file, acc)?;
    let width = match kind {
        5121 => 1,
        5123 => 2,
        5125 => 4,
        _ => return Err(format!("index component {kind}")),
    };
    let stride = if stride == 0 { width } else { stride.max(width) };
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = i * stride;
        if at + width > bytes.len() {
            break;
        }
        let index = match width {
            1 => u32::from(bytes[at]),
            2 => u32::from(u16::from_le_bytes([bytes[at], bytes[at + 1]])),
            _ => u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]),
        };
        out.push(index);
    }
    Ok(out)
}

fn accessor_bytes(
    doc: &serde_json::Value,
    file: &Buffer<'_>,
    acc: &serde_json::Value,
) -> Result<(Vec<u8>, usize), String> {
    let view_index = acc["bufferView"].as_u64().ok_or("accessor has no bufferView")? as usize;
    let view = &doc["bufferViews"][view_index];
    let view_off = view["byteOffset"].as_u64().unwrap_or(0);
    let view_len = view["byteLength"].as_u64().unwrap_or(0);
    let acc_off = acc["byteOffset"].as_u64().unwrap_or(0);
    let stride = view["byteStride"].as_u64().unwrap_or(0) as usize;
    let start = view_off + acc_off;
    let bytes = file
        .source
        .buffer_range(file.uri, start, view_len.saturating_sub(acc_off) as usize)?;
    Ok((bytes, stride))
}

fn node_matrix_for_mesh(nodes: Option<&Vec<serde_json::Value>>, mesh_index: usize) -> [f32; 16] {
    let Some(nodes) = nodes else {
        return ident();
    };
    for node in nodes {
        if node["mesh"].as_u64() == Some(mesh_index as u64) {
            return node_matrix(node);
        }
    }
    ident()
}

fn ident() -> [f32; 16] {
    [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

/// Column-major. glTF matrix, or T * R * S.
fn node_matrix(node: &serde_json::Value) -> [f32; 16] {
    if let Some(list) = node["matrix"].as_array()
        && list.len() == 16
    {
        let mut m = [0.0f32; 16];
        for (i, value) in list.iter().enumerate() {
            m[i] = value.as_f64().unwrap_or(0.0) as f32;
        }
        return m;
    }
    let t = vec3_or(node.get("translation"), [0.0, 0.0, 0.0]);
    let s = vec3_or(node.get("scale"), [1.0, 1.0, 1.0]);
    let q = quat_or(node.get("rotation"));
    trs(t, q, s)
}

fn vec3_or(value: Option<&serde_json::Value>, fallback: [f32; 3]) -> [f32; 3] {
    let Some(list) = value.and_then(|value| value.as_array()) else {
        return fallback;
    };
    if list.len() < 3 {
        return fallback;
    }
    [
        list[0].as_f64().unwrap_or(f64::from(fallback[0])) as f32,
        list[1].as_f64().unwrap_or(f64::from(fallback[1])) as f32,
        list[2].as_f64().unwrap_or(f64::from(fallback[2])) as f32,
    ]
}

fn quat_or(value: Option<&serde_json::Value>) -> [f32; 4] {
    let Some(list) = value.and_then(|value| value.as_array()) else {
        return [0.0, 0.0, 0.0, 1.0];
    };
    if list.len() < 4 {
        return [0.0, 0.0, 0.0, 1.0];
    }
    [
        list[0].as_f64().unwrap_or(0.0) as f32,
        list[1].as_f64().unwrap_or(0.0) as f32,
        list[2].as_f64().unwrap_or(0.0) as f32,
        list[3].as_f64().unwrap_or(1.0) as f32,
    ]
}

fn trs(t: [f32; 3], q: [f32; 4], s: [f32; 3]) -> [f32; 16] {
    let (x, y, z, w) = (q[0], q[1], q[2], q[3]);
    let x2 = x + x;
    let y2 = y + y;
    let z2 = z + z;
    let xx = x * x2;
    let xy = x * y2;
    let xz = x * z2;
    let yy = y * y2;
    let yz = y * z2;
    let zz = z * z2;
    let wx = w * x2;
    let wy = w * y2;
    let wz = w * z2;
    [
        (1.0 - (yy + zz)) * s[0],
        (xy + wz) * s[0],
        (xz - wy) * s[0],
        0.0,
        (xy - wz) * s[1],
        (1.0 - (xx + zz)) * s[1],
        (yz + wx) * s[1],
        0.0,
        (xz + wy) * s[2],
        (yz - wx) * s[2],
        (1.0 - (xx + yy)) * s[2],
        0.0,
        t[0],
        t[1],
        t[2],
        1.0,
    ]
}

fn mul_point(m: [f32; 16], p: [f32; 3]) -> [f32; 3] {
    [
        m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
        m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
        m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
    ]
}

#[cfg(test)]
mod tests;
