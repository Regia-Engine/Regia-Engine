// SPDX-License-Identifier: MPL-2.0
//! Incremental GI bakes (ADR-1094): a rebake traces only the probes near what changed.
//!
//! The volume is split into cells ([`bake_cells`]): one per detail brick, and one per tile of
//! [`IncrementalSettings::tile_columns`]² terrain-layer columns. Each cell has
//!
//! - a **key**: a hash of its probes' nominal positions and relocation bounds. A brick that did
//!   not move keeps its key when bricks are added or removed around it; a terrain tile keeps
//!   its key until the ground under its columns changes.
//! - a **scene hash** ([`cell_scene_hashes`]): a hash of every part of the scene within the
//!   influence margin ([`IncrementalSettings::influence_margin`]) of the cell's probes: the
//!   placed meshes (mesh data, material, textures, transform), the local lights, and the
//!   heightfield samples. It does not depend on the order of the instances.
//!
//! Five global hashes ([`GlobalHashes`]) cover what lights every probe: the tracer and device,
//! the bake settings, the sky (sun, sky, sky bodies, stars, media, and a cloud field when one
//! is set), the terrain's grid and material, and the camera (texture detail fades with the
//! distance to it).
//!
//! [`bake_gi_volume_incremental`] compares these with the [`BakeState`] the previous bake
//! saved. A global change, or no state, bakes every cell. Otherwise a cell whose key and scene
//! hash are in the state reuses its stored probes (full precision, before the filter), and only
//! the other cells' probes are traced ([`crate::probe::bake_probes`]). The 3×3×3 filter and the
//! encoding then run over the whole volume ([`crate::probe::finish_bake`]). So a rebake of an
//! unchanged scene traces nothing and gives the previous volume bit for bit, and the probes of
//! a reused cell are the previous bake's bit for bit.
//!
//! What is approximate: light from an edit farther than the margin from a cell. Such a cell
//! keeps the light it was baked with. The margin defaults to the layout's distance clamp
//! (`max_distance`, what the probes' visibility sees), and [`IncrementalReport::approximate`]
//! says when a cell was reused although the scene changed. A bake without a state is the
//! reference: run one before shipping a volume.
//!
//! A cell traced alone gets other random streams than in a full bake (the tracers key them by
//! the position in the traced list), so its probes match a full bake in expectation, not bit
//! for bit, unless the traced cells are a prefix of the volume.
//!
//! The state file (`FGIS`, [`BakeState::write`]) is a build cache, not a content format: a file
//! of another version, or one that fails its checksum, is refused, and the caller bakes from
//! scratch. It is written to a temporary name and renamed, so a killed bake leaves the old
//! state or the new one.

#[cfg(test)]
use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[cfg(test)]
thread_local! {
    /// Mesh buffer bytes [`SceneDigest::new`] has hashed on this thread.
    static MESH_BYTES_HASHED: Cell<u64> = const { Cell::new(0) };
    /// Item overlap tests [`SceneDigest::cell_hash`] has run on this thread.
    static CELL_ITEM_TESTS: Cell<u64> = const { Cell::new(0) };
}

/// Mesh buffer bytes the incremental scene digest has hashed on this thread.
#[cfg(test)]
#[must_use]
pub(crate) fn incremental_mesh_bytes_hashed() -> u64 {
    MESH_BYTES_HASHED.with(Cell::get)
}

/// Item overlap tests cell scene hashes have run on this thread.
#[cfg(test)]
#[must_use]
pub(crate) fn incremental_cell_item_tests() -> u64 {
    CELL_ITEM_TESTS.with(Cell::get)
}

use regia_asset::baked_gi::{GI_COMPONENTS, ProbeData, ProbeMeta, ShRgb, VolumeLayout};
use regia_core::glam::Vec3;

use crate::PtError;
use crate::probe::{
    BakedProbe, GiBake, GiBakeSettings, ProbeQuality, ProbeTracer, bake_probes_with_stats, finish_bake,
};
use crate::scene::{PtHeightfield, PtLight, PtMaterial, PtScene, PtSky};
use crate::sha256::Sha256;

/// Version of the state file; a state of another version is refused (the bake starts over).
pub const STATE_FORMAT_VERSION: u32 = 1;
/// Terrain-layer columns per tile edge ([`IncrementalSettings::tile_columns`]).
pub const DEFAULT_TILE_COLUMNS: u32 = 8;

const STATE_MAGIC: &[u8; 4] = b"FGIS";
/// A box `(min, max)`, meters.
type Box3 = ([f32; 3], [f32; 3]);
/// Bytes of the checksum after the state's contents.
const CHECKSUM_BYTES: usize = 32;

/// How an incremental bake splits the volume and what it counts as a cell's surroundings.
#[derive(Clone, Debug, PartialEq)]
pub struct IncrementalSettings {
    /// Scene content within this many meters of a cell's probes belongs to the cell: an edit
    /// there rebakes it. `None` (or a negative or non-finite value) uses the layout's
    /// `max_distance`.
    pub influence_margin: Option<f32>,
    /// Terrain-layer columns per tile edge (at least 1).
    pub tile_columns: u32,
    /// The device the traces run on (the adapter's name and driver, or `cpu`): a state from
    /// another device is not reused, so a volume's probes come from one device.
    pub device: String,
}

impl Default for IncrementalSettings {
    fn default() -> Self {
        Self {
            influence_margin: None,
            tile_columns: DEFAULT_TILE_COLUMNS,
            device: String::new(),
        }
    }
}

impl IncrementalSettings {
    /// The influence margin in meters for `layout`.
    #[must_use]
    pub fn margin(&self, layout: &VolumeLayout) -> f32 {
        match self.influence_margin {
            Some(m) if m.is_finite() && m >= 0.0 => m,
            _ => layout.max_distance.max(0.0),
        }
    }
}

/// What a cell covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CellKind {
    /// Terrain-layer tile `[i, j]`: columns `i·n..(i + 1)·n` along X and `j·n..(j + 1)·n`
    /// along Z, `n` = [`IncrementalSettings::tile_columns`].
    Terrain([u32; 2]),
    /// Detail brick `index` of the layout.
    Brick(usize),
}

/// One cell of a volume (module docs).
#[derive(Clone, Debug, PartialEq)]
pub struct BakeCell {
    /// What the cell covers.
    pub kind: CellKind,
    /// Volume indices of the cell's probes, ascending.
    pub probes: Vec<usize>,
    /// Corner of the box every probe of the cell can take (nominal position ± half cell).
    pub min: [f32; 3],
    /// Opposite corner of that box.
    pub max: [f32; 3],
    /// Hash of the probes' nominal positions and half cells: the cell's identity.
    pub key: [u8; 32],
}

/// The cells of `layout`: the terrain tiles row by row (Z outer), then one per brick.
#[must_use]
pub fn bake_cells(layout: &VolumeLayout, tile_columns: u32) -> Vec<BakeCell> {
    let mut layout = layout.clone();
    layout.assign_brick_offsets();
    let nominal = layout.nominal_positions();
    let half = layout.half_cells();
    let mut cells = Vec::new();
    if let Some(t) = &layout.terrain {
        let n = tile_columns.max(1);
        let tiles = [t.counts[0].div_ceil(n), t.counts[1].div_ceil(n)];
        let levels = t.levels.len() as u32;
        for tj in 0..tiles[1] {
            for ti in 0..tiles[0] {
                let mut probes = Vec::new();
                for j in tj * n..((tj + 1) * n).min(t.counts[1]) {
                    for i in ti * n..((ti + 1) * n).min(t.counts[0]) {
                        probes.extend((0..levels).map(|l| t.index(i, j, l)));
                    }
                }
                cells.push(cell(CellKind::Terrain([ti, tj]), probes, &nominal, &half));
            }
        }
    }
    for (k, b) in layout.bricks.iter().enumerate() {
        let first = b.first_probe as usize;
        let probes = (first..first + b.probe_count()).collect();
        cells.push(cell(CellKind::Brick(k), probes, &nominal, &half));
    }
    cells
}

fn cell(kind: CellKind, probes: Vec<usize>, nominal: &[[f32; 3]], half: &[[f32; 3]]) -> BakeCell {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    let mut h = Sha256::new();
    text(&mut h, "regia-gi-cell");
    h.update(&(probes.len() as u64).to_le_bytes());
    for &i in &probes {
        let (p, r) = (nominal[i], half[i]);
        min = std::array::from_fn(|a| min[a].min(p[a] - r[a]));
        max = std::array::from_fn(|a| max[a].max(p[a] + r[a]));
        pod(&mut h, &p);
        pod(&mut h, &r);
    }
    BakeCell {
        kind,
        probes,
        min,
        max,
        key: h.finish(),
    }
}

/// The hashes of what lights every probe of a volume (module docs). Two bakes with any of them
/// different share no probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlobalHashes {
    /// The state version, the crate version, the tracer backend and the device.
    pub tracer: [u8; 32],
    /// The bake settings (all but the batching), the moment resolution, the distance clamp
    /// and the influence margin.
    pub settings: [u8; 32],
    /// The sun, the sky, the sky bodies, the stars, the media and, when set, the cloud field.
    ///
    /// A cloudless scene does not fold the field, so a state baked before clouds were part of
    /// this hash still matches. Adding, editing or clearing the field changes the hash and the
    /// plan is a full rebake.
    pub sky: [u8; 32],
    /// The heightfield's grid and material, and the terrain view albedo.
    pub terrain: [u8; 32],
    /// The camera.
    pub camera: [u8; 32],
}

impl GlobalHashes {
    /// The names [`Self::first_difference`] returns, in comparison order.
    pub const NAMES: [&'static str; 5] = ["tracer", "settings", "sky", "terrain", "camera"];

    fn parts(&self) -> [&[u8; 32]; 5] {
        [
            &self.tracer,
            &self.settings,
            &self.sky,
            &self.terrain,
            &self.camera,
        ]
    }

    /// The name of the first hash that differs from `other`'s ([`Self::NAMES`]).
    #[must_use]
    pub fn first_difference(&self, other: &Self) -> Option<&'static str> {
        let (ours, theirs) = (self.parts(), other.parts());
        (0..Self::NAMES.len())
            .find(|&k| ours[k] != theirs[k])
            .map(|k| Self::NAMES[k])
    }
}

/// A placed part of the scene with a world box: an instance or a light.
struct Item {
    min: [f32; 3],
    max: [f32; 3],
    hash: [u8; 32],
}

impl Item {
    fn overlaps(&self, lo: [f32; 3], hi: [f32; 3]) -> bool {
        (0..3).all(|a| self.min[a] <= hi[a] && self.max[a] >= lo[a])
    }

    fn finite(&self) -> bool {
        self.min.iter().chain(self.max.iter()).all(|v| v.is_finite())
    }
}

/// Uniform grid over the finite items (Ericson, *Real-Time Collision Detection*, 2005, ch. 7).
/// An item with no finite box, or one that covers most of the grid, is tested with every cell.
struct ItemIndex {
    origin: [f32; 3],
    cell: [f32; 3],
    dims: [usize; 3],
    bins: Vec<Vec<u32>>,
    always: Vec<u32>,
}

fn item_bin(v: f32, origin: f32, cell: f32, n: usize) -> usize {
    let i = ((v - origin) / cell).floor();
    if !i.is_finite() {
        return 0;
    }
    (i as isize).clamp(0, n as isize - 1) as usize
}

impl ItemIndex {
    fn build(items: &[Item]) -> Self {
        const MAX_AXIS: usize = 32;
        let finite: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.finite())
            .map(|(i, _)| i)
            .collect();
        if finite.is_empty() {
            return Self {
                origin: [0.0; 3],
                cell: [1.0; 3],
                dims: [1, 1, 1],
                bins: vec![Vec::new()],
                always: (0..items.len() as u32).collect(),
            };
        }
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        for &i in &finite {
            for a in 0..3 {
                lo[a] = lo[a].min(items[i].min[a]);
                hi[a] = hi[a].max(items[i].max[a]);
            }
        }
        let mut dims = [1usize; 3];
        let mut cell = [1.0f32; 3];
        for a in 0..3 {
            let extent = (hi[a] - lo[a]).max(1.0);
            dims[a] = ((extent / 4.0).ceil() as usize).clamp(1, MAX_AXIS);
            cell[a] = extent / dims[a] as f32;
        }
        let bins_n = dims[0] * dims[1] * dims[2];
        let mut bins = vec![Vec::new(); bins_n];
        let mut always = Vec::new();
        let at = |x: usize, y: usize, z: usize| x + dims[0] * (y + dims[1] * z);
        for (i, it) in items.iter().enumerate() {
            if !it.finite() {
                always.push(i as u32);
                continue;
            }
            let a0: [usize; 3] = std::array::from_fn(|a| item_bin(it.min[a], lo[a], cell[a], dims[a]));
            let a1: [usize; 3] = std::array::from_fn(|a| item_bin(it.max[a], lo[a], cell[a], dims[a]));
            let cover = (a1[0] - a0[0] + 1) * (a1[1] - a0[1] + 1) * (a1[2] - a0[2] + 1);
            if cover * 2 > bins_n {
                always.push(i as u32);
                continue;
            }
            for z in a0[2]..=a1[2] {
                for y in a0[1]..=a1[1] {
                    for x in a0[0]..=a1[0] {
                        bins[at(x, y, z)].push(i as u32);
                    }
                }
            }
        }
        Self {
            origin: lo,
            cell,
            dims,
            bins,
            always,
        }
    }

    /// Item indices whose bin overlaps `lo..hi`, plus the items tested with every cell.
    fn query(&self, lo: [f32; 3], hi: [f32; 3], out: &mut Vec<u32>) {
        out.clear();
        out.extend_from_slice(&self.always);
        let a0: [usize; 3] =
            std::array::from_fn(|a| item_bin(lo[a], self.origin[a], self.cell[a], self.dims[a]));
        let a1: [usize; 3] =
            std::array::from_fn(|a| item_bin(hi[a], self.origin[a], self.cell[a], self.dims[a]));
        let at = |x: usize, y: usize, z: usize| x + self.dims[0] * (y + self.dims[1] * z);
        for z in a0[2]..=a1[2] {
            for y in a0[1]..=a1[1] {
                for x in a0[0]..=a1[0] {
                    out.extend_from_slice(&self.bins[at(x, y, z)]);
                }
            }
        }
    }
}

fn scene_cell_hashes(
    digest: &SceneDigest,
    scene: &PtScene,
    cells: &[BakeCell],
    margin: f32,
) -> Vec<[u8; 32]> {
    let index = ItemIndex::build(&digest.items);
    cells
        .iter()
        .map(|c| digest.cell_hash(scene, c, margin, &index))
        .collect()
}

/// Everything about a scene the incremental bake compares, hashed once.
struct SceneDigest {
    textures: Vec<[u8; 32]>,
    items: Vec<Item>,
    /// Every item and every terrain sample: whether the scene changed anywhere.
    all: [u8; 32],
}

impl SceneDigest {
    fn new(scene: &PtScene) -> Self {
        let _span = tracing::info_span!("pt_scene_digest", instances = scene.instances.len()).entered();
        let textures: Vec<[u8; 32]> = scene
            .textures
            .iter()
            .map(|t| {
                let mut h = Sha256::new();
                debug(&mut h, &(t.width, t.height, t.srgb));
                pod(&mut h, &t.rgba);
                h.finish()
            })
            .collect();
        let meshes: Vec<([u8; 32], Option<Box3>)> = scene
            .meshes
            .iter()
            .map(|m| {
                #[cfg(test)]
                {
                    let bytes = (std::mem::size_of_val(m.positions.as_slice())
                        + std::mem::size_of_val(m.normals.as_slice())
                        + std::mem::size_of_val(m.uvs.as_slice())
                        + std::mem::size_of_val(m.colors.as_slice())
                        + std::mem::size_of_val(m.tangents.as_slice())
                        + std::mem::size_of_val(m.indices.as_slice())) as u64;
                    MESH_BYTES_HASHED.with(|c| c.set(c.get() + bytes));
                }
                let mut h = Sha256::new();
                pod(&mut h, &m.positions);
                pod(&mut h, &m.normals);
                pod(&mut h, &m.uvs);
                pod(&mut h, &m.colors);
                pod(&mut h, &m.tangents);
                pod(&mut h, &m.indices);
                (h.finish(), points_box(m.positions.iter().copied()))
            })
            .collect();
        let mut items = Vec::with_capacity(scene.instances.len() + scene.lights.len());
        for inst in &scene.instances {
            let mut h = Sha256::new();
            text(&mut h, "instance");
            match meshes.get(inst.mesh) {
                Some((mesh, _)) => h.update(mesh),
                None => debug(&mut h, &("missing mesh", inst.mesh)),
            }
            match scene.materials.get(inst.material) {
                Some(m) => material(&mut h, m, &textures),
                None => debug(&mut h, &("missing material", inst.material)),
            }
            let transform = inst.transform.to_cols_array();
            pod(&mut h, &transform);
            let world = match meshes.get(inst.mesh) {
                // An empty mesh lights nothing.
                Some((_, None)) => continue,
                Some((_, Some((lo, hi)))) => {
                    let corners = (0..8).map(|k: usize| {
                        let c = Vec3::new(
                            if k & 1 == 0 { lo[0] } else { hi[0] },
                            if k & 2 == 0 { lo[1] } else { hi[1] },
                            if k & 4 == 0 { lo[2] } else { hi[2] },
                        );
                        inst.transform.transform_point3(c).to_array()
                    });
                    points_box(corners)
                }
                // The tracer refuses a missing mesh; count it everywhere.
                None => None,
            };
            let (min, max) = world.unwrap_or(([f32::NEG_INFINITY; 3], [f32::INFINITY; 3]));
            items.push(Item {
                min,
                max,
                hash: h.finish(),
            });
        }
        for light in &scene.lights {
            let mut h = Sha256::new();
            text(&mut h, "light");
            debug(&mut h, light);
            let (min, max) = light_box(light).unwrap_or(([f32::NEG_INFINITY; 3], [f32::INFINITY; 3]));
            items.push(Item {
                min,
                max,
                hash: h.finish(),
            });
        }
        let mut sorted: Vec<[u8; 32]> = items.iter().map(|i| i.hash).collect();
        sorted.sort_unstable();
        let mut h = Sha256::new();
        text(&mut h, "regia-gi-scene");
        for s in &sorted {
            h.update(s);
        }
        match &scene.terrain {
            Some(t) => pod(&mut h, &t.heights),
            None => text(&mut h, "no terrain"),
        }
        Self {
            textures,
            items,
            all: h.finish(),
        }
    }

    fn cell_hash(&self, scene: &PtScene, cell: &BakeCell, margin: f32, index: &ItemIndex) -> [u8; 32] {
        let lo = cell.min.map(|v| v - margin);
        let hi = cell.max.map(|v| v + margin);
        let mut ids = Vec::new();
        let finite = lo.iter().chain(hi.iter()).all(|v| v.is_finite());
        let mut near: Vec<[u8; 32]> = if finite {
            index.query(lo, hi, &mut ids);
            ids.sort_unstable();
            ids.dedup();
            ids.into_iter()
                .filter(|&id| {
                    #[cfg(test)]
                    CELL_ITEM_TESTS.with(|c| c.set(c.get() + 1));
                    self.items[id as usize].overlaps(lo, hi)
                })
                .map(|id| self.items[id as usize].hash)
                .collect()
        } else {
            self.items
                .iter()
                .filter(|it| {
                    #[cfg(test)]
                    CELL_ITEM_TESTS.with(|c| c.set(c.get() + 1));
                    it.overlaps(lo, hi)
                })
                .map(|it| it.hash)
                .collect()
        };
        near.sort_unstable();
        let mut h = Sha256::new();
        text(&mut h, "regia-gi-cell-scene");
        h.update(&(near.len() as u64).to_le_bytes());
        for n in &near {
            h.update(n);
        }
        if let Some(t) = &scene.terrain {
            terrain_samples(&mut h, t, lo, hi);
        }
        h.finish()
    }

    fn globals(
        &self,
        scene: &PtScene,
        layout: &VolumeLayout,
        settings: &GiBakeSettings,
        inc: &IncrementalSettings,
        backend: &str,
    ) -> GlobalHashes {
        let mut tracer = Sha256::new();
        text(&mut tracer, "regia-gi-incremental");
        tracer.update(&STATE_FORMAT_VERSION.to_le_bytes());
        text(&mut tracer, env!("CARGO_PKG_VERSION"));
        text(&mut tracer, backend);
        text(&mut tracer, &inc.device);

        let mut set = Sha256::new();
        let mut trace = *settings;
        // Neither the batch size nor the samples per GPU submission change a result.
        trace.trace.probes_per_batch = 0;
        trace.trace.samples_per_submit = 0;
        debug(&mut set, &trace);
        set.update(&layout.moment_resolution.to_le_bytes());
        pod(&mut set, &[layout.max_distance, inc.margin(layout)]);

        let mut sky = Sha256::new();
        debug(&mut sky, &scene.sun);
        match &scene.sky {
            PtSky::Table {
                width,
                height,
                texels,
                sun_direction,
                scale,
            } => {
                debug(&mut sky, &("table", width, height, sun_direction, scale));
                pod(&mut sky, texels);
            }
            other => debug(&mut sky, other),
        }
        debug(&mut sky, &scene.sky_bodies);
        debug(&mut sky, &(scene.stars, &scene.media));
        // Only when present, the same rule as `PtScene::content_hash`: a cloudless scene keeps
        // the sky hash a state baked before this fold still matches. The GPU probe shade does
        // read the field, so adding, editing or clearing it must not reuse a clear-sky bake.
        if let Some(clouds) = &scene.clouds {
            debug(&mut sky, clouds);
        }

        let mut terrain = Sha256::new();
        match &scene.terrain {
            Some(t) => {
                debug(&mut terrain, &(t.width, t.depth, t.origin, t.spacing));
                match scene.materials.get(t.material) {
                    Some(m) => material(&mut terrain, m, &self.textures),
                    None => debug(&mut terrain, &("missing material", t.material)),
                }
            }
            None => text(&mut terrain, "no terrain"),
        }
        match &scene.terrain_view_albedo {
            Some(v) => {
                debug(&mut terrain, v);
                for t in [Some(v.texture), v.normal].into_iter().flatten() {
                    texture(&mut terrain, t, &self.textures);
                }
            }
            None => text(&mut terrain, "no view albedo"),
        }

        let mut camera = Sha256::new();
        debug(&mut camera, &scene.camera);
        GlobalHashes {
            tracer: tracer.finish(),
            settings: set.finish(),
            sky: sky.finish(),
            terrain: terrain.finish(),
            camera: camera.finish(),
        }
    }
}

/// The scene hash of every cell of `cells` (module docs): what of `scene` lies within `margin`
/// meters of the cell's probes.
#[must_use]
pub fn cell_scene_hashes(scene: &PtScene, cells: &[BakeCell], margin: f32) -> Vec<[u8; 32]> {
    let digest = SceneDigest::new(scene);
    scene_cell_hashes(&digest, scene, cells, margin)
}

/// The global hashes of a bake of `layout` in `scene` with `settings` on `backend`.
#[must_use]
pub fn global_hashes(
    scene: &PtScene,
    layout: &VolumeLayout,
    settings: &GiBakeSettings,
    inc: &IncrementalSettings,
    backend: &str,
) -> GlobalHashes {
    SceneDigest::new(scene).globals(scene, layout, settings, inc, backend)
}

fn material(h: &mut Sha256, m: &PtMaterial, textures: &[[u8; 32]]) {
    debug(h, m);
    let detail = m.detail.map_or([None, None], |d| [d.image, d.normal]);
    for t in [m.albedo, m.normal, m.emissive_map, m.orm, detail[0], detail[1]]
        .into_iter()
        .flatten()
    {
        texture(h, t, textures);
    }
}

fn texture(h: &mut Sha256, index: usize, textures: &[[u8; 32]]) {
    match textures.get(index) {
        Some(t) => h.update(t),
        None => debug(h, &("missing texture", index)),
    }
}

/// The box of the finite points, `None` when there are none.
fn points_box(points: impl Iterator<Item = [f32; 3]>) -> Option<Box3> {
    points
        .filter(|p| p.iter().all(|v| v.is_finite()))
        .fold(None, |acc, p| match acc {
            None => Some((p, p)),
            Some((lo, hi)) => Some((
                std::array::from_fn(|a| lo[a].min(p[a])),
                std::array::from_fn(|a| hi[a].max(p[a])),
            )),
        })
}

fn light_box(light: &PtLight) -> Option<Box3> {
    let add = |a: [f32; 3], b: [f32; 3]| [a[0] + b[0], a[1] + b[1], a[2] + b[2]];
    match *light {
        PtLight::Point { position, .. } | PtLight::Spot { position, .. } => {
            points_box(std::iter::once(position))
        }
        PtLight::Sphere { position, radius, .. } | PtLight::SphereSpot { position, radius, .. } => {
            let r = radius.abs();
            points_box([position.map(|v| v - r), position.map(|v| v + r)].into_iter())
        }
        PtLight::Quad {
            corner,
            edge_u,
            edge_v,
            ..
        } => points_box(
            [
                corner,
                add(corner, edge_u),
                add(corner, edge_v),
                add(add(corner, edge_u), edge_v),
            ]
            .into_iter(),
        ),
    }
}

/// The heightfield samples under the XZ box `lo..hi`, one sample of margin each side (the
/// triangles that reach into the box).
fn terrain_samples(h: &mut Sha256, t: &PtHeightfield, lo: [f32; 3], hi: [f32; 3]) {
    text(h, "terrain");
    let (w, d) = (t.width as usize, t.depth as usize);
    let usable = w > 0
        && d > 0
        && t.heights.len() >= w * d
        && t.spacing[0] > 0.0
        && t.spacing[1] > 0.0
        && [lo[0], lo[2], hi[0], hi[2]].iter().all(|v| v.is_finite());
    let range = |from: f32, to: f32, origin: f32, spacing: f32, n: usize| -> Option<(usize, usize)> {
        let first = ((from - origin) / spacing).floor() - 1.0;
        let last = ((to - origin) / spacing).ceil() + 1.0;
        // Clamp in usize: `(n - 1) as f32` rounds up for n above 2^24 and would end one past the row.
        let end = n - 1;
        if last >= 0.0 && first <= end as f32 {
            Some(((first.max(0.0) as usize).min(end), (last as usize).min(end)))
        } else {
            None
        }
    };
    let ranges = if usable {
        range(lo[0], hi[0], t.origin[0], t.spacing[0], w).zip(range(
            lo[2],
            hi[2],
            t.origin[1],
            t.spacing[1],
            d,
        ))
    } else {
        None
    };
    match ranges {
        Some(((i0, i1), (j0, j1))) => {
            for v in [i0, i1, j0, j1] {
                h.update(&(v as u64).to_le_bytes());
            }
            for j in j0..=j1 {
                pod(h, &t.heights[j * w + i0..=j * w + i1]);
            }
        }
        None => text(h, "no samples"),
    }
}

fn text(h: &mut Sha256, s: &str) {
    h.update(&(s.len() as u64).to_le_bytes());
    h.update(s.as_bytes());
}

fn pod<T: bytemuck::Pod>(h: &mut Sha256, values: &[T]) {
    h.update(&(values.len() as u64).to_le_bytes());
    h.update(bytemuck::cast_slice(values));
}

fn debug(h: &mut Sha256, value: &impl std::fmt::Debug) {
    // `{:?}` of an f32 is its shortest round-trip form, so the text is exact.
    text(h, &format!("{value:?}"));
}

/// A cell an incremental bake traced, and why.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RebakedCell {
    /// The cell.
    pub cell: CellKind,
    /// Its probes.
    pub probes: usize,
    /// `layout`: the state has no cell with these probes (a new or moved brick, a tile whose
    /// ground changed); `scene`: something within the margin changed; `state`: the stored cell
    /// does not fit the layout.
    pub reason: &'static str,
}

/// What an incremental bake reuses and traces ([`plan_incremental`]).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct IncrementalReport {
    /// Why every cell is baked: `no_state`, or the first global hash that differs
    /// ([`GlobalHashes::NAMES`]); `None` when the cells were compared one by one.
    pub full: Option<&'static str>,
    /// Cells in the volume.
    pub cells: usize,
    /// Cells whose probes come from the state.
    pub cells_reused: usize,
    /// Cells traced.
    pub cells_baked: usize,
    /// Probes that come from the state.
    pub probes_reused: usize,
    /// Probes traced.
    pub probes_baked: usize,
    /// The scene differs from the state's somewhere (a placed mesh, a light, a terrain sample).
    pub scene_changed: bool,
    /// Cells were reused although the scene changed: light from an edit beyond a reused cell's
    /// margin is the old scene's there. A bake without a state is the reference.
    pub approximate: bool,
    /// The influence margin, meters.
    pub influence_margin: f32,
    /// The cells traced and why (empty when [`Self::full`] is set).
    pub baked: Vec<RebakedCell>,
}

/// What an incremental bake will do, without tracing ([`plan_incremental`]).
#[derive(Clone, Debug)]
pub struct IncrementalPlan {
    /// The volume's cells ([`bake_cells`]).
    pub cells: Vec<BakeCell>,
    /// Their scene hashes ([`cell_scene_hashes`]).
    pub scene_hashes: Vec<[u8; 32]>,
    /// The global hashes.
    pub global: GlobalHashes,
    /// The hash of the whole scene's placed content ([`IncrementalReport::scene_changed`]).
    pub scene: [u8; 32],
    /// Per cell, the index of the state's cell whose probes it reuses, `None` when it is traced.
    pub reuse: Vec<Option<usize>>,
    /// The summary.
    pub report: IncrementalReport,
}

/// Compares a bake of `layout` in `scene` with `previous` (module docs) and says which cells
/// it reuses and which it traces, without tracing. `backend` is the tracer's
/// ([`ProbeTracer::backend`]).
#[must_use]
pub fn plan_incremental(
    scene: &PtScene,
    layout: &VolumeLayout,
    settings: &GiBakeSettings,
    previous: Option<&BakeState>,
    inc: &IncrementalSettings,
    backend: &str,
) -> IncrementalPlan {
    let _span = tracing::info_span!("pt_plan_incremental").entered();
    let cells = bake_cells(layout, inc.tile_columns);
    let margin = inc.margin(layout);
    let digest = SceneDigest::new(scene);
    let global = digest.globals(scene, layout, settings, inc, backend);
    // The whole-scene hash already says nothing moved. Copy the stored cell hashes (the first
    // cell of a repeated key wins, the same rule as the reuse lookup) and hash only a new key.
    let scene_hashes: Vec<[u8; 32]> = match previous {
        Some(prev) if global.first_difference(&prev.global).is_none() && prev.scene == digest.all => {
            let mut stored: HashMap<[u8; 32], [u8; 32]> = HashMap::new();
            for cell in prev.cells.iter().rev() {
                stored.insert(cell.key, cell.scene_hash);
            }
            let copied: Vec<Option<[u8; 32]>> = cells.iter().map(|c| stored.get(&c.key).copied()).collect();
            if copied.iter().any(Option::is_none) {
                let index = ItemIndex::build(&digest.items);
                cells
                    .iter()
                    .zip(copied)
                    .map(|(cell, hash)| hash.unwrap_or_else(|| digest.cell_hash(scene, cell, margin, &index)))
                    .collect()
            } else {
                copied.into_iter().flatten().collect()
            }
        }
        _ => scene_cell_hashes(&digest, scene, &cells, margin),
    };
    let mut reuse = vec![None; cells.len()];
    let mut report = IncrementalReport {
        cells: cells.len(),
        influence_margin: margin,
        ..IncrementalReport::default()
    };
    let full = match previous {
        None => Some("no_state"),
        Some(prev) => global.first_difference(&prev.global),
    };
    match (previous, full) {
        (Some(prev), None) => {
            report.scene_changed = prev.scene != digest.all;
            let res = layout.moment_resolution as usize;
            let texels = res.saturating_mul(res);
            // The first stored cell wins when two have the same key (two identical bricks).
            let by_key: HashMap<[u8; 32], usize> = prev
                .cells
                .iter()
                .enumerate()
                .rev()
                .map(|(k, c)| (c.key, k))
                .collect();
            for (c, cell) in cells.iter().enumerate() {
                let stored = by_key.get(&cell.key).map(|&k| (k, &prev.cells[k]));
                let reason = match stored {
                    None => Some("layout"),
                    Some((_, s))
                        if s.probes.len() != cell.probes.len()
                            || s.probes.iter().any(|p| p.data.moments.len() != texels) =>
                    {
                        Some("state")
                    }
                    Some((_, s)) if s.scene_hash != scene_hashes[c] => Some("scene"),
                    Some((k, _)) => {
                        reuse[c] = Some(k);
                        None
                    }
                };
                if let Some(reason) = reason {
                    report.baked.push(RebakedCell {
                        cell: cell.kind,
                        probes: cell.probes.len(),
                        reason,
                    });
                }
            }
        }
        _ => report.full = full,
    }
    for (cell, r) in cells.iter().zip(&reuse) {
        if r.is_some() {
            report.cells_reused += 1;
            report.probes_reused += cell.probes.len();
        } else {
            report.cells_baked += 1;
            report.probes_baked += cell.probes.len();
        }
    }
    report.approximate = report.scene_changed && report.cells_reused > 0;
    IncrementalPlan {
        cells,
        scene_hashes,
        global,
        scene: digest.all,
        reuse,
        report,
    }
}

/// An incremental bake's result: the bake, the state to keep for the next one, and what was
/// reused.
#[derive(Clone, Debug)]
pub struct IncrementalBake {
    /// The bake ([`crate::probe::bake_gi_volume`]'s result shape).
    pub bake: GiBake,
    /// The state to pass to the next bake.
    pub state: BakeState,
    /// What was reused and traced.
    pub report: IncrementalReport,
}

/// Bakes `layout` in `scene`, tracing only the cells [`plan_incremental`] does not reuse from
/// `previous` (module docs). With no state it is [`crate::probe::bake_gi_volume`] bit for bit;
/// with the state of a bake of the same scene it traces nothing and gives that bake's volume
/// bit for bit. `progress` is [`crate::probe::bake_gi_volume`]'s, over the traced probes.
///
/// # Errors
///
/// An inconsistent layout, a trace error, or cancellation.
pub fn bake_gi_volume_incremental<T: ProbeTracer + ?Sized>(
    tracer: &T,
    scene: &PtScene,
    layout: &VolumeLayout,
    settings: &GiBakeSettings,
    previous: Option<&BakeState>,
    inc: &IncrementalSettings,
    progress: impl FnMut(&str, usize, usize) -> bool,
) -> Result<IncrementalBake, PtError> {
    let _span = tracing::info_span!("pt_bake_gi_incremental").entered();
    let started = Instant::now();
    let mut layout = layout.clone();
    layout.assign_brick_offsets();
    let plan = plan_incremental(scene, &layout, settings, previous, inc, tracer.backend());
    let mut traced_indices: Vec<usize> = plan
        .cells
        .iter()
        .zip(&plan.reuse)
        .filter(|(_, r)| r.is_none())
        .flat_map(|(c, _)| c.probes.iter().copied())
        .collect();
    traced_indices.sort_unstable();
    let (traced, stats) =
        bake_probes_with_stats(tracer, scene, &layout, settings, &traced_indices, None, progress)?;
    let mut slots: Vec<Option<BakedProbe>> = vec![None; layout.probe_count()];
    if let Some(prev) = previous {
        for (cell, reuse) in plan.cells.iter().zip(&plan.reuse) {
            if let Some(stored) = reuse.and_then(|k| prev.cells.get(k)) {
                for (&i, p) in cell.probes.iter().zip(&stored.probes) {
                    slots[i] = Some(p.clone());
                }
            }
        }
    }
    for (&i, p) in traced_indices.iter().zip(traced) {
        slots[i] = Some(p);
    }
    let probes: Vec<BakedProbe> = slots
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| PtError::Scene("a probe belongs to no bake cell".into()))?;
    let mut bake = finish_bake(scene, &layout, settings, &probes)?;
    stats.apply(&mut bake.report);
    bake.report.seconds = started.elapsed().as_secs_f64();
    let cells = plan
        .cells
        .iter()
        .zip(&plan.scene_hashes)
        .map(|(c, h)| CellState {
            key: c.key,
            scene_hash: *h,
            probes: c.probes.iter().map(|&i| probes[i].clone()).collect(),
        })
        .collect();
    tracing::info!(
        target: "regia_pt",
        cells = plan.report.cells,
        reused = plan.report.cells_reused,
        probes_baked = plan.report.probes_baked,
        seconds = bake.report.seconds,
        "incremental gi bake done"
    );
    Ok(IncrementalBake {
        bake,
        state: BakeState {
            global: plan.global,
            scene: plan.scene,
            cells,
        },
        report: plan.report,
    })
}

/// One cell of a [`BakeState`].
#[derive(Clone, Debug, PartialEq)]
pub struct CellState {
    /// [`BakeCell::key`].
    pub key: [u8; 32],
    /// The cell's scene hash when it was baked.
    pub scene_hash: [u8; 32],
    /// Its probes, in [`BakeCell::probes`] order.
    pub probes: Vec<BakedProbe>,
}

/// What an incremental bake keeps for the next one (module docs; file `FGIS`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BakeState {
    /// The global hashes of the bake.
    pub global: GlobalHashes,
    /// The whole scene's hash ([`IncrementalPlan::scene`]).
    pub scene: [u8; 32],
    /// Every cell of the bake.
    pub cells: Vec<CellState>,
}

/// Bytes of a probe record without its moment texels.
const PROBE_FIXED_BYTES: usize = GI_COMPONENTS * 12 * 4 + 4 + 3 * 4 + 2 + 3 * 4 + 3 * 4;

impl BakeState {
    /// Probes in the state.
    #[must_use]
    pub fn probe_count(&self) -> usize {
        self.cells.iter().map(|c| c.probes.len()).sum()
    }

    /// The state file's bytes: `FGIS`, the version, the hashes, the cells with their probes at
    /// full precision (little-endian), and a SHA-256 of everything before it.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&STATE_FORMAT_VERSION.to_le_bytes());
        for part in self.global.parts() {
            out.extend_from_slice(part);
        }
        out.extend_from_slice(&self.scene);
        out.extend_from_slice(&(self.cells.len() as u32).to_le_bytes());
        let f = |out: &mut Vec<u8>, v: f32| out.extend_from_slice(&v.to_le_bytes());
        for c in &self.cells {
            out.extend_from_slice(&c.key);
            out.extend_from_slice(&c.scene_hash);
            out.extend_from_slice(&(c.probes.len() as u32).to_le_bytes());
            for p in &c.probes {
                for v in p.data.sh.iter().flatten().flatten() {
                    f(&mut out, *v);
                }
                out.extend_from_slice(&(p.data.moments.len() as u32).to_le_bytes());
                for m in &p.data.moments {
                    f(&mut out, m[0]);
                    f(&mut out, m[1]);
                }
                for v in p.data.meta.offset {
                    f(&mut out, v);
                }
                out.extend_from_slice(&p.data.meta.flags.to_le_bytes());
                for v in p.position {
                    f(&mut out, v);
                }
                let q = &p.quality;
                for v in [
                    p.rays,
                    p.back_faces,
                    p.rejected,
                    p.escaped,
                    q.clamped,
                    q.suppressed_groups,
                ] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                // No noise estimate (one sample group) is stored as NaN.
                for v in [q.clamp_loss, q.firefly_loss, q.noise.unwrap_or(f32::NAN)] {
                    f(&mut out, v);
                }
            }
        }
        let mut h = Sha256::new();
        h.update(&out);
        out.extend_from_slice(&h.finish());
        out
    }

    /// Reads [`Self::to_bytes`].
    ///
    /// # Errors
    ///
    /// [`PtError::Io`]: not a state file, another version, a failed checksum, or a malformed
    /// record.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PtError> {
        decode_state(bytes).map_err(|e| PtError::Io(format!("bake state: {e}")))
    }

    /// Writes the state to `path` (a temporary file in the same folder, then a rename; the
    /// folder is created) and returns the bytes written.
    ///
    /// # Errors
    ///
    /// [`PtError::Io`] when the folder, the file or the rename fails.
    pub fn write(&self, path: &Path) -> Result<u64, PtError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)
                .map_err(|e| PtError::Io(format!("cannot create {}: {e}", dir.display())))?;
        }
        let data = self.to_bytes();
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(
            ".tmp-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, &data)
            .map_err(|e| PtError::Io(format!("cannot write {}: {e}", tmp.display())))?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(PtError::Io(format!(
                "cannot rename {} to {}: {e}",
                tmp.display(),
                path.display()
            )));
        }
        Ok(data.len() as u64)
    }

    /// Reads the state at `path`; `Ok(None)` when there is no file.
    ///
    /// # Errors
    ///
    /// [`PtError::Io`] when the file cannot be read or is not a usable state
    /// ([`Self::from_bytes`]).
    pub fn read(path: &Path) -> Result<Option<Self>, PtError> {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_bytes(&bytes)
                .map(Some)
                .map_err(|e| PtError::Io(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(PtError::Io(format!("cannot read {}: {e}", path.display()))),
        }
    }
}

/// A little-endian reader; `None` past the end.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }

    fn u16(&mut self) -> Option<u16> {
        self.take::<2>().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Option<u32> {
        self.take::<4>().map(u32::from_le_bytes)
    }

    fn f32(&mut self) -> Option<f32> {
        self.take::<4>().map(f32::from_le_bytes)
    }

    fn vec3(&mut self) -> Option<[f32; 3]> {
        Some([self.f32()?, self.f32()?, self.f32()?])
    }
}

fn decode_state(bytes: &[u8]) -> Result<BakeState, String> {
    let mut r = Reader(bytes);
    if r.take::<4>().as_ref() != Some(STATE_MAGIC) {
        return Err("not a bake state file".into());
    }
    let version = r.u32().ok_or("truncated header")?;
    if version != STATE_FORMAT_VERSION {
        return Err(format!(
            "state version {version}, this build reads {STATE_FORMAT_VERSION}"
        ));
    }
    let body = bytes.len().saturating_sub(CHECKSUM_BYTES);
    if body < 8 {
        return Err("truncated".into());
    }
    let mut h = Sha256::new();
    h.update(&bytes[..body]);
    if h.finish()[..] != bytes[body..] {
        return Err("the checksum does not match (a damaged or truncated file)".into());
    }
    let mut r = Reader(&bytes[8..body]);
    let truncated = || "truncated".to_string();
    let mut hash = || r.take::<32>().ok_or_else(truncated);
    let global = GlobalHashes {
        tracer: hash()?,
        settings: hash()?,
        sky: hash()?,
        terrain: hash()?,
        camera: hash()?,
    };
    let scene = hash()?;
    let count = r.u32().ok_or_else(truncated)? as usize;
    if count > r.0.len() / 68 {
        return Err(format!("{count} cells do not fit the file"));
    }
    let mut cells = Vec::with_capacity(count);
    for _ in 0..count {
        let key = r.take::<32>().ok_or_else(truncated)?;
        let scene_hash = r.take::<32>().ok_or_else(truncated)?;
        let probes = r.u32().ok_or_else(truncated)? as usize;
        if probes > r.0.len() / PROBE_FIXED_BYTES {
            return Err(format!("{probes} probes do not fit the file"));
        }
        let mut out = Vec::with_capacity(probes);
        for _ in 0..probes {
            let mut sh: [ShRgb; GI_COMPONENTS] = [[[0.0; 4]; 3]; GI_COMPONENTS];
            for v in sh.iter_mut().flatten().flatten() {
                *v = r.f32().ok_or_else(truncated)?;
            }
            let texels = r.u32().ok_or_else(truncated)? as usize;
            if texels > r.0.len() / 8 {
                return Err(truncated());
            }
            let mut moments = Vec::with_capacity(texels);
            for _ in 0..texels {
                moments.push([r.f32().ok_or_else(truncated)?, r.f32().ok_or_else(truncated)?]);
            }
            let offset = r.vec3().ok_or_else(truncated)?;
            let flags = r.u16().ok_or_else(truncated)?;
            let position = r.vec3().ok_or_else(truncated)?;
            let rays = r.u32().ok_or_else(truncated)?;
            let back_faces = r.u32().ok_or_else(truncated)?;
            let rejected = r.u32().ok_or_else(truncated)?;
            let escaped = r.u32().ok_or_else(truncated)?;
            let clamped = r.u32().ok_or_else(truncated)?;
            let suppressed_groups = r.u32().ok_or_else(truncated)?;
            let clamp_loss = r.f32().ok_or_else(truncated)?;
            let firefly_loss = r.f32().ok_or_else(truncated)?;
            let noise = r.f32().ok_or_else(truncated)?;
            out.push(BakedProbe {
                data: ProbeData {
                    sh,
                    moments,
                    meta: ProbeMeta { offset, flags },
                },
                position,
                rays,
                back_faces,
                rejected,
                escaped,
                quality: ProbeQuality {
                    clamped,
                    clamp_loss,
                    suppressed_groups,
                    firefly_loss,
                    noise: (!noise.is_nan()).then_some(noise),
                },
            });
        }
        cells.push(CellState {
            key,
            scene_hash,
            probes: out,
        });
    }
    if !r.0.is_empty() {
        return Err(format!("{} bytes after the last cell", r.0.len()));
    }
    Ok(BakeState { global, scene, cells })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use regia_core::glam::Mat4;

    use super::*;
    use crate::cpu_probe::CpuProbeTracer;
    use crate::probe::{ProbeTrace, ProbeTraceSettings, bake_gi_volume, detail_brick, terrain_layer};
    use crate::scene::{PtCamera, PtMesh, PtSun};

    /// The CPU tracer, recording how many probes every trace call got.
    #[derive(Default)]
    struct Counting {
        inner: CpuProbeTracer,
        calls: RefCell<Vec<usize>>,
    }

    impl ProbeTracer for Counting {
        fn trace_streams(
            &self,
            scene: &PtScene,
            positions: &[[f32; 3]],
            streams: &[u32],
            settings: &ProbeTraceSettings,
            progress: &mut dyn FnMut(usize, usize) -> bool,
        ) -> Result<Vec<ProbeTrace>, PtError> {
            self.calls.borrow_mut().push(positions.len());
            self.inner
                .trace_streams(scene, positions, streams, settings, progress)
        }

        fn backend(&self) -> &'static str {
            "cpu"
        }
    }

    impl Counting {
        fn traced(&self) -> Vec<usize> {
            self.calls.borrow().clone()
        }
    }

    /// An axis-aligned box `min..max`, faces outward.
    fn box_mesh(min: [f32; 3], max: [f32; 3]) -> PtMesh {
        let c = |i: usize| {
            [
                if i & 1 == 0 { min[0] } else { max[0] },
                if i & 2 == 0 { min[1] } else { max[1] },
                if i & 4 == 0 { min[2] } else { max[2] },
            ]
        };
        let positions: Vec<[f32; 3]> = (0..8).map(c).collect();
        let faces: [[u32; 4]; 6] = [
            [0, 4, 6, 2],
            [1, 3, 7, 5],
            [0, 1, 5, 4],
            [2, 6, 7, 3],
            [0, 2, 3, 1],
            [4, 5, 7, 6],
        ];
        let indices = faces
            .iter()
            .flat_map(|f| [f[0], f[1], f[2], f[0], f[2], f[3]])
            .collect();
        PtMesh {
            normals: vec![[0.0; 3]; positions.len()],
            positions,
            indices,
            ..PtMesh::default()
        }
    }

    fn sun() -> PtSun {
        PtSun {
            toward: [0.5, 0.8, 0.3],
            color: [1.0; 3],
            illuminance: 1000.0,
            angular_radius: 0.0,
        }
    }

    /// A floor under a uniform sky and a sun, and a unit box whose low X face is at `box_x`.
    fn scene(box_x: f32) -> PtScene {
        let mut s = PtScene::new(PtCamera::pinhole([0.0, 5.0, 10.0], [0.0; 3], 1.0, 8, 8));
        s.sky = PtSky::Uniform([0.5, 0.6, 0.8]);
        s.sun = Some(sun());
        let grey = s.add_material(crate::PtMaterial::lambert([0.5; 3]));
        let floor = s.add_mesh(PtMesh {
            positions: vec![
                [-100.0, 0.0, -100.0],
                [100.0, 0.0, -100.0],
                [-100.0, 0.0, 100.0],
                [100.0, 0.0, 100.0],
            ],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            indices: vec![0, 2, 1, 1, 2, 3],
            ..PtMesh::default()
        });
        s.add_instance(floor, grey, Mat4::IDENTITY);
        let red = s.add_material(crate::PtMaterial::lambert([0.8, 0.1, 0.1]));
        let cube = s.add_mesh(box_mesh([0.0; 3], [1.0; 3]));
        s.add_instance(cube, red, Mat4::from_translation(Vec3::new(box_x, 0.0, 0.0)));
        s
    }

    /// Brick A (8 probes) next to the box near the origin, brick B (8 probes) 40 m away.
    fn layout(bricks: &[[f32; 3]]) -> VolumeLayout {
        let mut l = VolumeLayout {
            terrain: None,
            bricks: bricks
                .iter()
                .map(|o| detail_brick(*o, [o[0] + 1.0, o[1] + 1.0, o[2] + 1.0], 1.0))
                .collect(),
            moment_resolution: 4,
            max_distance: 8.0,
        };
        l.assign_brick_offsets();
        l
    }

    const A: [f32; 3] = [-1.5, 0.5, 0.0];
    const B: [f32; 3] = [40.0, 0.5, 0.0];

    fn settings() -> GiBakeSettings {
        GiBakeSettings {
            trace: ProbeTraceSettings {
                rays_per_probe: 32,
                samples: 1,
                max_bounces: 2,
                probes_per_batch: 64,
                ..ProbeTraceSettings::default()
            },
            relocation_iterations: 1,
            ..GiBakeSettings::default()
        }
    }

    fn inc() -> IncrementalSettings {
        IncrementalSettings {
            influence_margin: Some(4.0),
            device: "cpu (test)".into(),
            ..IncrementalSettings::default()
        }
    }

    fn bake(
        tracer: &dyn ProbeTracer,
        s: &PtScene,
        l: &VolumeLayout,
        previous: Option<&BakeState>,
    ) -> IncrementalBake {
        bake_gi_volume_incremental(tracer, s, l, &settings(), previous, &inc(), |_, _, _| true).expect("bake")
    }

    fn bytes(b: &IncrementalBake) -> Vec<u8> {
        b.bake.volume.to_bytes().expect("encode")
    }

    /// Without a state every cell is traced and the result is `bake_gi_volume`'s, bit for bit
    /// (the split into `bake_probes` and `finish_bake` changes nothing).
    #[test]
    fn without_a_state_the_bake_is_the_full_bake() {
        let (s, l, t) = (scene(0.0), layout(&[A, B]), CpuProbeTracer::with_threads(2));
        let full = bake_gi_volume(&t, &s, &l, &settings(), |_, _, _| true).expect("full");
        let first = bake(&t, &s, &l, None);
        assert_eq!(bytes(&first), full.volume.to_bytes().expect("encode"));
        assert_eq!(first.bake.raw, full.raw);
        assert_eq!(first.bake.positions, full.positions);
        // The wall-clock fields differ between any two runs.
        let plain = |r: &crate::probe::GiBakeReport| crate::probe::GiBakeReport {
            seconds: 0.0,
            relocation_seconds: 0.0,
            trace_seconds: 0.0,
            encode_seconds: 0.0,
            ..r.clone()
        };
        assert_eq!(plain(&first.bake.report), plain(&full.report));
        let r = &first.report;
        assert_eq!(r.full, Some("no_state"));
        assert_eq!(
            (r.cells, r.cells_baked, r.probes_baked, r.cells_reused),
            (2, 2, 16, 0)
        );
        assert!(r.baked.is_empty(), "a full bake lists no cells");
        assert!(!r.approximate);
        assert_eq!(first.state.probe_count(), 16);
        assert_eq!(first.state.cells.len(), 2);
    }

    /// A rebake of the same scene traces nothing and rebuilds the volume and the state bit for
    /// bit.
    #[test]
    fn an_unchanged_scene_traces_nothing() {
        let (s, l) = (scene(0.0), layout(&[A, B]));
        let first = bake(&CpuProbeTracer::new(), &s, &l, None);
        let counting = Counting::default();
        let again = bake(&counting, &s, &l, Some(&first.state));
        assert!(counting.traced().is_empty(), "{:?}", counting.traced());
        assert_eq!(bytes(&again), bytes(&first));
        assert_eq!(again.state, first.state);
        let r = &again.report;
        assert_eq!(r.full, None);
        assert_eq!((r.cells_reused, r.probes_reused, r.cells_baked), (2, 16, 0));
        assert!(!r.scene_changed && !r.approximate);
        assert_eq!(r.influence_margin, 4.0);
    }

    /// Moving the box near brick A rebakes A only. B's probes are the first bake's bit for bit;
    /// A's are a full bake's of the edited scene bit for bit (A is the volume's first cell, so
    /// its probes keep their random streams). The report names A, `scene`, and says the result
    /// is approximate (B kept the old scene's light).
    #[test]
    fn an_edit_rebakes_only_the_cells_it_reaches() {
        let l = layout(&[A, B]);
        let t = CpuProbeTracer::new();
        let first = bake(&t, &scene(0.0), &l, None);
        let moved = scene(0.5);
        let counting = Counting::default();
        let second = bake(&counting, &moved, &l, Some(&first.state));
        assert_eq!(
            counting.traced(),
            [8, 8],
            "one relocation round and the main trace of A"
        );
        assert_eq!(
            second.report.baked,
            [RebakedCell {
                cell: CellKind::Brick(0),
                probes: 8,
                reason: "scene",
            }]
        );
        assert!(second.report.scene_changed && second.report.approximate);
        assert_eq!(second.bake.raw[8..], first.bake.raw[8..]);
        let full = bake_gi_volume(&t, &moved, &l, &settings(), |_, _, _| true).expect("full");
        assert_eq!(second.bake.raw[..8], full.raw[..8]);
        assert_ne!(
            second.bake.raw[..8],
            first.bake.raw[..8],
            "the box moved next to A"
        );
        // Its state is the edited scene's: a third bake traces nothing.
        let third = Counting::default();
        bake(&third, &moved, &l, Some(&second.state));
        assert!(third.traced().is_empty());
    }

    /// A change that lights every probe rebakes everything and names the hash: the seed, the
    /// sun, the device, the camera.
    #[test]
    fn a_global_change_rebakes_every_cell() {
        let (s, l, t) = (scene(0.0), layout(&[A, B]), CpuProbeTracer::new());
        let first = bake(&t, &s, &l, None);
        let plan = |s: &PtScene, set: &GiBakeSettings, inc: &IncrementalSettings| {
            plan_incremental(s, &l, set, Some(&first.state), inc, "cpu").report
        };
        assert_eq!(plan(&s, &settings(), &inc()).full, None);
        let mut reseeded = settings();
        reseeded.trace.seed += 1;
        assert_eq!(plan(&s, &reseeded, &inc()).full, Some("settings"));
        let mut batched = settings();
        batched.trace.probes_per_batch = 3;
        batched.trace.samples_per_submit = 1;
        assert_eq!(
            plan(&s, &batched, &inc()).full,
            None,
            "the batching changes no result"
        );
        let mut lit = s.clone();
        lit.sun = Some(PtSun {
            illuminance: 2000.0,
            ..sun()
        });
        let r = plan(&lit, &settings(), &inc());
        assert_eq!((r.full, r.cells_baked, r.probes_baked), (Some("sky"), 2, 16));
        let other = IncrementalSettings {
            device: "another gpu".into(),
            ..inc()
        };
        assert_eq!(plan(&s, &settings(), &other).full, Some("tracer"));
        let wider = IncrementalSettings {
            influence_margin: Some(9.0),
            ..inc()
        };
        assert_eq!(plan(&s, &settings(), &wider).full, Some("settings"));
        let mut seen = s.clone();
        seen.camera.eye[0] += 1.0;
        assert_eq!(plan(&seen, &settings(), &inc()).full, Some("camera"));
        assert_eq!(
            plan_incremental(&s, &l, &settings(), Some(&first.state), &inc(), "gpu")
                .report
                .full,
            Some("tracer")
        );
    }

    /// Adding a brick before the others traces only the new brick, though every later probe
    /// index moved; removing one traces nothing.
    #[test]
    fn layout_edits_keep_the_cells_that_did_not_move() {
        let (s, t) = (scene(0.0), CpuProbeTracer::new());
        let first = bake(&t, &s, &layout(&[A, B]), None);
        let c = [-40.0, 0.5, 0.0];
        let counting = Counting::default();
        let added = bake(&counting, &s, &layout(&[c, A, B]), Some(&first.state));
        assert_eq!(counting.traced(), [8, 8]);
        assert_eq!(
            added.report.baked,
            [RebakedCell {
                cell: CellKind::Brick(0),
                probes: 8,
                reason: "layout",
            }]
        );
        assert!(!added.report.scene_changed && !added.report.approximate);
        assert_eq!(added.bake.raw[8..], first.bake.raw[..]);
        let counting = Counting::default();
        let removed = bake(&counting, &s, &layout(&[B]), Some(&added.state));
        assert!(counting.traced().is_empty());
        assert_eq!(removed.bake.raw[..], first.bake.raw[8..]);
        assert_eq!(removed.report.cells_reused, 1);
    }

    /// A heightfield bump in one corner of the map rebakes the one terrain tile whose columns it
    /// lifts; the tiles beyond the margin are reused.
    #[test]
    fn a_terrain_edit_rebakes_the_nearby_tiles() {
        let mut s = PtScene::new(PtCamera::pinhole([0.0, 30.0, 30.0], [0.0; 3], 1.0, 8, 8));
        s.sky = PtSky::Uniform([0.5, 0.6, 0.8]);
        s.sun = Some(sun());
        let ground = s.add_material(crate::PtMaterial::lambert([0.4; 3]));
        s.terrain = Some(PtHeightfield {
            width: 41,
            depth: 41,
            origin: [-20.0, -20.0],
            spacing: [1.0, 1.0],
            heights: vec![0.0; 41 * 41],
            material: ground,
        });
        let hf = s.terrain.clone().expect("terrain");
        let l = VolumeLayout {
            terrain: Some(terrain_layer(&hf, [2.0, 2.0], &[1.0, 3.0]).expect("layer")),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 8.0,
        };
        let inc = IncrementalSettings {
            influence_margin: Some(2.0),
            tile_columns: 5,
            ..inc()
        };
        let set = settings();
        let first =
            bake_gi_volume_incremental(&CpuProbeTracer::new(), &s, &l, &set, None, &inc, |_, _, _| true)
                .expect("first");
        assert_eq!(first.report.cells, 16, "20 × 20 columns in tiles of 5");

        let mut bumped = s.clone();
        if let Some(t) = &mut bumped.terrain {
            for j in 34..41 {
                for i in 34..41 {
                    t.heights[j * 41 + i] += 0.5;
                }
            }
        }
        // The layer follows the new ground, as the bake description rebuilds it.
        let hf = bumped.terrain.clone().expect("terrain");
        let l2 = VolumeLayout {
            terrain: Some(terrain_layer(&hf, [2.0, 2.0], &[1.0, 3.0]).expect("layer")),
            ..l.clone()
        };
        let counting = Counting::default();
        let second = bake_gi_volume_incremental(
            &counting,
            &bumped,
            &l2,
            &set,
            Some(&first.state),
            &inc,
            |_, _, _| true,
        )
        .expect("second");
        assert_eq!(
            second.report.baked,
            [RebakedCell {
                cell: CellKind::Terrain([3, 3]),
                probes: 50,
                reason: "layout",
            }]
        );
        assert_eq!(counting.traced(), [50, 50]);
        assert_eq!(second.report.cells_reused, 15);
        let tile = |b: &IncrementalBake, kind: CellKind| {
            let cells = bake_cells(&l, 5);
            let cell = cells.iter().find(|c| c.kind == kind).expect("tile");
            cell.probes
                .iter()
                .map(|&i| b.bake.raw[i].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            tile(&second, CellKind::Terrain([0, 0])),
            tile(&first, CellKind::Terrain([0, 0]))
        );
        assert_ne!(
            tile(&second, CellKind::Terrain([3, 3])),
            tile(&first, CellKind::Terrain([3, 3]))
        );

        // The same bump without moving the layer's probes: the tile is dirty for its scene.
        let plan = plan_incremental(&bumped, &l, &set, Some(&first.state), &inc, "cpu");
        assert_eq!(plan.report.baked.len(), 1);
        assert_eq!(plan.report.baked[0].reason, "scene");
    }

    /// A cell's scene hash ignores the order of the instances and what lies beyond the margin,
    /// and follows a moved mesh, a recolored material and a light inside it.
    #[test]
    fn cell_hashes_follow_what_is_near() {
        let l = layout(&[A, B]);
        let cells = bake_cells(&l, DEFAULT_TILE_COLUMNS);
        let s = scene(0.0);
        let base = cell_scene_hashes(&s, &cells, 4.0);
        let mut reordered = s.clone();
        reordered.instances.reverse();
        assert_eq!(cell_scene_hashes(&reordered, &cells, 4.0), base);
        let mut far = s.clone();
        far.instances[1].transform = Mat4::from_translation(Vec3::new(20.0, 0.0, 0.0));
        let h = cell_scene_hashes(&far, &cells, 4.0);
        assert_ne!(h[0], base[0], "the box left A");
        assert_eq!(h[1], base[1], "and is still beyond B's margin");
        let mut recolored = s.clone();
        recolored.materials[1].base_color[1] = 0.5;
        let h = cell_scene_hashes(&recolored, &cells, 4.0);
        assert!(h[0] != base[0] && h[1] == base[1]);
        let mut lamp = s.clone();
        lamp.lights.push(PtLight::Point {
            position: [41.0, 1.0, 0.5],
            intensity: [100.0; 3],
        });
        let h = cell_scene_hashes(&lamp, &cells, 4.0);
        assert!(h[0] == base[0] && h[1] != base[1]);
        // The floor spans both bricks: a new floor color dirties both.
        let mut floor = s.clone();
        floor.materials[0].base_color[0] = 0.9;
        let h = cell_scene_hashes(&floor, &cells, 4.0);
        assert!(h[0] != base[0] && h[1] != base[1]);
    }

    /// The cells of a terrain layer and bricks: tiles row by row, partial tiles at the edges,
    /// then one per brick; every probe in exactly one cell; keys follow the probes, not the
    /// volume index.
    #[test]
    fn cells_cover_every_probe_once() {
        let hf = PtHeightfield {
            width: 15,
            depth: 9,
            origin: [0.0, 0.0],
            spacing: [1.0, 1.0],
            heights: vec![0.0; 15 * 9],
            material: 0,
        };
        let mut l = layout(&[A, B]);
        l.terrain = Some(terrain_layer(&hf, [1.0, 1.0], &[1.0, 2.0, 4.0]).expect("layer"));
        l.assign_brick_offsets();
        let cells = bake_cells(&l, 4);
        // 14 × 8 columns: tiles 4 × 2, the last column of tiles 2 wide.
        let tiles: Vec<CellKind> = cells.iter().map(|c| c.kind).collect();
        assert_eq!(tiles.len(), 4 * 2 + 2);
        assert_eq!(tiles[3], CellKind::Terrain([3, 0]));
        assert_eq!(tiles[8], CellKind::Brick(0));
        assert_eq!(cells[3].probes.len(), 2 * 4 * 3);
        let mut seen = vec![0u32; l.probe_count()];
        for c in &cells {
            assert!(c.probes.windows(2).all(|w| w[0] < w[1]));
            for &i in &c.probes {
                seen[i] += 1;
            }
        }
        assert!(seen.iter().all(|&n| n == 1));
        let alone = bake_cells(&layout(&[B]), 4);
        assert_eq!(alone[0].key, cells[9].key, "B keeps its key at another index");
        assert_ne!(cells[8].key, cells[9].key);
    }

    /// The state file round trips bit for bit through a file; a missing file is `None`; a
    /// truncated, damaged, foreign or newer file is refused.
    #[test]
    fn the_state_file_round_trips_and_refuses_damage() {
        let first = bake(&CpuProbeTracer::new(), &scene(0.0), &layout(&[A, B]), None);
        let data = first.state.to_bytes();
        assert_eq!(BakeState::from_bytes(&data).expect("decode"), first.state);
        let dir = regia_test_utils::unique_temp_dir("regia_pt_bake_state");
        let path = dir.join("nested").join("volume.fgis");
        assert_eq!(first.state.write(&path).expect("write"), data.len() as u64);
        assert_eq!(BakeState::read(&path).expect("read"), Some(first.state.clone()));
        assert_eq!(BakeState::read(&dir.join("missing.fgis")).expect("missing"), None);
        assert!(
            BakeState::from_bytes(&data[..data.len() - 1]).is_err(),
            "truncated"
        );
        assert!(BakeState::from_bytes(&data[..20]).is_err(), "header only");
        let mut flipped = data.clone();
        flipped[200] ^= 1;
        let err = BakeState::from_bytes(&flipped).expect_err("damaged");
        assert!(err.to_string().contains("checksum"), "{err}");
        let mut newer = data.clone();
        newer[4] = 2;
        let err = BakeState::from_bytes(&newer).expect_err("newer");
        assert!(err.to_string().contains("version"), "{err}");
        assert!(BakeState::from_bytes(b"FGIVxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").is_err());
        std::fs::write(&path, &flipped).expect("damage the file");
        assert!(BakeState::read(&path).is_err());
        let empty = BakeState::default();
        assert_eq!(BakeState::from_bytes(&empty.to_bytes()).expect("empty"), empty);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An overcast layer: coverage and density above 0, uniform through the shell.
    fn overcast(coverage: f32) -> crate::scene::PtCloudField {
        let mut settings = regia_render::CloudSettings::coverage_layer(coverage);
        settings.density = 0.08;
        crate::scene::PtCloudField {
            density: crate::scene::PtCloudDensity::Uniform(1.0),
            ..crate::scene::PtCloudField::from_settings(settings, 6.36e7)
        }
    }

    /// A state whose cells match `scene`, so a plan reuses them when the globals match too.
    fn state_of(scene: &PtScene, layout: &VolumeLayout) -> BakeState {
        let plan = plan_incremental(scene, layout, &settings(), None, &inc(), "cpu");
        let texels = (layout.moment_resolution as usize).saturating_mul(layout.moment_resolution as usize);
        let probe = BakedProbe {
            data: ProbeData {
                sh: [[[0.0; 4]; 3]; GI_COMPONENTS],
                moments: vec![[1.0, 1.0]; texels],
                meta: ProbeMeta::default(),
            },
            position: [0.0; 3],
            rays: 1,
            back_faces: 0,
            rejected: 0,
            escaped: 0,
            quality: ProbeQuality::default(),
        };
        BakeState {
            global: plan.global,
            scene: plan.scene,
            cells: plan
                .cells
                .iter()
                .zip(&plan.scene_hashes)
                .map(|(cell, scene_hash)| CellState {
                    key: cell.key,
                    scene_hash: *scene_hash,
                    probes: vec![probe.clone(); cell.probes.len()],
                })
                .collect(),
        }
    }

    /// Two scenes that differ only by a cloud field must not share a sky hash, and the plan
    /// against the cloudless state must be a full rebake. A cloudless scene keeps the hash it
    /// had before the field was folded in, so an old cloudless state still reuses.
    #[test]
    fn a_cloud_edit_changes_the_sky_hash_and_rebakes_every_cell() {
        let (clear, layout) = (scene(0.0), layout(&[A, B]));
        let previous = state_of(&clear, &layout);
        let same = plan_incremental(&clear, &layout, &settings(), Some(&previous), &inc(), "cpu").report;
        assert_eq!(
            (same.full, same.cells_reused, same.cells_baked),
            (None, same.cells, 0),
            "a cloudless rebake must reuse every cell"
        );

        let mut cloudy = clear.clone();
        cloudy.clouds = Some(overcast(0.9));
        let edited = plan_incremental(&cloudy, &layout, &settings(), Some(&previous), &inc(), "cpu").report;
        let clear_sky = global_hashes(&clear, &layout, &settings(), &inc(), "cpu").sky;
        let cloudy_sky = global_hashes(&cloudy, &layout, &settings(), &inc(), "cpu").sky;
        assert!(
            clear_sky != cloudy_sky && edited.full == Some("sky") && edited.cells_reused == 0,
            "sky differs: {}; plan full={:?} reused={} baked={} of {}",
            clear_sky != cloudy_sky,
            edited.full,
            edited.cells_reused,
            edited.cells_baked,
            edited.cells
        );
        assert_eq!(edited.cells_baked, edited.cells);

        let mut thinner = cloudy.clone();
        thinner.clouds = Some(overcast(0.2));
        let thin_sky = global_hashes(&thinner, &layout, &settings(), &inc(), "cpu").sky;
        assert_ne!(
            thin_sky, cloudy_sky,
            "editing the cloud field must change the sky hash"
        );

        thinner.clouds = None;
        assert_eq!(
            global_hashes(&thinner, &layout, &settings(), &inc(), "cpu").sky,
            clear_sky,
            "clearing the field must restore the cloudless sky hash"
        );
    }

    #[test]
    fn terrain_samples_clamps_the_last_column_beyond_f32_precision() {
        // (n - 1) as f32 rounds up to n for n = 16_777_220: the slice used to end one past the row.
        let n = 16_777_220u32;
        let t = PtHeightfield {
            width: n,
            depth: 1,
            origin: [0.0, 0.0],
            spacing: [1.0, 1.0],
            heights: vec![0.0; n as usize],
            material: 0,
        };
        let mut h = Sha256::new();
        terrain_samples(&mut h, &t, [16_772_200.0, 0.0, 0.0], [16_772_219.0, 0.0, 0.0]);
        let _ = h.finish();
    }

    /// An unchanged scene reuses every cell. The whole-scene hash already matches, so no cell
    /// walks the item list. Mesh bytes are still hashed once, to form that scene hash.
    #[test]
    fn an_unchanged_scene_reuses_every_cell_without_scanning_items() {
        let (s, l) = (scene(0.0), layout(&[A, B]));
        let baked = bake(&CpuProbeTracer::with_threads(1), &s, &l, None);
        let bytes_before = incremental_mesh_bytes_hashed();
        let tests_before = incremental_cell_item_tests();
        let plan = plan_incremental(&s, &l, &settings(), Some(&baked.state), &inc(), "cpu");
        let bytes = incremental_mesh_bytes_hashed() - bytes_before;
        let tests = incremental_cell_item_tests() - tests_before;
        let one_pass: u64 = s
            .meshes
            .iter()
            .map(|m| {
                (std::mem::size_of_val(m.positions.as_slice())
                    + std::mem::size_of_val(m.normals.as_slice())
                    + std::mem::size_of_val(m.uvs.as_slice())
                    + std::mem::size_of_val(m.colors.as_slice())
                    + std::mem::size_of_val(m.tangents.as_slice())
                    + std::mem::size_of_val(m.indices.as_slice())) as u64
            })
            .sum();
        assert_eq!(bytes, one_pass, "mesh bytes hashed once");
        assert_eq!(tests, 0, "overlap tests {tests}");
        assert_eq!(plan.report.cells_reused, plan.cells.len());
        assert!(plan.reuse.iter().all(|r| r.is_some()));
        assert_eq!(
            plan.scene_hashes,
            cell_scene_hashes(&s, &plan.cells, inc().margin(&l))
        );
        let mut moved = s.clone();
        moved.instances[1].transform = Mat4::from_translation(Vec3::new(20.0, 0.0, 0.0));
        let dirty = plan_incremental(&moved, &l, &settings(), Some(&baked.state), &inc(), "cpu");
        assert_eq!(dirty.report.cells_reused, 1, "{:?}", dirty.report);
        assert_eq!(dirty.report.baked.len(), 1);
        assert_eq!(dirty.report.baked[0].cell, CellKind::Brick(0));
        assert_eq!(dirty.report.baked[0].reason, "scene");
        assert_eq!(
            dirty.scene_hashes,
            cell_scene_hashes(&moved, &dirty.cells, inc().margin(&l))
        );
    }
}
