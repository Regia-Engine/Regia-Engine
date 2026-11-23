// SPDX-License-Identifier: MPL-2.0
//! The terrain mesh-layer cook (spec 26 § 2a.5 and § 2a.10, ADR-8398, cites ADR-7974).
//!
//! One call writes the cooked `<map>.meshlayers` and the chunk LOD meshes:
//!
//! 1. The canonical half (header, sections, triangles, lattice, cut bitmaps) is W-06's:
//!    [`regia_world_geom::cook::cook`] builds it and
//!    [`CookedMeshLayers::encode`](regia_world_geom::cooked::CookedMeshLayers::encode) writes it.
//!    There is one writer of the canonical section, and `content_hash` is its.
//! 2. Each layer's canonical triangles are cut on the CDLOD node grid at the chunk level (the
//!    coarsest level whose node is at most `chunk_m`): a triangle belongs to the chunk holding its
//!    XZ centroid, so neighbors share the canonical border vertices bit for bit.
//! 3. LOD `i` (1..=3) is simplified with meshopt to an absolute error of at most the terrain's
//!    level-`i` cell (`node_world_size(i) / grid_size`), with chunk borders locked
//!    (`LockBorder`) and every vertex within the layer's `max_stitch_gap_m` of a mouth ring
//!    locked, so the band meets every LOD.
//! 4. Each LOD is a `CookedMesh` v3 sub-asset of the map (`meshlayers/<layer>/<x>_<z>/lod<i>`);
//!    the layer table ([`TerrainMeshLayersCooked`](regia_terrain::TerrainMeshLayersCooked)) is the
//!    file's chunk section (ADR-8576; W-06's cook keeps writing the header, the canonical section
//!    and W-07's presentation section).
//!
//! An unchanged input (the canonical input hash, the chunk grid and the map id) writes nothing;
//! every write is atomic and a file whose bytes would not change is not rewritten.
//!
//! **`Volume` layers (spec 26 § 2a.9, ADR-10071).** [`cook_mesh_layers_with_volumes`](crate::terrain_mesh_layers::cook_mesh_layers_with_volumes) takes the
//! host's signed-distance fields ([`VolumeSource`](crate::terrain_mesh_layers::VolumeSource), named by the layer's `sdf`). Each is meshed
//! by dual contouring on its cell grid and clipped under the LOD-0 ground
//! ([`regia_terrain::mesh_layer::volume`]); the clipped LOD-0 mesh is the layer's canonical
//! (query) triangles ([`regia_world_geom::cook::cook_with_layers`]), and each clip loop the layer
//! fills is that hole's footprint, so the lattice cut, collar and zipper join it to the ring as
//! for any layer. Steps 2 to 4 then cut, simplify and write its chunks like any layer's. There is
//! no volume file format yet (`<map>.voxfield` is proposed, spec 122): a `Volume` layer without a
//! [`VolumeSource`](crate::terrain_mesh_layers::VolumeSource) is still `cook_not_built`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use meshopt::SimplifyOptions;
use regia_asset::{
    AssetId, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CpuMesh, cooked_file_name,
    encode_cooked_file, pack_importer_version,
};
use regia_terrain::cdlod::quadtree_span;
use regia_terrain::{
    CdlodConfig, CookedChunkLod, CookedLayer, CookedLayerChunk, LayerChunkGrid, TerrainMeshLayersCooked,
    chunk_lod_asset_id, root_level_for,
};
use regia_world_geom::compat::sampler::TerrainSampler;
use regia_world_geom::cook::{CookError, CookInputs, CookReport, GroundInput, input_hash};
use regia_world_geom::cooked::{CookedMeshLayers, TriKind, split_file};
use regia_world_geom::meshlayers::{LayerSource, MeshLayersDocument};
use regia_world_geom::mm_to_m;

use crate::mesh_cook::pack_cooked_mesh;

/// Importer version of the chunk LOD meshes (bump when their bytes change for the same input).
/// 1.1.0: normals shared across chunk borders and LODs (ADR-8451 decision 6).
pub const MESH_LAYER_CHUNK_IMPORTER: (u32, u32, u32) = (1, 1, 0);

/// [`MESH_LAYER_CHUNK_IMPORTER`] packed (`pack_importer_version`), as the chunk headers and the
/// presentation's `chunk_importer` carry it.
fn chunk_importer() -> u32 {
    let (a, b, c) = MESH_LAYER_CHUNK_IMPORTER;
    pack_importer_version(a, b, c)
}

/// What the mesh-layer cook reads.
pub struct MeshLayersCookRequest<'a> {
    /// The `<map>.meshlayers.ron` bytes.
    pub document: &'a [u8],
    /// The heightfield as the sim lattice reads it.
    pub ground: &'a GroundInput,
    /// The Stage A sampler the `Passages` mesher reads (`None`: flat).
    pub terrain: Option<&'a TerrainSampler>,
    /// Reads a file the document names.
    pub read_file: &'a dyn Fn(&str) -> Result<Vec<u8>, String>,
    /// The map id: the chunk LODs are its sub-assets.
    pub map_id: AssetId,
    /// The cooked `<map>.meshlayers` to write.
    pub out_file: &'a Path,
    /// The cooked cache the chunk LOD meshes go to.
    pub cooked_dir: &'a Path,
    /// The terrain's CDLOD settings (grid size and range factor; default for a level terrain).
    pub cdlod: CdlodConfig,
    /// The map's XZ grid origin, metres (`[0, 0]` for a centered map).
    pub origin: [f32; 2],
    /// Resolves a layer's non-empty `material` path to its cooked `CookedStandardMaterial` id
    /// (ADR-8451; [`material_resolver`] answers from a cook's manifest). An empty `material` is
    /// not asked: it is the default rock.
    pub resolve_material: &'a dyn Fn(&str) -> Result<AssetId, MaterialResolveError>,
}

/// A host's signed-distance field for the document's `Volume` layers whose `sdf` is `sdf`
/// (ADR-10071). The field is negative inside the solid (the [`regia_terrain::volume_mesh`]
/// convention); the layer's surface is where it equals the layer's `iso`.
pub struct VolumeSource<'a> {
    /// The name the document's `Volume { sdf, .. }` gives.
    pub sdf: &'a str,
    /// The field.
    pub field: &'a dyn regia_terrain::volume_mesh::SdfField,
    /// The world box meshed, metres (`[min, max]`).
    pub bounds: [[f32; 3]; 2],
    /// The field's identity for the input hash (a host's content hash of whatever defines it).
    pub fingerprint: &'a [u8],
}

/// Why a layer `material` did not resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaterialResolveError {
    /// No cooked asset at that path or id.
    Missing,
    /// The asset is not a `CookedStandardMaterial`: its cooked type path.
    NotMaterial(String),
}

/// The manifest type path of a cooked standard material.
pub const MATERIAL_TYPE_PATH: &str = "regia_asset::CookedStandardMaterial";

/// Resolves a layer `material` by the asset registry's rule (`AssetRegistry::resolve`: an asset id
/// written as text, a full `assets/` path, or a path under `assets/`) over `entries` (a cook's
/// manifest rows), and requires a [`MATERIAL_TYPE_PATH`] asset.
///
/// # Errors
///
/// [`MaterialResolveError`].
pub fn resolve_material_in<'e>(
    entries: impl IntoIterator<Item = &'e regia_asset::ManifestEntry>,
    material: &str,
) -> Result<AssetId, MaterialResolveError> {
    let by_id = material.parse::<AssetId>().ok();
    let path = material.replace('\\', "/");
    let under = format!("assets/{}", path.trim_start_matches('/'));
    let found = entries
        .into_iter()
        .find(|e| match by_id {
            Some(id) => e.id == id,
            None => e.path == path || e.path == under,
        })
        .ok_or(MaterialResolveError::Missing)?;
    if found.type_path != MATERIAL_TYPE_PATH {
        return Err(MaterialResolveError::NotMaterial(found.type_path.clone()));
    }
    Ok(found.id)
}

/// A [`MeshLayersCookRequest::resolve_material`] over a cook's manifest rows.
pub fn material_resolver(
    entries: &[regia_asset::ManifestEntry],
) -> impl Fn(&str) -> Result<AssetId, MaterialResolveError> + '_ {
    move |material| resolve_material_in(entries, material)
}

/// The Stage A sampler the `Passages` mesher reads over `ground`'s heights: 1/32 m a count, zero
/// at count 32768 (the rule `regia terrain passages import` uses, ADR-8451). `None` when the
/// ground is not a square lattice.
#[must_use]
pub fn stage_a_sampler(ground: &GroundInput) -> Option<TerrainSampler> {
    let counts: Vec<u16> = ground
        .counts
        .iter()
        .map(|&c| {
            let h = f64::from(ground.offset_m) + f64::from(c) * f64::from(ground.step_m);
            ((h + 1024.0) * 32.0).round().clamp(0.0, 65535.0) as u16
        })
        .collect();
    let grid = regia_world_geom::compat::sampler::HeightGrid::square(ground.lattice_n as usize, counts)?;
    Some(TerrainSampler::new(grid, ground.width_m).with_zero_counts(32768))
}

/// What the cook did.
#[derive(Clone, Debug)]
pub struct MeshLayersCookOutcome {
    /// `false` when the input was unchanged and nothing was cooked or written.
    pub cooked: bool,
    /// Files written (an unchanged file is not rewritten).
    pub written: Vec<PathBuf>,
    /// The layer table.
    pub presentation: TerrainMeshLayersCooked,
    /// W-06's report (`None` when the cook was skipped).
    pub report: Option<CookReport>,
}

/// Why the mesh-layer cook failed. Codes are part of the agent protocol.
#[derive(Debug, thiserror::Error)]
pub enum MeshLayersCookError {
    /// W-06's canonical cook refused the input (its own codes).
    #[error(transparent)]
    Geom(#[from] CookError),
    /// (`meshlayers_cook_io`) a file could not be written.
    #[error("{path}: {reason}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What failed.
        reason: String,
    },
    /// (`meshlayers_cook_mesh`) a chunk mesh did not pack.
    #[error("chunk mesh: {0}")]
    Mesh(String),
    /// (`meshlayers_material_missing`) a layer's `material` names no cooked asset.
    #[error(
        "layer `{layer}`: material `{path}` is not a cooked asset of this project (meshlayers_material_missing)"
    )]
    MaterialMissing {
        /// The layer id.
        layer: String,
        /// The `material` as written.
        path: String,
    },
    /// (`meshlayers_volume_mesh`, `meshlayers_volume_grid`) a `Volume` layer did not mesh.
    #[error("layer `{layer}`: {error}")]
    Volume {
        /// The layer id.
        layer: String,
        /// The mesher's error.
        error: regia_terrain::mesh_layer::volume::VolumeLayerError,
    },
    /// (`meshlayers_volume_mouths`) a `Volume` layer's clip loops cannot be its filled holes: a
    /// count that differs from `fills`, a chain that does not close, or a loop that is not
    /// star-shaped from its mean point.
    #[error("layer `{layer}`: {detail} (meshlayers_volume_mouths)")]
    VolumeMouths {
        /// The layer id.
        layer: String,
        /// What did not match.
        detail: String,
    },
    /// (`meshlayers_material_type`) a layer's `material` names an asset that is not a material.
    #[error(
        "layer `{layer}`: material `{path}` is a {type_path}, not a CookedStandardMaterial (meshlayers_material_type)"
    )]
    MaterialType {
        /// The layer id.
        layer: String,
        /// The `material` as written.
        path: String,
        /// The asset's cooked type path.
        type_path: String,
    },
}

impl MeshLayersCookError {
    /// The agent error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Geom(e) => e.code(),
            Self::Io { .. } => "meshlayers_cook_io",
            Self::Mesh(_) => "meshlayers_cook_mesh",
            Self::MaterialMissing { .. } => "meshlayers_material_missing",
            Self::MaterialType { .. } => "meshlayers_material_type",
            Self::Volume { error, .. } => error.code(),
            Self::VolumeMouths { .. } => "meshlayers_volume_mouths",
        }
    }
}

fn io_err(path: &Path, e: impl std::fmt::Display) -> MeshLayersCookError {
    MeshLayersCookError::Io {
        path: path.to_path_buf(),
        reason: e.to_string(),
    }
}

/// Writes `bytes` to `path` atomically unless the file already holds them. Returns whether it wrote.
fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<bool, MeshLayersCookError> {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    }
    crate::cook_ledger::write_atomic(path, bytes).map_err(|e| io_err(path, e))?;
    Ok(true)
}

/// The chunk grid the cook cuts on for this ground and CDLOD config: the level terrain's root
/// ([`root_level_for`]) and draw span ([`quadtree_span`]).
#[must_use]
pub fn chunk_grid(
    ground: &GroundInput,
    cdlod: &CdlodConfig,
    origin: [f32; 2],
    chunk_m: f32,
) -> LayerChunkGrid {
    let n = ground.lattice_n;
    let root = root_level_for(n, n, cdlod);
    let span = quadtree_span(ground.width_m, n, cdlod.grid_size, root);
    LayerChunkGrid::new(span, root, cdlod.grid_size, cdlod.range_factor, origin, chunk_m)
}

/// The files the document names, read in layer order (for the input hash).
fn named_files(
    doc: &MeshLayersDocument,
    read: &dyn Fn(&str) -> Result<Vec<u8>, String>,
) -> Option<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    for layer in &doc.layers {
        let path = match &layer.source {
            LayerSource::Passages { path } => path,
            _ => return None,
        };
        out.push((path.clone(), read(path).ok()?));
    }
    Some(out)
}

/// Whether `out_file` already holds this input's cook (same canonical input hash, file and
/// presentation versions, chunk grid and map id) with every chunk LOD file present.
fn up_to_date(
    req: &MeshLayersCookRequest<'_>,
    doc: &MeshLayersDocument,
    grid: &LayerChunkGrid,
    materials: &[Option<AssetId>],
) -> Option<TerrainMeshLayersCooked> {
    let bytes = std::fs::read(req.out_file).ok()?;
    let parts = split_file(&bytes).ok()?;
    if parts.header.version != regia_world_geom::cooked::COOKED_FORMAT_VERSION {
        return None;
    }
    let files = named_files(doc, req.read_file)?;
    let named: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect();
    if parts.header.input_hash != input_hash(req.document, req.ground, &named) {
        return None;
    }
    let presentation = TerrainMeshLayersCooked::decode(&parts.chunks).ok()??;
    if presentation.grid != *grid
        || presentation.layers.len() != doc.layers.len()
        || presentation.chunk_importer != chunk_importer()
    {
        return None;
    }
    for ((layer, cooked), material) in doc.layers.iter().zip(&presentation.layers).zip(materials) {
        // `collider`: a file migrated from v1 says false whatever the document says (ADR-8872).
        if cooked.id != layer.id || cooked.material_id != *material || cooked.collider != layer.collider {
            return None;
        }
        for c in &cooked.chunks {
            for (i, lod) in c.lods.iter().enumerate() {
                if lod.mesh != chunk_lod_asset_id(req.map_id, &layer.id, c.x, c.z, i as u8)
                    || !req.cooked_dir.join(cooked_file_name(lod.mesh)).is_file()
                {
                    return None;
                }
            }
        }
    }
    Some(presentation)
}

/// One welded chunk mesh at LOD 0.
#[derive(Default)]
struct ChunkSoup {
    positions: Vec<[f32; 3]>,
    indices: Vec<u32>,
    /// World millimetres of each vertex (the weld key).
    keys: Vec<[i32; 3]>,
    lookup: HashMap<[i32; 3], u32>,
}

impl ChunkSoup {
    fn vertex(&mut self, mm: [i32; 3], m: [f32; 3]) -> u32 {
        if let Some(&i) = self.lookup.get(&mm) {
            return i;
        }
        let i = self.positions.len() as u32;
        self.positions.push(m);
        self.keys.push(mm);
        self.lookup.insert(mm, i);
        i
    }
}

/// A triangle's area-weighted normal (the cross product, twice the area long).
fn face_normal([a, b, c]: [[f32; 3]; 3]) -> [f32; 3] {
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ]
}

/// A unit vector along `n`, `+y` for a zero sum.
fn unit(n: [f32; 3]) -> [f32; 3] {
    let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if l > 0.0 {
        [n[0] / l, n[1] / l, n[2] / l]
    } else {
        [0.0, 1.0, 0.0]
    }
}

/// Area-weighted vertex normals of a whole layer at LOD 0, keyed by `(layer, world millimetres)`
/// (ADR-8451 decision 6). A vertex on a chunk border sums the triangles of every chunk around it,
/// so both sides of the border, and every LOD (the simplifier makes no vertex), read the same
/// normal: no lighting seam at a border and none between LODs.
#[derive(Default)]
struct LayerNormals {
    sums: HashMap<(u16, [i32; 3]), [f32; 3]>,
}

impl LayerNormals {
    /// Adds one drawn triangle (in the canonical order, so the sums are the same on every machine).
    fn add(&mut self, layer: u16, mm: [[i32; 3]; 3], m: [[f32; 3]; 3]) {
        let n = face_normal(m);
        for k in mm {
            let s = self.sums.entry((layer, k)).or_insert([0.0; 3]);
            for (s, v) in s.iter_mut().zip(n) {
                *s += v;
            }
        }
    }

    /// The unit normal at `key` of `layer`.
    fn get(&self, layer: u16, key: [i32; 3]) -> [f32; 3] {
        unit(self.sums.get(&(layer, key)).copied().unwrap_or([0.0; 3]))
    }
}

/// Box-projected UVs: the plane of the dominant normal axis, `uv_tile_m` per repeat.
fn box_uvs(positions: &[[f32; 3]], normals: &[[f32; 3]], tile_m: f32) -> Vec<[f32; 2]> {
    let s = if tile_m.is_finite() && tile_m > 0.0 {
        1.0 / tile_m
    } else {
        0.25
    };
    positions
        .iter()
        .zip(normals)
        .map(|(p, n)| {
            let (ax, ay, az) = (n[0].abs(), n[1].abs(), n[2].abs());
            if ay >= ax && ay >= az {
                [p[0] * s, p[2] * s]
            } else if ax >= az {
                [p[2] * s, p[1] * s]
            } else {
                [p[0] * s, p[1] * s]
            }
        })
        .collect()
}

/// The vertices `indices` use, renumbered in first-use order, with each one's soup index.
fn compact(positions: &[[f32; 3]], indices: &[u32]) -> (Vec<[f32; 3]>, Vec<u32>, Vec<usize>) {
    let mut remap = vec![u32::MAX; positions.len()];
    let mut out_p = Vec::new();
    let mut source = Vec::new();
    let mut out_i = Vec::with_capacity(indices.len());
    for &i in indices {
        let slot = &mut remap[i as usize];
        if *slot == u32::MAX {
            *slot = out_p.len() as u32;
            out_p.push(positions[i as usize]);
            source.push(i as usize);
        }
        out_i.push(*slot);
    }
    (out_p, out_i, source)
}

/// The cooked `CookedMesh` v3 file of one chunk LOD, and its payload length (the budget bytes).
fn chunk_lod_file(
    id: AssetId,
    positions: Vec<[f32; 3]>,
    n: Vec<[f32; 3]>,
    indices: Vec<u32>,
    uv_tile_m: f32,
) -> Result<(Vec<u8>, u64), MeshLayersCookError> {
    let uvs = box_uvs(&positions, &n, uv_tile_m);
    let cpu = CpuMesh {
        positions,
        normals: n,
        uvs,
        indices,
        ..CpuMesh::default()
    };
    let cooked = pack_cooked_mesh(&cpu, None).map_err(|e| MeshLayersCookError::Mesh(e.to_string()))?;
    let payload = cooked
        .to_postcard()
        .map_err(|e| MeshLayersCookError::Mesh(e.to_string()))?;
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: AssetTypeTag::mesh().0,
        asset_id: id,
        importer_version: chunk_importer(),
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    let file = encode_cooked_file(&header, &payload).map_err(|e| MeshLayersCookError::Mesh(e.to_string()))?;
    Ok((file, payload.len() as u64))
}

/// A layer's material id: `None` for an empty `material` (the default rock), else the resolved id.
fn layer_material(
    req: &MeshLayersCookRequest<'_>,
    layer: &str,
    material: &str,
) -> Result<Option<AssetId>, MeshLayersCookError> {
    if material.trim().is_empty() {
        return Ok(None);
    }
    (req.resolve_material)(material).map(Some).map_err(|e| match e {
        MaterialResolveError::Missing => MeshLayersCookError::MaterialMissing {
            layer: layer.to_owned(),
            path: material.to_owned(),
        },
        MaterialResolveError::NotMaterial(type_path) => MeshLayersCookError::MaterialType {
            layer: layer.to_owned(),
            path: material.to_owned(),
            type_path,
        },
    })
}

/// The LOD-0 ground height at world `(x, z)` metres: the lattice triangle under the point (the
/// engine's diagonal, `v(i, j + 1)` to `v(i + 1, j)`), interpolated in `f64`; clamped onto the
/// map at its edges.
fn lattice_ground(
    lattice: &regia_world_geom::cooked::LatticeConstants,
    counts: &[u16],
    x: f64,
    z: f64,
) -> f64 {
    let cells = lattice.cells().max(1);
    let locate = |v_m: f64| -> (u32, f64) {
        let v = v_m * 1000.0;
        let mut i = 0u32;
        while i + 1 < cells && (lattice.vertex_mm(i + 1) as f64) <= v {
            i += 1;
        }
        let (a, b) = (lattice.vertex_mm(i) as f64, lattice.vertex_mm(i + 1) as f64);
        (i, ((v - a) / (b - a)).clamp(0.0, 1.0))
    };
    let (i, fx) = locate(x);
    let (j, fz) = locate(z);
    let h = |di: u32, dj: u32| {
        f64::from(regia_world_geom::cut::count_height_m(
            lattice,
            counts,
            i + di,
            j + dj,
        ))
    };
    if fx + fz <= 1.0 {
        h(0, 0) + fx * (h(1, 0) - h(0, 0)) + fz * (h(0, 1) - h(0, 0))
    } else {
        h(1, 1) + (1.0 - fx) * (h(0, 1) - h(1, 1)) + (1.0 - fz) * (h(1, 0) - h(1, 1))
    }
}

/// The canonical triangles and mouth footprints of the document's `Volume` layers that have a
/// [`VolumeSource`] (module docs, ADR-10071).
fn volume_layers(
    req: &MeshLayersCookRequest<'_>,
    doc: &MeshLayersDocument,
    volumes: &[VolumeSource<'_>],
) -> Result<Vec<regia_world_geom::cook::ProvidedLayer>, MeshLayersCookError> {
    use regia_terrain::mesh_layer::volume::{VolumeLayerInputs, loop_footprint_quads, mesh_volume_layer};
    let mut out = Vec::new();
    let mut lattice = None;
    for layer in &doc.layers {
        let LayerSource::Volume { sdf, iso, cell_m } = &layer.source else {
            continue;
        };
        let Some(source) = volumes.iter().find(|v| v.sdf == sdf) else {
            continue;
        };
        let _span = tracing::info_span!("meshlayers_volume_layer", layer = %layer.id).entered();
        if lattice.is_none() {
            lattice = Some(req.ground.lattice(doc.ground_tile_cells)?);
        }
        let Some(lat) = lattice.as_ref() else {
            continue;
        };
        let counts = &req.ground.counts;
        let ground = |x: f64, z: f64| lattice_ground(lat, counts, x, z);
        let block_cells = (doc.chunk_m / cell_m).floor().clamp(1.0, 4096.0) as u32;
        let mesh = mesh_volume_layer(&VolumeLayerInputs {
            field: source.field,
            iso: *iso,
            cell_m: *cell_m,
            bounds: source.bounds,
            block_cells,
            ground: &ground,
        })
        .map_err(|error| MeshLayersCookError::Volume {
            layer: layer.id.clone(),
            error,
        })?;
        let mouths_err = |detail: String| MeshLayersCookError::VolumeMouths {
            layer: layer.id.clone(),
            detail,
        };
        let mut mouths = Vec::new();
        if !layer.fills.is_empty() {
            if mesh.open_chains > 0 {
                return Err(mouths_err(format!(
                    "{} clip chain(s) on the ground do not close inside the volume bounds",
                    mesh.open_chains
                )));
            }
            if mesh.loops.len() != layer.fills.len() {
                return Err(mouths_err(format!(
                    "{} clip loop(s) on the ground for {} filled hole(s)",
                    mesh.loops.len(),
                    layer.fills.len()
                )));
            }
            // `fills` in document order take the loops in their (z, x) order.
            for (hole, ring) in layer.fills.iter().zip(&mesh.loops) {
                let quads = loop_footprint_quads(ring)
                    .ok_or_else(|| mouths_err(format!("the clip loop for `{hole}` is not star-shaped")))?;
                mouths.push(regia_world_geom::cut::MouthFootprint {
                    id: hole.clone(),
                    quads,
                });
            }
        }
        let mut source_bytes = source.fingerprint.to_vec();
        for v in [*iso, *cell_m]
            .into_iter()
            .chain(source.bounds.into_iter().flatten())
        {
            source_bytes.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        out.push(regia_world_geom::cook::ProvidedLayer {
            id: layer.id.clone(),
            triangles: mesh
                .triangles
                .iter()
                .map(|t| t.map(|p| p.map(regia_world_geom::m_to_mm)))
                .collect(),
            mouths,
            source_bytes,
        });
    }
    Ok(out)
}

/// Cooks `<map>.meshlayers` and its chunk LOD meshes (module docs). A `Volume` layer is
/// `cook_not_built` here: [`cook_mesh_layers_with_volumes`] takes its field.
///
/// # Errors
///
/// [`MeshLayersCookError`]: W-06's cook errors, a write failure, a chunk that does not pack.
pub fn cook_mesh_layers(
    req: &MeshLayersCookRequest<'_>,
) -> Result<MeshLayersCookOutcome, MeshLayersCookError> {
    cook_mesh_layers_with_volumes(req, &[])
}

/// [`cook_mesh_layers`] with the host's fields for the document's `Volume` layers (module docs,
/// ADR-10071). With no volumes this is [`cook_mesh_layers`], byte for byte.
///
/// # Errors
///
/// [`MeshLayersCookError`]; [`MeshLayersCookError::Volume`] and
/// [`MeshLayersCookError::VolumeMouths`] for a volume layer.
pub fn cook_mesh_layers_with_volumes(
    req: &MeshLayersCookRequest<'_>,
    volumes: &[VolumeSource<'_>],
) -> Result<MeshLayersCookOutcome, MeshLayersCookError> {
    let _span = tracing::info_span!("meshlayers_stream_cook").entered();
    let text = std::str::from_utf8(req.document).map_err(|e| {
        CookError::Document(regia_world_geom::meshlayers::MeshLayersError::Parse(format!(
            "not UTF-8: {e}"
        )))
    })?;
    let doc = MeshLayersDocument::from_ron(text).map_err(CookError::Document)?;
    let grid = chunk_grid(req.ground, &req.cdlod, req.origin, doc.chunk_m);
    // Materials first: a missing one fails the cook before anything is written (ADR-8451).
    let materials = doc
        .layers
        .iter()
        .map(|layer| layer_material(req, &layer.id, &layer.material))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(presentation) = up_to_date(req, &doc, &grid, &materials) {
        return Ok(MeshLayersCookOutcome {
            cooked: false,
            written: Vec::new(),
            presentation,
            report: None,
        });
    }
    // 1. The canonical half: W-06's writer, with the volume layers' meshed triangles (ADR-10071).
    let provided = volume_layers(req, &doc, volumes)?;
    let out = regia_world_geom::cook::cook_with_layers(
        &CookInputs {
            document: req.document,
            ground: req.ground,
            terrain: req.terrain,
            read_file: req.read_file,
        },
        &provided,
    )?;
    let canonical = &out.file.canonical;
    let lattice = canonical.lattice;
    // Mouth rings, world millimetres XZ, per layer.
    let mut rings: BTreeMap<u16, Vec<[i64; 2]>> = BTreeMap::new();
    for m in &out.report.ground.mouths {
        let v = rings.entry(m.layer).or_default();
        for ring in &m.rings {
            v.extend(
                ring.iter()
                    .map(|&(i, j)| [lattice.vertex_mm(i), lattice.vertex_mm(j)]),
            );
        }
    }
    // 2. Cut each layer's canonical triangles on the node grid.
    let mut soups: BTreeMap<(u16, u32, u32), ChunkSoup> = BTreeMap::new();
    let mut layer_normals = LayerNormals::default();
    let mut outside = 0u32;
    for (origin, t) in canonical.triangles_sorted() {
        if t.kind == TriKind::Blocker {
            continue;
        }
        let mm = t.v.map(|p| [origin[0] + p[0], p[1], origin[1] + p[2]]);
        let m = t.v.map(|p| {
            [
                mm_to_m(origin[0], p[0]),
                mm_to_m(0, p[1]),
                mm_to_m(origin[1], p[2]),
            ]
        });
        let cx = (m[0][0] + m[1][0] + m[2][0]) / 3.0;
        let cz = (m[0][2] + m[1][2] + m[2][2]) / 3.0;
        let Some((x, z)) = grid.chunk_at([cx, cz]) else {
            outside += 1;
            continue;
        };
        let soup = soups.entry((t.layer, z, x)).or_default();
        let ix = [0, 1, 2].map(|k| soup.vertex(mm[k], m[k]));
        if ix[0] != ix[1] && ix[1] != ix[2] && ix[0] != ix[2] {
            soup.indices.extend_from_slice(&ix);
            layer_normals.add(t.layer, mm, m);
        }
    }
    if outside > 0 {
        tracing::warn!(target: "regia_asset_pipeline", outside, "mesh layer triangles outside the terrain's node grid were not drawn");
    }
    // 3 and 4. LODs, files, the layer table.
    let lod_cap = (u32::from(regia_terrain::mesh_layer::MAX_LAYER_LODS)).min(grid.root_level + 1);
    let mut written = Vec::new();
    let mut layers = Vec::with_capacity(doc.layers.len());
    for ((li, layer), material_id) in doc.layers.iter().enumerate().zip(&materials) {
        let li16 = li as u16;
        let lods = u32::from(layer.max_lods.max(1)).min(lod_cap) as u8;
        let lock_r_mm = f64::from(layer.max_stitch_gap_m.max(0.0)) * 1000.0;
        let ring = rings.get(&li16).map(Vec::as_slice).unwrap_or(&[]);
        let mut chunks = Vec::new();
        for ((_, z, x), soup) in soups.range((li16, 0, 0)..=(li16, u32::MAX, u32::MAX)) {
            if soup.indices.is_empty() {
                continue;
            }
            let locks: Vec<bool> = soup
                .keys
                .iter()
                .map(|k| {
                    ring.iter().any(|r| {
                        let dx = (i64::from(k[0]) - r[0]) as f64;
                        let dz = (i64::from(k[2]) - r[1]) as f64;
                        dx * dx + dz * dz <= lock_r_mm * lock_r_mm
                    })
                })
                .collect();
            let mut bounds = [[f32::INFINITY; 3], [f32::NEG_INFINITY; 3]];
            for p in &soup.positions {
                for (k, v) in p.iter().enumerate() {
                    bounds[0][k] = bounds[0][k].min(*v);
                    bounds[1][k] = bounds[1][k].max(*v);
                }
            }
            let mut records = Vec::with_capacity(usize::from(lods));
            let mut prev = soup.indices.clone();
            for lod in 0..lods {
                let (indices, error_m) = if lod == 0 {
                    (soup.indices.clone(), 0.0)
                } else {
                    let mut err = 0.0f32;
                    let simplified = meshopt::simplify_with_locks_decoder(
                        &soup.indices,
                        &soup.positions,
                        &locks,
                        0,
                        grid.lod_error_m(lod),
                        SimplifyOptions::LockBorder | SimplifyOptions::ErrorAbsolute,
                        Some(&mut err),
                    );
                    if simplified.is_empty() {
                        (prev.clone(), 0.0)
                    } else {
                        (simplified, err)
                    }
                };
                prev.clone_from(&indices);
                let (positions, local, source) = compact(&soup.positions, &indices);
                let normals = source
                    .iter()
                    .map(|&i| layer_normals.get(li16, soup.keys[i]))
                    .collect();
                let id = chunk_lod_asset_id(req.map_id, &layer.id, *x, *z, lod);
                let triangles = (local.len() / 3) as u32;
                let (file, bytes) = chunk_lod_file(id, positions, normals, local, layer.look.uv_tile_m)?;
                let path = req.cooked_dir.join(cooked_file_name(id));
                if write_if_changed(&path, &file)? {
                    written.push(path);
                }
                records.push(CookedChunkLod {
                    mesh: id,
                    triangles,
                    bytes,
                    error_m,
                });
            }
            chunks.push(CookedLayerChunk {
                x: *x,
                z: *z,
                bounds,
                lods: records,
            });
        }
        layers.push(CookedLayer {
            id: layer.id.clone(),
            map_layer: layer.map_layer,
            cast_shadows: layer.cast_shadows,
            collider: layer.collider,
            material: layer.material.clone(),
            material_id: *material_id,
            chunks,
        });
    }
    let presentation = TerrainMeshLayersCooked {
        version: regia_terrain::mesh_layer::PRESENTATION_VERSION,
        grid,
        layers,
        chunk_importer: chunk_importer(),
    };
    let file = CookedMeshLayers {
        chunks: presentation
            .encode()
            .map_err(|e| MeshLayersCookError::Mesh(e.to_string()))?,
        ..out.file
    };
    let bytes = file
        .encode()
        .map_err(|e| MeshLayersCookError::Mesh(e.to_string()))?;
    if write_if_changed(req.out_file, &bytes)? {
        written.push(req.out_file.to_path_buf());
    }
    Ok(MeshLayersCookOutcome {
        cooked: true,
        written,
        presentation,
        report: Some(out.report),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::ManifestEntry;

    fn rows() -> (Vec<ManifestEntry>, AssetId, AssetId) {
        let rock = AssetId::sub_asset(AssetId::default(), "rock");
        let png = AssetId::sub_asset(AssetId::default(), "png");
        let rows = vec![
            ManifestEntry {
                path: "assets/materials/rock.material.ron".into(),
                id: rock,
                type_path: MATERIAL_TYPE_PATH.into(),
            },
            ManifestEntry {
                path: "assets/textures/rock.png".into(),
                id: png,
                type_path: "regia_asset::CookedTexture".into(),
            },
        ];
        (rows, rock, png)
    }

    #[test]
    fn a_layer_material_resolves_by_the_registry_rule() {
        let (rows, rock, _) = rows();
        for written in [
            "materials/rock.material.ron",
            "assets/materials/rock.material.ron",
            "/materials/rock.material.ron",
            "materials\\rock.material.ron",
        ] {
            assert_eq!(resolve_material_in(&rows, written), Ok(rock), "{written}");
        }
        assert_eq!(resolve_material_in(&rows, &rock.to_string()), Ok(rock), "id text");
    }

    #[test]
    fn a_missing_or_wrong_layer_material_is_refused_with_its_code() {
        let (rows, _, png) = rows();
        assert_eq!(
            resolve_material_in(&rows, "materials/gone.material.ron"),
            Err(MaterialResolveError::Missing)
        );
        let other = AssetId::sub_asset(AssetId::default(), "gone");
        assert_eq!(
            resolve_material_in(&rows, &other.to_string()),
            Err(MaterialResolveError::Missing)
        );
        assert_eq!(
            resolve_material_in(&rows, "textures/rock.png"),
            Err(MaterialResolveError::NotMaterial(
                "regia_asset::CookedTexture".into()
            ))
        );
        assert_eq!(
            resolve_material_in(&rows, &png.to_string()).map_err(|_| ()),
            Err(())
        );
        // The cook names the layer, the path and the code.
        let resolve = material_resolver(&rows);
        let ground = GroundInput {
            lattice_n: 2,
            width_m: 1.0,
            offset_m: 0.0,
            step_m: 1.0,
            counts: vec![0; 4],
        };
        let dir = regia_test_utils::unique_temp_dir("meshlayers-material-missing");
        let out_file = dir.join("m.meshlayers");
        let doc = b"(version: 1, layers: [(id: \"tubes\", source: Passages(path: \"p\"), material: \"materials/gone.material.ron\")])";
        let result = cook_mesh_layers(&MeshLayersCookRequest {
            document: doc,
            ground: &ground,
            terrain: None,
            read_file: &|_| Err("unread".into()),
            map_id: AssetId::default(),
            out_file: &out_file,
            cooked_dir: &dir,
            cdlod: CdlodConfig::default(),
            origin: [0.0, 0.0],
            resolve_material: &resolve,
        });
        let Err(err) = result else {
            panic!("a missing material must fail the cook");
        };
        assert_eq!(err.code(), "meshlayers_material_missing");
        let text = err.to_string();
        assert!(
            text.contains("tubes") && text.contains("materials/gone.material.ron"),
            "{text}"
        );
        assert!(!out_file.exists(), "nothing written");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
