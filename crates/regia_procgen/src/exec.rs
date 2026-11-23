// SPDX-License-Identifier: MPL-2.0
//! The evaluator (spec 62 § 5): runs a compiled graph as a pure function of its inputs.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use regia_core::id::Guid;
use regia_core::tasks::TaskPool;
use regia_render::quality::QualityTier;

use crate::data::{Bundle, DataItem, GroundData, Payload, PointSet, ShapeData, Table};
use crate::graph::{CompiledGraph, GraphError};
use crate::nodes::NodeError;
use crate::seeds;

/// Chunk size for cancellation checks inside element loops (spec 62 § 5.1).
pub const CHUNK: usize = 4096;

/// Run limits (spec 62 § 5.2). Exceeding one fails the run with `limit_exceeded` naming it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunLimits {
    /// Points per item.
    pub points_per_item: usize,
    /// Elements across every node output stored so far in a frame (cumulative: an output is
    /// counted when stored and not released when its consumers are done; bypassed nodes count
    /// their pass-through copy).
    pub total_elements: usize,
    /// Estimated bytes across every node output stored so far in a frame (cumulative, as
    /// `total_elements`).
    pub memory_bytes: usize,
    /// Loop iterations (`graph.loop`).
    pub loop_iterations: usize,
    /// Call depth (`graph.call`).
    pub call_depth: usize,
    /// Prefab instances per generator (`spawn.prefab`, spec 63 § 13.2).
    pub prefabs_per_generator: usize,
}

impl RunLimits {
    /// Desktop defaults.
    pub const DESKTOP: RunLimits = RunLimits {
        points_per_item: 4 << 20,
        total_elements: 32 << 20,
        memory_bytes: 1 << 30,
        loop_iterations: 65_536,
        call_depth: 32,
        prefabs_per_generator: 20_000,
    };
    /// Android defaults.
    pub const ANDROID: RunLimits = RunLimits {
        points_per_item: 1 << 20,
        total_elements: 8 << 20,
        memory_bytes: 256 << 20,
        loop_iterations: 65_536,
        call_depth: 32,
        prefabs_per_generator: 5_000,
    };

    /// The defaults for the platform this build runs on.
    pub fn for_platform() -> Self {
        if cfg!(target_os = "android") {
            Self::ANDROID
        } else {
            Self::DESKTOP
        }
    }

    /// These limits lowered by an override, never raised above the platform caps.
    pub fn with_override(self, o: &RunLimitsOverride) -> Self {
        let cap = |v: Option<usize>, max: usize| v.map_or(max, |x| x.min(max));
        Self {
            points_per_item: cap(o.points_per_item, self.points_per_item),
            total_elements: cap(o.total_elements, self.total_elements),
            memory_bytes: cap(o.memory_bytes, self.memory_bytes),
            loop_iterations: cap(o.loop_iterations, self.loop_iterations),
            call_depth: cap(o.call_depth, self.call_depth),
            prefabs_per_generator: cap(o.prefabs_per_generator, self.prefabs_per_generator),
        }
    }
}

impl Default for RunLimits {
    fn default() -> Self {
        Self::for_platform()
    }
}

/// Per-generator limit overrides; each is capped by the platform default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunLimitsOverride {
    /// See [`RunLimits::points_per_item`].
    pub points_per_item: Option<usize>,
    /// See [`RunLimits::total_elements`].
    pub total_elements: Option<usize>,
    /// See [`RunLimits::memory_bytes`].
    pub memory_bytes: Option<usize>,
    /// See [`RunLimits::loop_iterations`].
    pub loop_iterations: Option<usize>,
    /// See [`RunLimits::call_depth`].
    pub call_depth: Option<usize>,
    /// See [`RunLimits::prefabs_per_generator`].
    pub prefabs_per_generator: Option<usize>,
}

/// A cancellation flag shared between the requester and the run (checked between nodes and
/// chunks).
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, untripped token. Clones share the flag: hand one to the run, keep one to cancel it.
    ///
    /// ```
    /// use regia_procgen::CancelToken;
    ///
    /// let token = CancelToken::new();
    /// let for_the_run = token.clone();
    /// assert!(!for_the_run.is_cancelled());
    /// token.cancel();
    /// assert!(for_the_run.is_cancelled());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// The generator itself as the graph sees it (`in.self`).
#[derive(Clone, Debug, PartialEq)]
pub struct SelfInfo {
    /// The generator entity's guid.
    pub guid: Guid,
    /// Its name.
    pub name: String,
    /// The run bounds as a box shape in world space.
    pub bounds: ShapeData,
}

/// A level entity captured for `in.entities`.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapEntity {
    /// Guid.
    pub guid: Guid,
    /// Name.
    pub name: String,
    /// World position.
    pub position: [f32; 3],
    /// World rotation.
    pub rotation: [f32; 4],
    /// World scale.
    pub scale: [f32; 3],
    /// Tags.
    pub tags: Vec<String>,
    /// Its `Spline` in world space (spec 64 § 2), for `in.entities { as: splines }`.
    pub spline: Option<Arc<crate::data::SplineData>>,
    /// Its `Shape` as a world-space region (spec 64 § 2.4), for `in.entities { as: shapes }`.
    pub shape: Option<ShapeData>,
    /// Its `RoadOf` profile path (spec 64 § 7.1), item scope `road_profile` of spline items.
    pub road_profile: Option<String>,
    /// The local (unscaled) bounds `(min, max)` of its `MeshRenderer` mesh, for the `bounds_*` of its
    /// `in.entities` point (ADR-3027); `None` without a mesh (the point keeps the unit box).
    pub mesh_bounds: Option<([f32; 3], [f32; 3])>,
}

/// World data captured before a run (spec 62 § 5.3). The evaluator reads the world only through
/// this.
pub trait WorldSnapshot: Send + Sync {
    /// The generator (`in.self`).
    fn self_info(&self) -> Option<&SelfInfo>;
    /// The terrain over the run bounds (plus margin), if the level has terrain.
    fn ground(&self) -> Option<Arc<GroundData>>;
    /// Level entities (not generated ones), in guid order.
    fn entities(&self) -> &[SnapEntity];
    /// A data table read for `in.table`, by asset path.
    fn table(&self, path: &str) -> Option<&Table>;
    /// Another generator's exported table or points (`in.generator_output`), by the generator's guid
    /// or name and the export name.
    fn generator_export(&self, _generator: &str, _name: &str) -> Option<DataItem> {
        None
    }
    /// A mask texture's CPU pixels for `in.mask` (RGBA, linear [0, 1]), by asset path.
    fn mask_image(&self, _path: &str) -> Option<&MaskImage> {
        None
    }
    /// A mesh's CPU geometry for `in.mesh` / `sample.mesh_surface`, by asset path.
    fn mesh(&self, _path: &str) -> Option<Arc<crate::data::MeshData>> {
        None
    }
    /// The ray hits the host cast for the `in.rays` node `node` (of the generator's own graph).
    fn rays(&self, _node: u32) -> &[RayHitRecord] {
        &[]
    }
    /// The collider bounds the host captured for the `in.overlap` node `node`.
    fn colliders(&self, _node: u32) -> &[ColliderBox] {
        &[]
    }
    /// The generator's painted points (`PaintedPoints`, spec 67 § 5.2), read by `in.self`.
    fn painted(&self) -> Option<&PointSet> {
        None
    }
    /// A road profile (`.road.ron`, spec 64 § 7.1) the road nodes name, by asset path.
    fn road_profile(&self, _path: &str) -> Option<&crate::road::RoadProfile> {
        None
    }
    /// Biome areas, exclusions and water for the `biome.*` nodes (spec 65).
    fn biome(&self) -> Option<Arc<crate::biome::BiomeWorld>> {
        None
    }
    /// What `in.self` adds after the bounds: the entity's `Shape` and `Spline` items, a closed
    /// spline also as a `Polygon` item, all tagged `self` (spec 63 § 3).
    fn self_items(&self) -> Vec<DataItem> {
        Vec::new()
    }
    /// A hash of the world data cacheable nodes read ([`crate::nodes::NodeKind::reads_world`]): the
    /// ground heights, the meshes and the road profiles. It joins those nodes' cache keys, so an edited terrain or mesh
    /// never answers from the cache. `None` (the default: unknown) makes those nodes skip the cache.
    fn content_hash(&self) -> Option<[u8; 32]> {
        None
    }
}

/// One captured collider as a world AABB (`in.overlap`).
#[derive(Clone, Debug, PartialEq)]
pub struct ColliderBox {
    /// World AABB minimum.
    pub min: [f32; 3],
    /// World AABB maximum.
    pub max: [f32; 3],
    /// The collider entity's guid (empty when it has none).
    pub entity: String,
    /// The collider's shape as world-space convex parts for the exact test of `in.overlap`
    /// (ADR-3027); empty = only the AABB is tested.
    pub parts: Vec<ConvexPart>,
}

/// A convex piece of a captured collider in world space: the convex hull of `points` and `disks`,
/// grown by `radius` (a box is 8 points, a sphere 1 point and a radius, a capsule 2, a cylinder 2
/// disks, a cone a point and a disk, a mesh triangle 3 points; ADR-3027).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConvexPart {
    /// Hull points.
    pub points: Vec<[f32; 3]>,
    /// Disks `(center, unit normal, radius)` in the hull.
    pub disks: Vec<([f32; 3], [f32; 3], f32)>,
    /// Radius the hull is grown by (`0` = none).
    pub radius: f32,
    /// World AABB minimum of the grown hull.
    pub min: [f32; 3],
    /// World AABB maximum of the grown hull.
    pub max: [f32; 3],
}

/// One captured physics ray hit (`in.rays`).
#[derive(Clone, Debug, PartialEq)]
pub struct RayHitRecord {
    /// World hit point.
    pub point: [f32; 3],
    /// World surface normal.
    pub normal: [f32; 3],
    /// The hit entity's guid (empty when it has none).
    pub entity: String,
    /// The hit collider's physics material name (`PhysicsMaterialRef`; empty when it has none,
    /// ADR-3027).
    pub phys_material: String,
}

/// CPU pixels of a mask texture (`in.mask`): row-major RGBA, row 0 at the rectangle's `min_z`.
#[derive(Clone, Debug, PartialEq)]
pub struct MaskImage {
    /// Columns.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// `width * height` pixels.
    pub rgba: Vec<[f32; 4]>,
}

impl MaskImage {
    /// The most pixels a decoded mask may have (4096 × 4096): a mask holds 16 bytes a pixel, so a
    /// small file that declares a huge size is refused before anything is allocated for it.
    pub const MAX_PIXELS: u64 = 4096 * 4096;

    /// Decodes a PNG (gray, gray with alpha, RGB or RGBA; 8 or 16 bits, or indexed). Values are the
    /// stored numbers over [0, 1] at the file's own precision (a 16-bit sample is `v / 65535`), with
    /// no sRGB decoding: a mask holds data, not color. A file with no alpha reads as alpha 1, gray as
    /// equal r, g and b.
    ///
    /// # Errors
    ///
    /// A message when the bytes do not decode, or when the image has more than [`Self::MAX_PIXELS`].
    pub fn from_png(bytes: &[u8]) -> Result<Self, String> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        // Palette to RGB, low bit depths to 8, tRNS to alpha; 16-bit samples stay 16-bit.
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().map_err(|e| format!("png: {e}"))?;
        let (width, height) = (reader.info().width, reader.info().height);
        if u64::from(width) * u64::from(height) > Self::MAX_PIXELS {
            return Err(format!(
                "png: {width} x {height} is more than the {} pixels a mask may have",
                Self::MAX_PIXELS
            ));
        }
        let size = reader.output_buffer_size().ok_or("png: too large")?;
        let mut buf = vec![0; size];
        let info = reader.next_frame(&mut buf).map_err(|e| format!("png: {e}"))?;
        let channels = info.color_type.samples();
        let wide = info.bit_depth == png::BitDepth::Sixteen;
        let samples: Vec<f32> = if wide {
            buf[..info.buffer_size()]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| f32::from(u16::from_be_bytes(*b)) / 65535.0)
                .collect()
        } else {
            buf[..info.buffer_size()]
                .iter()
                .map(|b| f32::from(*b) / 255.0)
                .collect()
        };
        let rgba = samples
            .chunks_exact(channels)
            .map(|c| match *c {
                [g] => [g, g, g, 1.0],
                [g, a] => [g, g, g, a],
                [r, g, b] => [r, g, b, 1.0],
                [r, g, b, a, ..] => [r, g, b, a],
                _ => [0.0; 4],
            })
            .collect();
        Ok(Self {
            width: info.width,
            height: info.height,
            rgba,
        })
    }
}

/// The cached hash of a snapshot's ground and meshes, and the buffers it was computed from. The
/// memo holds those `Arc`s, so their allocations cannot be freed and reused by another buffer that
/// would then match a stale hash (corr-procgen), and an `Arc::get_mut` edit cannot happen in place.
#[derive(Clone, Debug)]
struct CachedContentHash {
    ground: Option<Arc<GroundData>>,
    meshes: Vec<(String, Arc<crate::data::MeshData>)>,
    hash: [u8; 32],
}

/// Shared memo of [`WorldSnapshot::content_hash`]. Clones of a [`StaticSnapshot`] share one memo.
#[derive(Clone, Debug, Default)]
pub struct ContentHashMemo {
    cached: Arc<Mutex<Option<CachedContentHash>>>,
}

/// A plain-data [`WorldSnapshot`] (what the ECS host builds, and what tests use).
#[derive(Clone, Debug, Default)]
pub struct StaticSnapshot {
    /// See [`WorldSnapshot::self_info`].
    pub self_info: Option<SelfInfo>,
    /// See [`WorldSnapshot::ground`].
    pub ground: Option<Arc<GroundData>>,
    /// See [`WorldSnapshot::entities`].
    pub entities: Vec<SnapEntity>,
    /// See [`WorldSnapshot::table`].
    pub tables: BTreeMap<String, Table>,
    /// See [`WorldSnapshot::generator_export`]: `(generator, export name)` → item.
    pub exports: BTreeMap<(String, String), DataItem>,
    /// See [`WorldSnapshot::mask_image`].
    pub masks: BTreeMap<String, MaskImage>,
    /// See [`WorldSnapshot::mesh`].
    pub meshes: BTreeMap<String, Arc<crate::data::MeshData>>,
    /// See [`WorldSnapshot::rays`]: node id → hits.
    pub rays: BTreeMap<u32, Vec<RayHitRecord>>,
    /// See [`WorldSnapshot::colliders`]: node id → collider boxes.
    pub colliders: BTreeMap<u32, Vec<ColliderBox>>,
    /// See [`WorldSnapshot::painted`].
    pub painted: Option<PointSet>,
    /// See [`WorldSnapshot::road_profile`]: asset path → profile.
    pub road_profiles: BTreeMap<String, crate::road::RoadProfile>,
    /// See [`WorldSnapshot::biome`].
    pub biome: Option<Arc<crate::biome::BiomeWorld>>,
    /// See [`WorldSnapshot::self_items`].
    pub self_items: Vec<DataItem>,
    /// The memoized ground and mesh part of [`WorldSnapshot::content_hash`]. Clones share it. It holds
    /// the ground and mesh `Arc`s it hashed (keeping the last ones alive) and is reused while this
    /// snapshot holds the very same `Arc`s; self items and road profiles are hashed on every call.
    pub content_hash_memo: ContentHashMemo,
}

impl WorldSnapshot for StaticSnapshot {
    fn self_info(&self) -> Option<&SelfInfo> {
        self.self_info.as_ref()
    }
    fn ground(&self) -> Option<Arc<GroundData>> {
        self.ground.clone()
    }
    fn entities(&self) -> &[SnapEntity] {
        &self.entities
    }
    fn table(&self, path: &str) -> Option<&Table> {
        self.tables.get(path)
    }
    fn generator_export(&self, generator: &str, name: &str) -> Option<DataItem> {
        self.exports
            .get(&(generator.to_string(), name.to_string()))
            .cloned()
    }
    fn mask_image(&self, path: &str) -> Option<&MaskImage> {
        self.masks.get(path)
    }
    fn mesh(&self, path: &str) -> Option<Arc<crate::data::MeshData>> {
        self.meshes.get(path).cloned()
    }
    fn rays(&self, node: u32) -> &[RayHitRecord] {
        self.rays.get(&node).map_or(&[], Vec::as_slice)
    }
    fn colliders(&self, node: u32) -> &[ColliderBox] {
        self.colliders.get(&node).map_or(&[], Vec::as_slice)
    }
    fn painted(&self) -> Option<&PointSet> {
        self.painted.as_ref()
    }
    fn road_profile(&self, path: &str) -> Option<&crate::road::RoadProfile> {
        self.road_profiles.get(path)
    }
    fn biome(&self) -> Option<Arc<crate::biome::BiomeWorld>> {
        self.biome.clone()
    }
    fn self_items(&self) -> Vec<DataItem> {
        self.self_items.clone()
    }
    fn content_hash(&self) -> Option<[u8; 32]> {
        let mut h = blake3::Hasher::new();
        if !self.self_items.is_empty() {
            h.update(b"self items");
            h.update(&Bundle(self.self_items.clone()).hash());
        }
        h.update(&self.heavy_hash());
        // Road profiles (`road.exclusion` reads them): the Debug text renders every field, and an
        // empty map adds nothing.
        for (path, p) in &self.road_profiles {
            h.update(b"road profile");
            let text = format!("{p:?}");
            for part in [path.as_str(), text.as_str()] {
                h.update(&(part.len() as u64).to_le_bytes());
                h.update(part.as_bytes());
            }
        }
        Some(*h.finalize().as_bytes())
    }
}

impl StaticSnapshot {
    /// The ground and mesh part of the content hash, memoized while this snapshot holds the same
    /// `Arc`s as the memo. O(mesh count) when reused, not O(heights).
    fn heavy_hash(&self) -> [u8; 32] {
        let mut memo = self.content_hash_memo.cached.lock();
        if let Some(cached) = memo.as_ref() {
            let same_ground = match (&cached.ground, &self.ground) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            let same_meshes = cached.meshes.len() == self.meshes.len()
                && cached
                    .meshes
                    .iter()
                    .zip(&self.meshes)
                    .all(|((pa, a), (pb, b))| pa == pb && Arc::ptr_eq(a, b));
            if same_ground && same_meshes {
                return cached.hash;
            }
        }
        let hash = self.hash_ground_and_meshes();
        *memo = Some(CachedContentHash {
            ground: self.ground.clone(),
            meshes: self
                .meshes
                .iter()
                .map(|(path, mesh)| (path.clone(), Arc::clone(mesh)))
                .collect(),
            hash,
        });
        hash
    }

    /// The uncached ground and mesh hash. Walks every height sample.
    fn hash_ground_and_meshes(&self) -> [u8; 32] {
        // Buffered (ADR-7238): the walk feeds four bytes at a time.
        let mut h = HashSink::new();
        let f32s = |h: &mut HashSink, v: &[f32]| {
            for x in v {
                h.update(&x.to_bits().to_le_bytes());
            }
        };
        match &self.ground {
            Some(g) => {
                h.update(b"ground");
                f32s(&mut h, &g.rect);
                f32s(&mut h, &[g.world_width]);
                for v in g.map_size.iter().chain(&g.origin).chain(&g.size) {
                    h.update(&v.to_le_bytes());
                }
                h.update(&(g.heights.len() as u64).to_le_bytes());
                GROUND_HASH_SAMPLES.with(|c| c.set(c.get() + g.heights.len() as u64));
                f32s(&mut h, &g.heights);
            }
            None => {
                h.update(b"no ground");
            }
        }
        for (path, m) in &self.meshes {
            h.update(b"mesh");
            h.update(&(path.len() as u64).to_le_bytes());
            h.update(path.as_bytes());
            h.update(&(m.positions.len() as u64).to_le_bytes());
            m.positions.iter().for_each(|p| f32s(&mut h, p));
            h.update(&(m.normals.len() as u64).to_le_bytes());
            m.normals.iter().for_each(|p| f32s(&mut h, p));
            h.update(&(m.indices.len() as u64).to_le_bytes());
            for i in &m.indices {
                h.update(&i.to_le_bytes());
            }
        }
        *h.finalize().as_bytes()
    }
}

/// Everything a run reads besides the compiled graph.
#[derive(Clone)]
pub struct RunInputs {
    /// Captured world data.
    pub world: Arc<dyn WorldSnapshot>,
    /// The generator seed (spec 62 § 6.2).
    pub gen_seed: u32,
    /// Tier index (0 untiled / base tier).
    pub tier_index: u32,
    /// Cell `(i, j)` for tiled runs; `None` untiled.
    pub cell: Option<(i32, i32)>,
    /// Quality tier `flow.quality` reads.
    pub quality: QualityTier,
    /// Limits.
    pub limits: RunLimits,
    /// Keep every node output for `procgen.inspect`.
    pub keep_debug: bool,
    /// Texts of the graphs `graph.call` / `graph.loop` may run, by path (captured by the host,
    /// [`crate::graph::collect_subgraphs`]).
    pub graphs: Arc<BTreeMap<String, String>>,
    /// Whether this run may produce terrain edits (spec 64 § 7.3; the host decides).
    pub terrain_edits: TerrainEditGate,
    /// Tiled runs (spec 66 § 2): the XZ rectangle `[min_x, min_z, max_x, max_z]` the cell runs
    /// over (cell plus halo); neighborhood nodes count `seam_mismatch_risk` against its edge.
    pub run_rect: Option<[f32; 4]>,
    /// Tiled runs: the owned cell (spec 66 § 2), which mesh and terrain-edit nodes cut against.
    pub tile: Option<TileFrame>,
}

/// The cell a tiled run owns (spec 66 § 2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileFrame {
    /// The tier's cell size, meters.
    pub cell_m: f64,
    /// The cell's half-open rectangle `[min_x, min_z, max_x, max_z]`.
    pub rect: [f64; 4],
}

impl TileFrame {
    /// Whether `(x, z)` is in the cell.
    pub fn owns(&self, x: f64, z: f64) -> bool {
        crate::tiles::in_half_open(self.rect, x, z)
    }
}

/// Whether a run may produce terrain edits (spec 64 § 7.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerrainEditGate {
    /// A `Baked` generator with `allow_terrain_edits` in a project with `[procgen] terrain_edits`.
    Allowed,
    /// The generator is not `Baked` (`flatten_runtime_forbidden`).
    NotBaked,
    /// The generator or the project does not allow it (`terrain_edits_not_allowed`). The default.
    #[default]
    NotAllowed,
}

/// One terrain edit a run produced (spec 64 § 7.3): a sample rectangle of the full terrain grid
/// `[col0, row0, col1, row1)` with its new and original heights, row-major.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerrainEditRecord {
    /// `[col0, row0, col1, row1)` in full-terrain samples.
    pub rect: [u32; 4],
    /// New heights, meters.
    pub samples: Vec<f32>,
    /// Heights before the edit (the captured terrain), meters.
    pub original: Vec<f32>,
}

impl RunInputs {
    /// Inputs over a snapshot with a seed and the platform defaults (High tier).
    pub fn new(world: Arc<dyn WorldSnapshot>, gen_seed: u32) -> Self {
        Self {
            world,
            gen_seed,
            tier_index: 0,
            cell: None,
            quality: QualityTier::High,
            limits: RunLimits::for_platform(),
            keep_debug: false,
            graphs: Arc::new(BTreeMap::new()),
            terrain_edits: TerrainEditGate::NotAllowed,
            run_rect: None,
            tile: None,
        }
    }

    /// The same inputs with subgraph texts.
    pub fn with_graphs(mut self, graphs: BTreeMap<String, String>) -> Self {
        self.graphs = Arc::new(graphs);
        self
    }
}

/// Where a (sub)graph evaluation sits: call depth, the caller's bundles on its graph inputs, and the
/// enclosing loop indices (innermost last).
#[derive(Clone, Debug, Default)]
pub struct Frame {
    /// `graph.call` nesting depth (0 = the generator's graph).
    pub depth: usize,
    /// Bundles on the graph's declared input pins (`graph.input`).
    pub graph_inputs: BTreeMap<String, Bundle>,
    /// Enclosing `graph.loop` iteration indices.
    pub loop_index: Vec<i64>,
}

/// How a spawned instance set collides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Collision {
    /// No collision.
    #[default]
    None,
    /// A box per instance.
    Simple,
    /// The mesh's cooked collision mesh.
    Mesh,
}

impl Collision {
    /// The settings name.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Simple => "Simple",
            Self::Mesh => "Mesh",
        }
    }
}

/// One instance of an instance set.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Instance {
    /// World translation.
    pub translation: [f32; 3],
    /// World rotation `[x, y, z, w]`.
    pub rotation: [f32; 4],
    /// Scale.
    pub scale: [f32; 3],
    /// Per-instance tint.
    pub color: [f32; 4],
    /// Up to four custom floats.
    pub custom: [f32; 4],
}

/// Instances of one mesh with one material, shadow and collision setting (spec 62 § 7.3).
#[derive(Clone, Debug, PartialEq)]
pub struct InstanceSet {
    /// Mesh asset.
    pub mesh: String,
    /// Material override.
    pub material: Option<String>,
    /// Casts shadows.
    pub cast_shadows: bool,
    /// Collision.
    pub collision: Collision,
    /// Draw distance, meters.
    pub draw_distance_m: f32,
    /// Fade band, meters.
    pub fade_m: f32,
    /// Stored with the set, included in its hash and written to `.pgout`. Drawing does not read it.
    pub cull_below_px: f32,
    /// Instances in point order.
    pub instances: Vec<Instance>,
    /// The data layer the set joins (`spawn.mesh` `target_layer`, spec 63 § 13.1): its instances
    /// draw, and its collider is active, only while that layer is `Live`. `None` follows the
    /// generator alone. Part of the grouping key; hashed only when set.
    pub layer: Option<String>,
}

/// How many height samples [`StaticSnapshot::content_hash`] has walked on the calling thread. A
/// second hash of an unchanged snapshot does not add them again. Per thread, so a test reading
/// exact deltas is not disturbed by another test thread hashing its own ground.
pub fn ground_hash_samples() -> u64 {
    GROUND_HASH_SAMPLES.with(std::cell::Cell::get)
}

thread_local! {
    static GROUND_HASH_SAMPLES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many times [`InstanceSet::key`] has cloned a mesh path in this process. [`RunOutput::set_for`]
/// on one prototype does not increment it per instance.
pub fn mesh_key_clones() -> u64 {
    MESH_KEY_CLONES.load(Ordering::Relaxed)
}

static MESH_KEY_CLONES: AtomicU64 = AtomicU64::new(0);

impl InstanceSet {
    /// The grouping key `(mesh, material, shadows, collision)`; sets also differ by [`Self::layer`].
    pub fn key(&self) -> (String, Option<String>, bool, Collision) {
        MESH_KEY_CLONES.fetch_add(1, Ordering::Relaxed);
        (
            self.mesh.clone(),
            self.material.clone(),
            self.cast_shadows,
            self.collision,
        )
    }
}

/// One prefab instance a run asks the host to spawn.
#[derive(Clone, Debug, PartialEq)]
pub struct PrefabSpawn {
    /// Prefab asset.
    pub prefab: String,
    /// Entity name.
    pub name: String,
    /// World translation.
    pub translation: [f32; 3],
    /// World rotation.
    pub rotation: [f32; 4],
    /// Scale.
    pub scale: [f32; 3],
    /// Tags added to the spawned entity.
    pub tags: Vec<String>,
    /// Reflected field overrides from the point's attributes (`spawn.prefab` `overrides`, spec 63
    /// § 13.2), in setting order; empty for a plain instance. The host applies them when the
    /// instance exists ([`crate::prefab_overrides`]).
    pub overrides: Vec<PrefabFieldOverride>,
}

/// One reflected field override of a spawned prefab instance (spec 63 § 13.2, ADR-2724).
#[derive(Clone, Debug, PartialEq)]
pub struct PrefabFieldOverride {
    /// The component: its full type path (`my_game::Glow`) or short name (`Glow`).
    pub component: String,
    /// The field path inside the component (`intensity`, `light.range`).
    pub field: String,
    /// The point's attribute value, converted to the field's type when applied.
    pub value: crate::data::AttrValue,
}

impl PrefabFieldOverride {
    fn hash_into(&self, h: &mut HashSink) {
        use crate::data::AttrValue as V;
        for part in [self.component.as_str(), self.field.as_str()] {
            h.update(&(part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
        let nums = |h: &mut HashSink, tag: u8, v: &[f64]| {
            h.update(&[tag]);
            for x in v {
                h.update(&x.to_bits().to_le_bytes());
            }
        };
        match &self.value {
            V::Bool(b) => nums(h, 0, &[f64::from(u8::from(*b))]),
            V::Int(i) => {
                h.update(&[1]);
                h.update(&i.to_le_bytes());
            }
            V::Float(x) => nums(h, 2, &[*x]),
            V::Vec2(v) => nums(h, 3, v),
            V::Vec3(v) => nums(h, 4, v),
            V::Vec4(v) => nums(h, 5, v),
            V::Quat(v) => nums(h, 6, v),
            V::Name(t) | V::Asset(t) => {
                h.update(&[7]);
                h.update(&(t.len() as u64).to_le_bytes());
                h.update(t.as_bytes());
            }
            other => {
                let text = format!("{other:?}");
                h.update(&[8]);
                h.update(&(text.len() as u64).to_le_bytes());
                h.update(text.as_bytes());
            }
        }
    }
}

/// How many times a [`RunOutput`] has been cloned in this process. Publishing a finished cell
/// shares one `Arc` and does not increment it.
pub fn run_output_clones() -> u64 {
    RUN_OUTPUT_CLONES.load(Ordering::Relaxed)
}

static RUN_OUTPUT_CLONES: AtomicU64 = AtomicU64::new(0);

static EVICT_INSPECTIONS: AtomicU64 = AtomicU64::new(0);

/// Everything a run produced (spec 62 § 7.3), before the host applies it.
#[derive(Debug, Default, PartialEq)]
pub struct RunOutput {
    /// Instance sets in first-spawn order of their key.
    pub instance_sets: Vec<InstanceSet>,
    /// Prefab instances in point order.
    pub prefabs: Vec<PrefabSpawn>,
    /// Exported tables (`out.table`).
    pub tables: BTreeMap<String, Table>,
    /// Exported points (`out.points`).
    pub points: BTreeMap<String, PointSet>,
    /// Exported masks (`out.mask`).
    pub masks: BTreeMap<String, crate::data::MaskData>,
    /// Graph output pins (`graph.output`).
    pub graph_outputs: BTreeMap<String, Bundle>,
    /// Meshes the graph built (spline-deformed pieces, road ribbons; spec 64 § 6), in world space.
    pub meshes: Vec<GeneratedMesh>,
    /// Terrain edits of a `Baked` run (spec 64 § 7.3).
    pub terrain_edits: Vec<TerrainEditRecord>,
    /// Road decal segments for hosts that draw roads in the terrain shader (`road.decals`, spec 64
    /// § 7.2), in road order.
    pub road_decals: Vec<regia_terrain::RoadDecal>,
    /// Plant exports recorded by `plant.export` (spec 68 § 6.3), in node order. The host writes them
    /// (`regia_asset_pipeline::plant_export::write_job`); they are neither hashed nor baked.
    pub exports: Vec<crate::plant::ExportJob>,
}

/// A mesh a graph built (spec 62 § 7.3, spec 64 § 6): world-space CPU streams with a material.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GeneratedMesh {
    /// Material asset (empty = the default material).
    pub material: String,
    /// Vertex positions.
    pub positions: Vec<[f32; 3]>,
    /// Unit vertex normals (same length).
    pub normals: Vec<[f32; 3]>,
    /// Texture coordinates (same length).
    pub uvs: Vec<[f32; 2]>,
    /// Triangle-list indices.
    pub indices: Vec<u32>,
    /// Linear vertex colors (same length as `positions`), or empty for white.
    pub colors: Vec<[f32; 4]>,
    /// Builds a static mesh collider (`spawn.spline_mesh` `collision: mesh`).
    pub collision: bool,
}

/// Most vertices in one [`GeneratedMesh`] (spec 64 § 6).
pub const GENERATED_MESH_MAX_VERTICES: usize = 65_536;

impl GeneratedMesh {
    /// Appends `other` into the last mesh of `list` with the same material when it fits under
    /// [`GENERATED_MESH_MAX_VERTICES`], else starts a new mesh.
    pub fn merge_into(list: &mut Vec<GeneratedMesh>, other: GeneratedMesh) {
        if let Some(last) = list
            .iter_mut()
            .rev()
            .find(|m| m.material == other.material && m.collision == other.collision)
            .filter(|m| m.positions.len() + other.positions.len() <= GENERATED_MESH_MAX_VERTICES)
        {
            let base = last.positions.len() as u32;
            let colored = !last.colors.is_empty() || !other.colors.is_empty();
            if colored {
                last.colors.resize(base as usize, [1.0; 4]);
            }
            if colored {
                let mut colors = other.colors;
                colors.resize(other.positions.len(), [1.0; 4]);
                last.colors.extend(colors);
            }
            last.positions.extend(other.positions);
            last.normals.extend(other.normals);
            last.uvs.extend(other.uvs);
            last.indices.extend(other.indices.iter().map(|i| i + base));
        } else {
            list.push(other);
        }
    }

    fn hash_into(&self, h: &mut HashSink) {
        h.update(self.material.as_bytes());
        h.update(&(self.positions.len() as u64).to_le_bytes());
        for v in self.positions.iter().chain(&self.normals) {
            for x in v {
                h.update(&x.to_bits().to_le_bytes());
            }
        }
        for v in &self.uvs {
            for x in v {
                h.update(&x.to_bits().to_le_bytes());
            }
        }
        for i in &self.indices {
            h.update(&i.to_le_bytes());
        }
        // Absent colors and collision hash as nothing, so meshes without them keep their hash.
        if !self.colors.is_empty() {
            h.update(b"colors");
            for v in &self.colors {
                for x in v {
                    h.update(&x.to_bits().to_le_bytes());
                }
            }
        }
        if self.collision {
            h.update(b"collision");
        }
    }

    /// Keeps the triangles `keep` accepts (by triangle index) and the vertices they use, in vertex
    /// order. Normals stay as computed on the whole mesh, so a mesh cut at a cell edge has no
    /// shading crease there (spec 66 § 2).
    pub fn keep_triangles(&mut self, keep: impl Fn(usize) -> bool) {
        let mut used = vec![false; self.positions.len()];
        let mut indices = Vec::with_capacity(self.indices.len());
        for (k, t) in self.indices.as_chunks::<3>().0.iter().enumerate() {
            if keep(k) {
                for i in t {
                    used[*i as usize] = true;
                }
                indices.extend_from_slice(t);
            }
        }
        let mut remap = vec![0u32; used.len()];
        let mut next = 0u32;
        for (r, u) in remap.iter_mut().zip(&used) {
            *r = next;
            next += u32::from(*u);
        }
        let kept = |i: usize| used[i];
        self.positions = self
            .positions
            .iter()
            .enumerate()
            .filter(|(i, _)| kept(*i))
            .map(|(_, v)| *v)
            .collect();
        self.normals = self
            .normals
            .iter()
            .enumerate()
            .filter(|(i, _)| kept(*i))
            .map(|(_, v)| *v)
            .collect();
        self.uvs = self
            .uvs
            .iter()
            .enumerate()
            .filter(|(i, _)| kept(*i))
            .map(|(_, v)| *v)
            .collect();
        self.colors = self
            .colors
            .iter()
            .enumerate()
            .filter(|(i, _)| kept(*i))
            .map(|(_, v)| *v)
            .collect();
        self.indices = indices.iter().map(|i| remap[*i as usize]).collect();
    }
}

impl Clone for RunOutput {
    fn clone(&self) -> Self {
        RUN_OUTPUT_CLONES.fetch_add(1, Ordering::Relaxed);
        Self {
            instance_sets: self.instance_sets.clone(),
            prefabs: self.prefabs.clone(),
            tables: self.tables.clone(),
            points: self.points.clone(),
            masks: self.masks.clone(),
            graph_outputs: self.graph_outputs.clone(),
            meshes: self.meshes.clone(),
            terrain_edits: self.terrain_edits.clone(),
            road_decals: self.road_decals.clone(),
            exports: self.exports.clone(),
        }
    }
}

impl RunOutput {
    /// Number of instances over all sets.
    pub fn instance_count(&self) -> usize {
        self.instance_sets.iter().map(|s| s.instances.len()).sum()
    }

    /// The instance set for a key, created on first use.
    pub fn set_for(&mut self, proto: &InstanceSet) -> &mut InstanceSet {
        let pos = self.set_index_for(proto);
        &mut self.instance_sets[pos]
    }

    /// The index in `instance_sets` of the set for a key, created on first use. Sets are never
    /// removed during a run, so a node may keep the index instead of looking the key up per point.
    /// The lookup compares mesh, material, shadows and collision in place.
    pub fn set_index_for(&mut self, proto: &InstanceSet) -> usize {
        match self.instance_sets.iter().position(|s| {
            s.mesh == proto.mesh
                && s.material == proto.material
                && s.cast_shadows == proto.cast_shadows
                && s.collision == proto.collision
                && s.layer == proto.layer
        }) {
            Some(p) => p,
            None => {
                self.instance_sets.push(InstanceSet {
                    instances: Vec::new(),
                    ..proto.clone()
                });
                self.instance_sets.len() - 1
            }
        }
    }

    /// The output hash: blake3 over the canonical encoding of the published parts (instance sets,
    /// prefabs, tables, points).
    pub fn hash(&self) -> [u8; 32] {
        let _span = tracing::info_span!(target: "regia_procgen", "procgen.output_hash").entered();
        let mut h = HashSink::new();
        h.update(b"regia.pgout.v1");
        let f = |h: &mut HashSink, v: &[f32]| {
            for x in v {
                h.update(&x.to_bits().to_le_bytes());
            }
        };
        for s in &self.instance_sets {
            h.update(s.mesh.as_bytes());
            h.update(s.material.as_deref().unwrap_or("").as_bytes());
            h.update(&[u8::from(s.cast_shadows), s.collision as u8]);
            f(&mut h, &[s.draw_distance_m, s.fade_m, s.cull_below_px]);
            // Absent layers hash as nothing, so outputs without them keep their hash.
            if let Some(layer) = &s.layer {
                h.update(b"layer");
                h.update(&(layer.len() as u64).to_le_bytes());
                h.update(layer.as_bytes());
            }
            h.update(&(s.instances.len() as u64).to_le_bytes());
            for i in &s.instances {
                f(&mut h, &i.translation);
                f(&mut h, &i.rotation);
                f(&mut h, &i.scale);
                f(&mut h, &i.color);
                f(&mut h, &i.custom);
            }
        }
        for p in &self.prefabs {
            h.update(p.prefab.as_bytes());
            h.update(p.name.as_bytes());
            f(&mut h, &p.translation);
            f(&mut h, &p.rotation);
            f(&mut h, &p.scale);
            for t in &p.tags {
                h.update(t.as_bytes());
            }
            // Absent overrides hash as nothing, so outputs without them keep their hash.
            if !p.overrides.is_empty() {
                h.update(b"overrides");
                h.update(&(p.overrides.len() as u64).to_le_bytes());
                for o in &p.overrides {
                    o.hash_into(&mut h);
                }
            }
        }
        for (k, t) in &self.tables {
            h.update(k.as_bytes());
            h.update(&Bundle::one(DataItem::new(Payload::Table(t.clone()))).hash());
        }
        for (k, p) in &self.points {
            h.update(k.as_bytes());
            h.update(&Bundle::one(DataItem::points(p.clone())).hash());
        }
        for (k, m) in &self.masks {
            h.update(k.as_bytes());
            h.update(&Bundle::one(DataItem::new(Payload::Mask(Arc::new(m.clone())))).hash());
        }
        if !self.terrain_edits.is_empty() {
            h.update(b"terrain_edits");
            for t in &self.terrain_edits {
                for v in t.rect {
                    h.update(&v.to_le_bytes());
                }
                f(&mut h, &t.samples);
                f(&mut h, &t.original);
            }
        }
        if !self.meshes.is_empty() {
            h.update(b"meshes");
            for m in &self.meshes {
                m.hash_into(&mut h);
            }
        }
        if !self.road_decals.is_empty() {
            h.update(b"road_decals");
            for d in &self.road_decals {
                f(&mut h, &d.a);
                f(&mut h, &d.b);
                f(&mut h, &[d.half_width]);
                f(&mut h, &d.color);
            }
        }
        // Graph output pins feed `graph.call` callers; they are not generator output (spec 62 § 7.3)
        // and are neither hashed nor baked.
        *h.finalize().as_bytes()
    }
}

/// A blake3 hasher fed through a byte buffer. The digest of a byte stream does not depend on how it
/// is split into `update` calls, so the output hash keeps its value; a large update takes blake3's
/// wide path instead of millions of 4- and 8-byte calls (ADR-7238).
struct HashSink {
    h: blake3::Hasher,
    buf: Vec<u8>,
}

impl HashSink {
    /// Buffered bytes before they go to the hasher.
    const FLUSH: usize = 64 << 10;

    fn new() -> Self {
        Self {
            h: blake3::Hasher::new(),
            buf: Vec::with_capacity(Self::FLUSH),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        if self.buf.len() + bytes.len() > Self::FLUSH {
            self.flush();
            if bytes.len() >= Self::FLUSH {
                self.h.update(bytes);
                return;
            }
        }
        self.buf.extend_from_slice(bytes);
    }

    fn flush(&mut self) {
        if !self.buf.is_empty() {
            self.h.update(&self.buf);
            self.buf.clear();
        }
    }

    fn finalize(mut self) -> blake3::Hash {
        self.flush();
        self.h.finalize()
    }
}

/// Per-node line of the run report (`procgen.report`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeReport {
    /// Node id.
    pub id: u32,
    /// Node type.
    pub ty: String,
    /// Wall time of the node, milliseconds (informational; never part of a hash).
    pub ms: f64,
    /// Items over all outputs.
    pub items: usize,
    /// Elements over all outputs.
    pub elements: usize,
    /// Served from the cache.
    pub cache_hit: bool,
    /// Skipped because every input was inactive.
    pub inactive: bool,
    /// Bypassed (`enabled: false`).
    pub bypassed: bool,
    /// Warnings `(code, message)`, each code once.
    pub warnings: Vec<(String, String)>,
    /// Counters a node reports (`unmatched`, ...).
    pub counters: BTreeMap<String, u64>,
}

/// The run report.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunReport {
    /// Per node in evaluation order.
    pub nodes: Vec<NodeReport>,
    /// Compile warnings.
    pub compile_warnings: Vec<GraphError>,
    /// Total wall time, milliseconds.
    pub total_ms: f64,
    /// Cache hits.
    pub cache_hits: usize,
    /// Per road (key: the spline item's `guid` scope, else its item index): spec 64 § 7.4.
    pub roads: BTreeMap<String, RoadReport>,
    /// Worker threads the run's schedule used (1 for [`Parallelism::Serial`]); informational, never
    /// part of a hash.
    pub threads: usize,
}

/// One road's line of the run report (spec 64 § 7.4).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoadReport {
    /// Length, meters.
    pub length_m: f32,
    /// Junctions on this road.
    pub junctions: u32,
    /// `[col0, row0, col1, row1)` the flatten touched, in full-terrain samples.
    pub flatten_rect: Option<[u32; 4]>,
    /// Distance along the road where the flatten first hit its carve or fill limit, meters.
    pub flatten_limit_at_m: Option<f32>,
}

impl RunReport {
    /// Adds a cell's report to a tiled run's (spec 66 § 2): node lines by id with times, counts and
    /// counters summed, a line a cache hit or inactive only when it is in every cell, warnings once per
    /// code; roads, which every cell sees whole, keep the largest counts and the union of the flatten
    /// rectangles.
    pub fn absorb(&mut self, cell: &RunReport) {
        self.total_ms += cell.total_ms;
        self.threads = self.threads.max(cell.threads);
        self.cache_hits += cell.cache_hits;
        for n in &cell.nodes {
            let Some(line) = self.nodes.iter_mut().find(|l| l.id == n.id) else {
                self.nodes.push(n.clone());
                continue;
            };
            line.ms += n.ms;
            line.items += n.items;
            line.elements += n.elements;
            line.cache_hit &= n.cache_hit;
            line.inactive &= n.inactive;
            line.bypassed &= n.bypassed;
            for w in &n.warnings {
                if !line.warnings.iter().any(|t| t.0 == w.0) {
                    line.warnings.push(w.clone());
                }
            }
            for (k, v) in &n.counters {
                *line.counters.entry(k.clone()).or_default() += v;
            }
        }
        for (k, r) in &cell.roads {
            let slot = self.roads.entry(k.clone()).or_default();
            slot.length_m = slot.length_m.max(r.length_m);
            slot.junctions = slot.junctions.max(r.junctions);
            slot.flatten_rect = match (slot.flatten_rect, r.flatten_rect) {
                (Some(a), Some(b)) => Some([a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]),
                (a, b) => a.or(b),
            };
            slot.flatten_limit_at_m = match (slot.flatten_limit_at_m, r.flatten_limit_at_m) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
    }

    /// The report line of node `id`.
    pub fn node(&self, id: u32) -> Option<&NodeReport> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Every warning code over all nodes.
    pub fn warning_codes(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .nodes
            .iter()
            .flat_map(|n| n.warnings.iter().map(|(c, _)| c.clone()))
            .chain(self.compile_warnings.iter().map(|w| w.code.clone()))
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

/// A successful run.
#[derive(Clone, Debug)]
pub struct RunResult {
    /// What the run produced.
    pub output: RunOutput,
    /// [`RunOutput::hash`].
    pub output_hash: [u8; 32],
    /// The report.
    pub report: RunReport,
    /// Every node's output bundles by node id, when `keep_debug` was set.
    pub debug: Option<BTreeMap<u32, Vec<Bundle>>>,
}

impl RunResult {
    /// The plant exports `plant.export` nodes recorded (spec 68 § 6.3 `RunResult::exports`).
    pub fn exports(&self) -> &[crate::plant::ExportJob] {
        &self.output.exports
    }
}

/// A failed or cancelled run (nothing is published).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    /// A node failed.
    #[error("node {node}: {code}: {message}")]
    Node {
        /// Node id.
        node: u32,
        /// Stable code.
        code: String,
        /// Detail.
        message: String,
    },
    /// The cancel token was tripped.
    #[error("cancelled")]
    Cancelled,
}

impl RunError {
    /// The stable code (`cancelled` for a cancel).
    pub fn code(&self) -> &str {
        match self {
            Self::Node { code, .. } => code,
            Self::Cancelled => "cancelled",
        }
    }
}

/// The per-node evaluation context (spec 62 § 5.1).
pub struct NodeCtx<'a> {
    /// The node's id.
    pub node_id: u32,
    /// `node_seed` (spec 62 § 6.2).
    pub node_seed: u32,
    /// `tier_seed`.
    pub tier_seed: u32,
    /// `node_run_seed` (per-run draws; no built-in node uses it).
    pub node_run_seed: u32,
    /// The run inputs.
    pub inputs: &'a RunInputs,
    /// The cancel token.
    pub cancel: &'a CancelToken,
    /// Per input pin: whether any edge feeds it.
    pub connected: Vec<bool>,
    /// The (sub)graph frame.
    pub frame: &'a Frame,
    pub(crate) evaluator: &'a Evaluator,
    pub(crate) graph: &'a CompiledGraph,
    pub(crate) output: &'a mut RunOutput,
    pub(crate) inactive: Vec<bool>,
    pub(crate) warnings: Vec<(String, String)>,
    pub(crate) counters: BTreeMap<String, u64>,
    pub(crate) roads: BTreeMap<String, RoadReport>,
    /// The pool element chunks run on (`None`: one after another on this thread).
    pub(crate) pool: Option<&'a TaskPool>,
    /// The columns of this node's point outputs the per-node finiteness check must read.
    pub(crate) finite_check: FiniteCheck,
}

impl NodeCtx<'_> {
    /// Runs `f` over `[0, len)` split into [`CHUNK`]-element ranges and returns the per-chunk results
    /// **in chunk order** (spec 62 § 5.1), whatever order the chunks finished in. With a pool the
    /// chunks run in parallel; without one they run one after another. Either way the cancel token
    /// is checked per chunk. Callers concatenate the results (or combine reductions) in that order.
    ///
    /// # Errors
    ///
    /// `cancelled` when the token trips.
    pub fn map_chunks<R, F>(&self, len: usize, f: F) -> Result<Vec<R>, NodeError>
    where
        R: Send + 'static,
        F: Fn(std::ops::Range<usize>) -> R + Sync,
    {
        let chunks = len.div_ceil(CHUNK);
        let range = |c: usize| c * CHUNK..((c + 1) * CHUNK).min(len);
        match self.pool {
            Some(pool) if chunks > 1 => {
                let parent = tracing::Span::current();
                let cancel = self.cancel;
                let (f, range) = (&f, &range);
                let mut done: Vec<Option<(usize, R)>> = pool.scope(|s| {
                    for c in 0..chunks {
                        let parent = parent.clone();
                        s.spawn(async move {
                            let _probe = crate::probe::pause();
                            if cancel.is_cancelled() {
                                return None;
                            }
                            let _span =
                                tracing::info_span!(parent: &parent, "procgen.chunk", index = c).entered();
                            Some((c, f(range(c))))
                        });
                    }
                });
                if done.iter().any(Option::is_none) {
                    return Err(NodeError::new("cancelled", "run cancelled"));
                }
                done.sort_by_key(|d| d.as_ref().map_or(0, |x| x.0));
                Ok(done.into_iter().flatten().map(|(_, r)| r).collect())
            }
            _ => {
                let mut v = Vec::with_capacity(chunks);
                for c in 0..chunks {
                    self.check_cancel()?;
                    v.push(f(range(c)));
                }
                Ok(v)
            }
        }
    }

    /// Runs `f` once per job (typically disjoint `chunks_mut` slices of one [`CHUNK`] each), in
    /// parallel with a pool and in order without one. Jobs must not depend on each other; the
    /// result is then the same either way. The cancel token is checked per job.
    ///
    /// # Errors
    ///
    /// `cancelled` when the token trips.
    pub fn for_each_job<J, F>(&self, jobs: Vec<J>, f: F) -> Result<(), NodeError>
    where
        J: Send,
        F: Fn(J) + Sync,
    {
        match self.pool {
            Some(pool) if jobs.len() > 1 => {
                let parent = tracing::Span::current();
                let cancel = self.cancel;
                let f = &f;
                let done: Vec<bool> = pool.scope(|s| {
                    for (index, job) in jobs.into_iter().enumerate() {
                        let parent = parent.clone();
                        s.spawn(async move {
                            let _probe = crate::probe::pause();
                            if cancel.is_cancelled() {
                                return false;
                            }
                            let _span =
                                tracing::info_span!(parent: &parent, "procgen.chunk", index).entered();
                            f(job);
                            true
                        });
                    }
                });
                if done.iter().all(|d| *d) {
                    Ok(())
                } else {
                    Err(NodeError::new("cancelled", "run cancelled"))
                }
            }
            _ => {
                for job in jobs {
                    self.check_cancel()?;
                    f(job);
                }
                Ok(())
            }
        }
    }

    /// The number of workers element chunks may use (1 without a pool).
    pub fn threads(&self) -> usize {
        self.pool.map_or(1, TaskPool::thread_num)
    }

    /// Runs `f` once per job, each on its own child context, in parallel with a pool and in order
    /// without one, and returns the results **in job order**. A child context has this node's seeds,
    /// inputs, world, limits and pool, and its own warnings, counters, road lines and run output;
    /// after the jobs finish they are merged into this context in job order (warnings kept once per
    /// code, the first job's message winning; counters summed; outputs through
    /// [`NodeCtx::merge_output`]). The result is then the same as calling the jobs one after another
    /// on this context, provided a job's run output (if any) merges the same either way: jobs that
    /// run subgraphs must not use this, because a subgraph's element cap reads the whole run output.
    ///
    /// # Errors
    ///
    /// The first failing job's error in job order (later jobs' results and counters are dropped, as
    /// a serial run would never have reached them); `cancelled` when the token trips.
    pub(crate) fn fork_jobs<J, R, F>(&mut self, jobs: Vec<J>, f: F) -> Result<Vec<R>, NodeError>
    where
        J: Send,
        R: Send + 'static,
        F: Fn(&mut NodeCtx<'_>, J) -> Result<R, NodeError> + Sync,
    {
        let (node_id, node_seed, tier_seed, node_run_seed) =
            (self.node_id, self.node_seed, self.tier_seed, self.node_run_seed);
        let (inputs, cancel, frame, evaluator, graph, pool) = (
            self.inputs,
            self.cancel,
            self.frame,
            self.evaluator,
            self.graph,
            self.pool,
        );
        let (connected, n_inactive, finite_check) =
            (self.connected.clone(), self.inactive.len(), self.finite_check);
        let connected = &connected;
        let run_one = |job: J| -> ForkedJob<R> {
            let mut output = RunOutput::default();
            let mut child = NodeCtx {
                node_id,
                node_seed,
                tier_seed,
                node_run_seed,
                inputs,
                cancel,
                connected: connected.clone(),
                frame,
                evaluator,
                graph,
                output: &mut output,
                inactive: vec![false; n_inactive],
                warnings: Vec::new(),
                counters: BTreeMap::new(),
                roads: BTreeMap::new(),
                pool,
                finite_check,
            };
            let result = f(&mut child, job);
            let NodeCtx {
                inactive,
                warnings,
                counters,
                roads,
                ..
            } = child;
            ForkedJob {
                result,
                output,
                inactive,
                warnings,
                counters,
                roads,
            }
        };
        let mut results = Vec::with_capacity(jobs.len());
        match pool {
            Some(pool) if jobs.len() > 1 => {
                let parent = tracing::Span::current();
                let run_one = &run_one;
                let mut done: Vec<(usize, Option<ForkedJob<R>>)> = pool.scope(|s| {
                    for (index, job) in jobs.into_iter().enumerate() {
                        let parent = parent.clone();
                        s.spawn(async move {
                            let _probe = crate::probe::pause();
                            if cancel.is_cancelled() {
                                return (index, None);
                            }
                            let _span = tracing::info_span!(parent: &parent, "procgen.job", index).entered();
                            (index, Some(run_one(job)))
                        });
                    }
                });
                done.sort_by_key(|d| d.0);
                for (_, job) in done {
                    let job = job.ok_or_else(|| NodeError::new("cancelled", "run cancelled"))?;
                    results.push(self.absorb_job(job)?);
                }
            }
            _ => {
                for job in jobs {
                    self.check_cancel()?;
                    let job = run_one(job);
                    results.push(self.absorb_job(job)?);
                }
            }
        }
        Ok(results)
    }

    /// Merges one forked job's side effects into this context; returns its result.
    fn absorb_job<R>(&mut self, job: ForkedJob<R>) -> Result<R, NodeError> {
        let r = job.result?;
        for (code, message) in job.warnings {
            self.warn(&code, message);
        }
        for (name, n) in job.counters {
            *self.counters.entry(name).or_default() += n;
        }
        for (key, line) in job.roads {
            self.roads.insert(key, line);
        }
        for (slot, set) in self.inactive.iter_mut().zip(job.inactive) {
            *slot |= set;
        }
        if job.output != RunOutput::default() {
            self.merge_output(&job.output);
        }
        Ok(r)
    }

    /// Merges a subgraph run's published output into this run's output.
    ///
    /// Instance sets, prefabs, meshes, terrain edits, road decals and exports accumulate over calls
    /// (and over `graph.loop` iterations), in call order. A named table, point set or mask holds one
    /// value per run: a call that publishes a name the run already has replaces it, as a second
    /// `out.*` node of the same name in one graph does (spec 62 § 7.3). The replacement warns
    /// `output_name_replaced` once per calling node; to keep every iteration's points, return them
    /// through a loop output pin (concatenated) and publish them once in the caller.
    pub fn merge_output(&mut self, sub: &RunOutput) {
        for set in &sub.instance_sets {
            self.output
                .set_for(set)
                .instances
                .extend_from_slice(&set.instances);
        }
        self.output.prefabs.extend(sub.prefabs.iter().cloned());
        let mut replaced: Vec<String> = Vec::new();
        for (k, t) in &sub.tables {
            if self.output.tables.insert(k.clone(), t.clone()).is_some() {
                replaced.push(format!("table `{k}`"));
            }
        }
        for (k, p) in &sub.points {
            if self.output.points.insert(k.clone(), p.clone()).is_some() {
                replaced.push(format!("points `{k}`"));
            }
        }
        for (k, m) in &sub.masks {
            if self.output.masks.insert(k.clone(), m.clone()).is_some() {
                replaced.push(format!("mask `{k}`"));
            }
        }
        for m in &sub.meshes {
            GeneratedMesh::merge_into(&mut self.output.meshes, m.clone());
        }
        self.output
            .terrain_edits
            .extend(sub.terrain_edits.iter().cloned());
        self.output.road_decals.extend_from_slice(&sub.road_decals);
        self.output.exports.extend(sub.exports.iter().cloned());
        if !replaced.is_empty() {
            self.warn(
                "output_name_replaced",
                format!(
                    "a called graph published {} again: the last write wins (named outputs hold one value per run)",
                    replaced.join(", ")
                ),
            );
        }
    }

    /// Compiles and runs a subgraph by path in a child frame; returns its result.
    ///
    /// # Errors
    ///
    /// `unknown_graph` (not captured), `procgen_graph_invalid` (does not compile), `limit_exceeded`
    /// (`call_depth`), or the subgraph's own node error.
    pub fn run_subgraph(
        &mut self,
        path: &str,
        overrides: &BTreeMap<String, crate::SettingValue>,
        graph_inputs: BTreeMap<String, Bundle>,
        loop_index: Option<i64>,
    ) -> Result<RunResult, NodeError> {
        let depth = self.frame.depth + 1;
        if depth > self.inputs.limits.call_depth {
            return Err(NodeError::new(
                "limit_exceeded",
                format!("call_depth: {depth} > {}", self.inputs.limits.call_depth),
            ));
        }
        let text = self
            .inputs
            .graphs
            .get(path)
            .ok_or_else(|| NodeError::new("unknown_graph", format!("graph `{path}` was not captured")))?;
        // One compile per (path, text, overrides) per run: a `graph.loop` used to parse and compile
        // its body on every iteration (audit M2).
        let key = {
            let mut h = blake3::Hasher::new();
            for part in [path, text.as_str(), &format!("{overrides:?}")] {
                h.update(&(part.len() as u64).to_le_bytes());
                h.update(part.as_bytes());
            }
            *h.finalize().as_bytes()
        };
        let cached = self.evaluator.subgraphs.lock().get(&key).cloned();
        let compiled = match cached {
            Some(c) => c,
            None => {
                let g = crate::graph::ProcGraph::from_ron(text)
                    .map_err(|e| NodeError::new("procgen_graph_invalid", e.to_string()))?;
                let c = Arc::new(
                    g.compile_full(&self.graph.registry, overrides, &|p| {
                        self.inputs.graphs.get(p).cloned()
                    })
                    .map_err(|e| NodeError::new("procgen_graph_invalid", e.to_string()))?,
                );
                self.evaluator.subgraphs.lock().insert(key, c.clone());
                c
            }
        };
        let mut loops = self.frame.loop_index.clone();
        loops.extend(loop_index);
        let frame = Frame {
            depth,
            graph_inputs,
            loop_index: loops,
        };
        match self
            .evaluator
            .run_frame(&compiled, self.inputs, self.cancel, &frame)
        {
            Ok(r) => {
                self.merge_output(&r.output);
                // Each child frame starts its element budget at zero, so bound what the calls merge
                // into this run's output instead (audit zai M3).
                let returned = r
                    .output
                    .graph_outputs
                    .values()
                    .map(Bundle::elements)
                    .sum::<usize>();
                let merged = self.output.instance_count()
                    + self.output.prefabs.len()
                    + self.output.points.values().map(PointSet::len).sum::<usize>()
                    + returned;
                let cap = self.inputs.limits.total_elements;
                if merged > cap {
                    return Err(NodeError::new(
                        "limit_exceeded",
                        format!("total_elements: subgraph outputs merged into the run hold {merged} > {cap}"),
                    ));
                }
                Ok(r)
            }
            Err(RunError::Cancelled) => Err(NodeError::new("cancelled", "run cancelled")),
            Err(RunError::Node { node, code, message }) => Err(NodeError::new(
                &code,
                format!("in `{path}` node {node}: {message}"),
            )),
        }
    }

    /// The captured world.
    pub fn world(&self) -> &dyn WorldSnapshot {
        self.inputs.world.as_ref()
    }

    /// The run's limits.
    pub fn limits(&self) -> &RunLimits {
        &self.inputs.limits
    }

    /// The cell a tiled run owns (spec 66 § 2); `None` untiled.
    pub fn tile(&self) -> Option<TileFrame> {
        self.inputs.tile
    }

    /// The road report line under `key`, created on first use (spec 64 § 7.4).
    pub fn road_report(&mut self, key: &str) -> &mut RoadReport {
        self.roads.entry(key.to_string()).or_default()
    }

    /// Whether the run may produce terrain edits.
    pub fn terrain_edits(&self) -> TerrainEditGate {
        self.inputs.terrain_edits
    }

    /// Marks output pin `pin` inactive (control flow, spec 63 § 10).
    pub fn set_inactive(&mut self, pin: usize) {
        if let Some(slot) = self.inactive.get_mut(pin) {
            *slot = true;
        }
    }

    /// Records a warning (kept once per code per node).
    pub fn warn(&mut self, code: &str, message: impl Into<String>) {
        if !self.warnings.iter().any(|(c, _)| c == code) {
            self.warnings.push((code.to_string(), message.into()));
        }
    }

    /// Adds to a report counter.
    pub fn count(&mut self, name: &str, n: u64) {
        *self.counters.entry(name.to_string()).or_default() += n;
    }

    /// The run output (spawn and export nodes write here).
    pub fn output(&mut self) -> &mut RunOutput {
        self.output
    }

    /// Fails with `cancelled` when the token was tripped (call once per [`CHUNK`]).
    ///
    /// # Errors
    ///
    /// A `cancelled` node error, which the evaluator turns into [`RunError::Cancelled`].
    pub fn check_cancel(&self) -> Result<(), NodeError> {
        if self.cancel.is_cancelled() {
            Err(NodeError::new("cancelled", "run cancelled"))
        } else {
            Ok(())
        }
    }

    /// Fails with `limit_exceeded` when `n` points would exceed the per-item cap.
    ///
    /// # Errors
    ///
    /// `limit_exceeded` naming `points_per_item`.
    pub fn check_points(&self, n: usize) -> Result<(), NodeError> {
        if n > self.inputs.limits.points_per_item {
            Err(NodeError::new(
                "limit_exceeded",
                format!("points_per_item: {n} > {}", self.inputs.limits.points_per_item),
            ))
        } else {
            Ok(())
        }
    }

    /// The point seed of a new point at `p` (spec 62 § 6.2).
    pub fn point_seed(&self, p: [f32; 3]) -> u32 {
        seeds::point_seed(p, self.tier_seed)
    }

    /// Draw `slot` in `[0, 1)` for an element seed.
    pub fn draw(&self, element_seed: u32, slot: u32) -> f64 {
        seeds::draw(element_seed, self.node_seed, slot)
    }
}

/// What one [`NodeCtx::fork_jobs`] job produced: its result and its child context's side effects.
struct ForkedJob<R> {
    result: Result<R, NodeError>,
    output: RunOutput,
    inactive: Vec<bool>,
    warnings: Vec<(String, String)>,
    counters: BTreeMap<String, u64>,
    roads: BTreeMap<String, RoadReport>,
}

/// One cache entry: the outputs, their estimated bytes and the last-use tick.
type CacheEntry = (Arc<Vec<Bundle>>, usize, u64);

/// An LRU cache of node outputs with a byte budget (spec 62 § 5.4). Caching never changes results.
#[derive(Debug, Default)]
pub struct ProcCache {
    budget: usize,
    used: usize,
    clock: u64,
    entries: BTreeMap<[u8; 32], CacheEntry>,
    /// Last-use clock to key, so eviction pops the oldest key without scanning `entries`.
    order: BTreeMap<u64, [u8; 32]>,
    hits: u64,
    misses: u64,
}

impl ProcCache {
    /// A cache with a byte budget (0 disables it).
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget: budget_bytes,
            ..Self::default()
        }
    }

    /// `(hits, misses)` so far.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    /// Bytes held.
    pub fn used_bytes(&self) -> usize {
        self.used
    }

    /// How many cache entries eviction has inspected in this process. Dropping one entry inspects
    /// one key.
    pub fn evict_inspections() -> u64 {
        EVICT_INSPECTIONS.load(Ordering::Relaxed)
    }

    fn get(&mut self, key: &[u8; 32]) -> Option<Arc<Vec<Bundle>>> {
        self.clock += 1;
        let clock = self.clock;
        let (old_clock, value) = {
            let Some(entry) = self.entries.get_mut(key) else {
                self.misses += 1;
                return None;
            };
            let old_clock = entry.2;
            entry.2 = clock;
            (old_clock, Arc::clone(&entry.0))
        };
        self.order.remove(&old_clock);
        self.order.insert(clock, *key);
        self.hits += 1;
        Some(value)
    }

    fn put(&mut self, key: [u8; 32], value: Arc<Vec<Bundle>>, bytes: usize) {
        if bytes > self.budget {
            return;
        }
        self.clock += 1;
        if let Some(old) = self.entries.insert(key, (value, bytes, self.clock)) {
            self.order.remove(&old.2);
            self.used -= old.1;
        }
        self.order.insert(self.clock, key);
        self.used += bytes;
        while self.used > self.budget {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            EVICT_INSPECTIONS.fetch_add(1, Ordering::Relaxed);
            if let Some(e) = self.entries.remove(&oldest) {
                self.used -= e.1;
            }
        }
    }
}

/// Estimated bytes of a bundle (built-ins 92 bytes per point, attributes by width).
pub fn estimate_bytes(b: &Bundle) -> usize {
    b.0.iter()
        .map(|item| match &item.payload {
            Payload::Points(p) => {
                let attrs: usize = p.attrs.values().map(|c| c.ty().width().unwrap_or(4) * 8).sum();
                p.len() * (92 + attrs)
            }
            Payload::Table(t) => t.rows * t.columns.len() * 16,
            Payload::Ground(g) => g.heights.len() * 4,
            Payload::Mask(m) => m.values.len() * 4,
            Payload::Mesh(m) => m.positions.len() * 24 + m.indices.len() * 4,
            Payload::Shape(_) => 64,
            Payload::Spline(sp) => sp.spline.points.len() * 64,
            Payload::Polygon(p) => p.rings.iter().map(Vec::len).sum::<usize>() * 16,
            Payload::PlantSkeleton(s) => s.node_count() * 120,
            Payload::PlantMesh(m) => m
                .slots
                .iter()
                .map(|s| s.positions.len() * 64 + s.indices.len() * 4)
                .sum(),
        })
        .sum()
}

/// What a planned (tiered) run does with one node.
#[derive(Clone, Debug)]
pub(crate) enum NodePlan {
    /// Evaluate it.
    Eval,
    /// Use these output bundles (one per output pin).
    Inject(Vec<Bundle>),
    /// Not part of this tier: empty, inactive outputs.
    Skip,
}

/// How an [`Evaluator`] schedules a run (spec 62 § 5.1). Every schedule gives bit-identical output:
/// node outputs are stored per node, nodes that write run output write in compiled order, element
/// chunks are concatenated in chunk order and the report is reduced in compiled order.
#[derive(Clone, Default)]
pub enum Parallelism {
    /// One node at a time in compiled order, element chunks one after another, on the calling thread
    /// (the reference schedule).
    Serial,
    /// Ready nodes and element chunks on the global compute pool (`regia_core::tasks::ComputePool`).
    #[default]
    Compute,
    /// Ready nodes and element chunks on this pool (tests, the agent's `threads` option).
    Pool(Arc<TaskPool>),
}

impl std::fmt::Debug for Parallelism {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serial => f.write_str("Serial"),
            Self::Compute => f.write_str("Compute"),
            Self::Pool(p) => write!(f, "Pool({} threads)", p.thread_num()),
        }
    }
}

impl Parallelism {
    /// `0` is [`Parallelism::Serial`]; otherwise a dedicated pool of `threads` workers (at most 256).
    pub fn threads(threads: usize) -> Self {
        if threads == 0 {
            return Self::Serial;
        }
        Self::Pool(Arc::new(
            bevy_tasks::TaskPoolBuilder::new()
                .num_threads(threads.min(256))
                .thread_name("procgen".to_string())
                .build(),
        ))
    }

    /// The pool, if any.
    pub fn pool(&self) -> Option<&TaskPool> {
        match self {
            Self::Serial => None,
            Self::Compute => Some(regia_core::tasks::ComputePool::get()),
            Self::Pool(p) => Some(p),
        }
    }

    /// The default schedule: [`Parallelism::Compute`], or `REGIA_PROCGEN_THREADS=n` when that is set to
    /// a number. Determinism checks run a whole test suite on a schedule with it (ADR-2731); results
    /// never depend on it.
    pub fn from_env() -> Self {
        // The schedule (and its pool) is built once per process: every evaluator shares it.
        static SCHEDULE: OnceLock<Option<Parallelism>> = OnceLock::new();
        SCHEDULE
            .get_or_init(|| {
                std::env::var("REGIA_PROCGEN_THREADS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .map(Self::threads)
            })
            .clone()
            .unwrap_or(Self::Compute)
    }

    /// Worker threads (1 for [`Parallelism::Serial`]).
    pub fn thread_count(&self) -> usize {
        self.pool().map_or(1, TaskPool::thread_num)
    }
}

/// Runs compiled graphs (spec 62 § 5.1). Pure: reads only its arguments.
#[derive(Default)]
pub struct Evaluator {
    cache: Option<Arc<Mutex<ProcCache>>>,
    /// How runs are scheduled.
    parallelism: Parallelism,
    /// Parallel runs redone serially because a node outside the writer order wrote run output.
    serial_fallbacks: AtomicU64,
    /// The last snapshot's content hash, so subgraph frames and loop iterations over the same
    /// snapshot hash it once.
    world_hash: Mutex<Option<WorldHashMemo>>,
    /// Subgraphs compiled during this evaluator's runs, by (path, text, overrides) hash.
    subgraphs: Mutex<BTreeMap<[u8; 32], Arc<CompiledGraph>>>,
}

/// A snapshot (kept alive, so its address cannot be reused) and its content hash.
type WorldHashMemo = (Arc<dyn WorldSnapshot>, Option<[u8; 32]>);

impl Evaluator {
    /// An evaluator without a cache, on [`Parallelism::from_env`].
    pub fn new() -> Self {
        Self::default().with_parallelism(Parallelism::from_env())
    }

    /// An evaluator sharing a node-output cache, on [`Parallelism::from_env`].
    pub fn with_cache(cache: Arc<Mutex<ProcCache>>) -> Self {
        Self {
            cache: Some(cache),
            ..Self::default()
        }
        .with_parallelism(Parallelism::from_env())
    }

    /// The same evaluator with another schedule (results never depend on it).
    pub fn with_parallelism(mut self, parallelism: Parallelism) -> Self {
        self.parallelism = parallelism;
        self
    }

    /// The schedule.
    pub fn parallelism(&self) -> &Parallelism {
        &self.parallelism
    }

    /// How many parallel runs were redone serially because a node that does not declare
    /// [`crate::nodes::NodeKind::writes_output`] wrote run output (0 for the built-in nodes; a test
    /// checks it).
    pub fn serial_fallbacks(&self) -> u64 {
        self.serial_fallbacks.load(Ordering::Relaxed)
    }

    /// [`WorldSnapshot::content_hash`] of the run's snapshot, memoized per snapshot.
    fn world_hash(&self, inputs: &RunInputs) -> Option<[u8; 32]> {
        let mut memo = self.world_hash.lock();
        if let Some((snap, hash)) = memo.as_ref()
            && Arc::ptr_eq(snap, &inputs.world)
        {
            return *hash;
        }
        let hash = inputs.world.content_hash();
        *memo = Some((inputs.world.clone(), hash));
        hash
    }

    /// Runs the graph.
    ///
    /// # Errors
    ///
    /// [`RunError::Node`] for the first failing node (including `limit_exceeded`, `attr_nan`,
    /// `check_failed`), [`RunError::Cancelled`] when the token trips.
    pub fn run(
        &self,
        graph: &CompiledGraph,
        inputs: &RunInputs,
        cancel: &CancelToken,
    ) -> Result<RunResult, RunError> {
        self.run_frame(graph, inputs, cancel, &Frame::default())
    }

    /// Runs a (sub)graph in a frame (`graph.call` / `graph.loop` run their graph through this).
    ///
    /// # Errors
    ///
    /// As [`Evaluator::run`].
    pub fn run_frame(
        &self,
        graph: &CompiledGraph,
        inputs: &RunInputs,
        cancel: &CancelToken,
        frame: &Frame,
    ) -> Result<RunResult, RunError> {
        self.run_inner(graph, inputs, cancel, frame, None).map(|(r, _)| r)
    }

    /// Runs one tier of a tiled graph (spec 66 § 3): per node in evaluation order, evaluate it,
    /// use injected bundles (coarse-tier data clipped to the cell), or skip it.
    pub(crate) fn run_planned(
        &self,
        graph: &CompiledGraph,
        inputs: &RunInputs,
        cancel: &CancelToken,
        plan: &[NodePlan],
    ) -> Result<(RunResult, Vec<Vec<Bundle>>), RunError> {
        self.run_inner(graph, inputs, cancel, &Frame::default(), Some(plan))
    }

    fn run_inner(
        &self,
        graph: &CompiledGraph,
        inputs: &RunInputs,
        cancel: &CancelToken,
        frame: &Frame,
        plan: Option<&[NodePlan]>,
    ) -> Result<(RunResult, Vec<Vec<Bundle>>), RunError> {
        let pool = self.parallelism.pool();
        let threads = self.parallelism.thread_count();
        let _span = tracing::info_span!(
            "procgen.run",
            nodes = graph.nodes.len(),
            depth = frame.depth,
            threads
        )
        .entered();
        let started = std::time::Instant::now();
        let run = RunCtx {
            graph,
            inputs,
            cancel,
            frame,
            plan,
            tier_seed: seeds::tier_seed(inputs.gen_seed, inputs.tier_index),
            cell_key: inputs.cell.map_or(0, |(i, j)| seeds::cell_key(i, j)),
            world_hash: OnceLock::new(),
            pool,
            // A planned (tiled) run hands every node's outputs to its caller and a debug run keeps
            // them; any other run may give an output to its last reader (ADR-2731).
            release_outs: plan.is_none() && !inputs.keep_debug,
        };
        let scheduled = match pool {
            Some(p) if graph.nodes.len() > 1 => match self.schedule_waves(&run, p)? {
                Some(done) => done,
                None => {
                    // A node outside the output-writing set wrote run output: the wave schedule
                    // cannot promise the serial result, so the run is redone serially.
                    self.serial_fallbacks.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        "procgen: a node wrote run output outside the writer order; reran serially"
                    );
                    self.schedule_serial(&run)?
                }
            },
            _ => self.schedule_serial(&run)?,
        };
        let Scheduled {
            output,
            mut report,
            outs,
        } = scheduled;
        report.compile_warnings = graph.warnings.clone();
        report.threads = threads;
        report.total_ms = started.elapsed().as_secs_f64() * 1000.0;
        let debug = inputs.keep_debug.then(|| {
            graph
                .nodes
                .iter()
                .zip(outs.iter().cloned())
                .map(|(n, o)| (n.id, o))
                .collect::<BTreeMap<_, _>>()
        });
        let output_hash = output.hash();
        Ok((
            RunResult {
                output,
                output_hash,
                report,
                debug,
            },
            outs,
        ))
    }

    /// The reference schedule: one node at a time in compiled order.
    fn schedule_serial(&self, run: &RunCtx<'_>) -> Result<Scheduled, RunError> {
        let n = run.graph.nodes.len();
        let mut st = SchedState::new(run);
        let mut output = RunOutput::default();
        for k in 0..n {
            if run.cancel.is_cancelled() {
                return Err(RunError::Cancelled);
            }
            let outcome = match self.prepare(run, k, &mut st.outs, &st.active, &mut st.readers) {
                Prepared::Done(o) => o,
                Prepared::Eval(ins) => self.eval_node(run, k, ins, &mut output),
            };
            st.complete(k, outcome);
            st.reduce(run, k)?;
        }
        Ok(st.finish(output))
    }

    /// The parallel schedule (spec 62 § 5.1): every node whose inputs are ready runs in one wave on
    /// the pool. Nodes that write run output ([`crate::nodes::NodeKind::writes_output`]) also wait for
    /// the previous such node, so they write in compiled order; the others write into a scratch output
    /// that must stay empty. Results are reduced in compiled order (limits, report lines, the first
    /// error), so the outcome equals [`Evaluator::schedule_serial`]'s. `Ok(None)` asks for a serial
    /// rerun (a scratch output was written).
    fn schedule_waves(&self, run: &RunCtx<'_>, pool: &TaskPool) -> Result<Option<Scheduled>, RunError> {
        let nodes = &run.graph.nodes;
        let n = nodes.len();
        let writer: Vec<bool> = nodes.iter().map(|nd| nd.kind.writes_output()).collect();
        let mut prev_writer = vec![None; n];
        let mut last = None;
        for k in 0..n {
            if writer[k] {
                prev_writer[k] = last;
                last = Some(k);
            }
        }
        let mut st = SchedState::new(run);
        let mut started = vec![false; n];
        let mut output = RunOutput::default();
        // Only nodes below `limit` are scheduled: a failure at node `k` (or a cumulative limit
        // crossed by the nodes up to `k`) means the serial run stops at or before `k`.
        let mut limit = n;
        let mut cursor = 0;
        let mut wave = 0u32;
        loop {
            if run.cancel.is_cancelled() {
                return Err(RunError::Cancelled);
            }
            while cursor < n && st.pending[cursor].is_some() {
                st.reduce(run, cursor)?;
                cursor += 1;
            }
            if cursor == n {
                break;
            }
            let mut jobs: Vec<(usize, Vec<Bundle>)> = Vec::new();
            let mut progressed = false;
            for k in cursor..limit {
                if started[k] {
                    continue;
                }
                let deps_ready = nodes[k]
                    .inputs
                    .iter()
                    .flatten()
                    .all(|&(src, _)| st.outs[src].is_some());
                let order_ready = prev_writer[k].is_none_or(|w: usize| st.done[w]);
                if !deps_ready || !order_ready {
                    continue;
                }
                started[k] = true;
                match self.prepare(run, k, &mut st.outs, &st.active, &mut st.readers) {
                    Prepared::Done(o) => {
                        st.complete(k, o);
                        progressed = true;
                    }
                    Prepared::Eval(ins) => jobs.push((k, ins)),
                }
            }
            if jobs.is_empty() {
                if progressed {
                    continue;
                }
                // Nothing runnable below the limit and nothing left to reduce: only a broken
                // invariant gets here; the serial schedule answers instead.
                return Ok(None);
            }
            wave += 1;
            let _wave_span = tracing::info_span!("procgen.wave", index = wave, nodes = jobs.len()).entered();
            let mut results: Vec<(usize, NodeOutcome, bool)> = if jobs.len() == 1 {
                jobs.into_iter()
                    .map(|(k, ins)| {
                        if writer[k] {
                            (k, self.eval_node(run, k, ins, &mut output), false)
                        } else {
                            let mut scratch = RunOutput::default();
                            let o = self.eval_node(run, k, ins, &mut scratch);
                            (k, o, scratch != RunOutput::default())
                        }
                    })
                    .collect()
            } else {
                let parent = tracing::Span::current();
                let mut out_slot = Some(&mut output);
                let writer = &writer;
                pool.scope(|s| {
                    for (k, ins) in jobs {
                        let parent = parent.clone();
                        let target = if writer[k] { out_slot.take() } else { None };
                        s.spawn(async move {
                            let _probe = crate::probe::pause();
                            let _task =
                                tracing::info_span!(parent: &parent, "procgen.task", index = k).entered();
                            match target {
                                Some(out) => (k, self.eval_node(run, k, ins, out), false),
                                None => {
                                    let mut scratch = RunOutput::default();
                                    let o = self.eval_node(run, k, ins, &mut scratch);
                                    // A writer that found the slot taken is impure by construction.
                                    (k, o, writer[k] || scratch != RunOutput::default())
                                }
                            }
                        });
                    }
                })
            };
            results.sort_by_key(|r| r.0);
            for (k, outcome, impure) in results {
                if impure {
                    return Ok(None);
                }
                if outcome.result.is_err() {
                    limit = limit.min(k + 1);
                }
                st.complete(k, outcome);
            }
            // The serial run fails at the first node where the running sums cross a cap; once the
            // completed nodes up to `k` cross it, nothing after `k` matters.
            let limits = &run.inputs.limits;
            let (mut elements, mut bytes) = (0usize, 0usize);
            for k in 0..limit {
                if let Some((e, b)) = st.counts[k] {
                    elements += e;
                    bytes += b;
                    if elements > limits.total_elements || bytes > limits.memory_bytes {
                        limit = k + 1;
                        break;
                    }
                }
            }
        }
        Ok(Some(st.finish(output)))
    }

    /// What node `k` does before any evaluation: a planned skip or injection, or an inactive skip;
    /// else its gathered input bundles. The last reader of an output (`readers` counts them, empty
    /// when outputs must stay) takes it instead of sharing it, so a node at the end of a linear chain
    /// holds the only reference and writes in place (ADR-2731).
    fn prepare(
        &self,
        run: &RunCtx<'_>,
        k: usize,
        outs: &mut [Option<Vec<Bundle>>],
        active: &[Option<Vec<bool>>],
        readers: &mut [Vec<u32>],
    ) -> Prepared {
        let node = &run.graph.nodes[k];
        let n_out = node.pins.outputs.len();
        let line = NodeReport {
            id: node.id,
            ty: node.kind.id().to_string(),
            ..NodeReport::default()
        };
        match run.plan.and_then(|p| p.get(k)) {
            Some(NodePlan::Skip) => {
                return Prepared::Done(NodeOutcome {
                    line: NodeReport {
                        inactive: true,
                        ..line
                    },
                    result: Ok((vec![Bundle::empty(); n_out], vec![false; n_out])),
                    ..NodeOutcome::default()
                });
            }
            Some(NodePlan::Inject(bundles)) => {
                let mut b = bundles.clone();
                b.resize(n_out, Bundle::empty());
                return Prepared::Done(NodeOutcome {
                    line: NodeReport {
                        items: b.iter().map(|x| x.0.len()).sum(),
                        elements: b.iter().map(Bundle::elements).sum(),
                        cache_hit: true,
                        ..line
                    },
                    result: Ok((b, vec![true; n_out])),
                    ..NodeOutcome::default()
                });
            }
            Some(NodePlan::Eval) | None => {}
        }
        // Gather inputs; a node all of whose connected inputs are inactive is skipped.
        let mut connected = false;
        let mut any_active = false;
        let mut ins: Vec<Bundle> = Vec::with_capacity(node.inputs.len());
        for srcs in &node.inputs {
            let mut b = Bundle::empty();
            for &(src, pin) in srcs {
                connected = true;
                let is_active = active[src]
                    .as_ref()
                    .and_then(|a| a.get(pin))
                    .copied()
                    .unwrap_or(false);
                let left = readers.get_mut(src).and_then(|r| r.get_mut(pin)).map(|r| {
                    *r = r.saturating_sub(1);
                    *r
                });
                if is_active {
                    any_active = true;
                    if let Some(o) = outs[src].as_mut().and_then(|o| o.get_mut(pin)) {
                        if left == Some(0) {
                            b.0.append(&mut o.0);
                        } else {
                            b.0.extend(o.0.iter().cloned());
                        }
                    }
                }
            }
            ins.push(b);
        }
        if connected && !any_active {
            return Prepared::Done(NodeOutcome {
                line: NodeReport {
                    inactive: true,
                    ..line
                },
                result: Ok((vec![Bundle::empty(); n_out], vec![false; n_out])),
                ..NodeOutcome::default()
            });
        }
        Prepared::Eval(ins)
    }

    /// Evaluates node `k` on gathered inputs (bypass, cache, override pins, the node itself and its
    /// per-node checks). Writes run output only through `output`.
    fn eval_node(
        &self,
        run: &RunCtx<'_>,
        k: usize,
        mut ins: Vec<Bundle>,
        output: &mut RunOutput,
    ) -> NodeOutcome {
        let node = &run.graph.nodes[k];
        let inputs = run.inputs;
        let node_started = std::time::Instant::now();
        let _node_span = tracing::info_span!("procgen.node", ty = node.kind.id(), id = node.id).entered();
        let n_out = node.pins.outputs.len();
        let mut out = NodeOutcome {
            line: NodeReport {
                id: node.id,
                ty: node.kind.id().to_string(),
                ..NodeReport::default()
            },
            counted: true,
            ..NodeOutcome::default()
        };
        let fail = |mut out: NodeOutcome, e: RunError| {
            out.result = Err(e);
            out
        };
        let node_seed = seeds::node_seed(run.tier_seed, node.seed_salt);
        let mut inactive = vec![false; n_out];
        let mut finite_check = FiniteCheck::All;
        let result: Vec<Bundle> = if !node.enabled {
            out.line.bypassed = true;
            let mut r = vec![Bundle::empty(); n_out];
            if let (Some(first), Some(slot)) = (ins.first(), r.first_mut()) {
                *slot = first.clone();
            }
            r
        } else {
            // A node that reads the world joins the snapshot's content hash to its key, and
            // skips the cache when the snapshot cannot say what it holds.
            let world = if node.kind.reads_world() {
                run.world_hash.get_or_init(|| self.world_hash(inputs)).as_ref()
            } else {
                None
            };
            let key = (node.kind.cacheable()
                && self.cache.is_some()
                && (world.is_some() || !node.kind.reads_world()))
            .then(|| cache_key(node, &ins, node_seed, inputs, run.cell_key, world));
            let hit = match (&key, &self.cache) {
                (Some(k), Some(c)) => c.lock().get(k),
                _ => None,
            };
            if let Some(h) = hit {
                out.line.cache_hit = true;
                out.cache_hit = true;
                (*h).clone()
            } else {
                let mut ctx = NodeCtx {
                    node_id: node.id,
                    node_seed,
                    tier_seed: run.tier_seed,
                    node_run_seed: seeds::node_seed(node_seed, run.cell_key),
                    inputs,
                    cancel: run.cancel,
                    connected: node.inputs.iter().map(|e| !e.is_empty()).collect(),
                    frame: run.frame,
                    evaluator: self,
                    graph: run.graph,
                    output,
                    inactive: vec![false; n_out],
                    warnings: Vec::new(),
                    counters: BTreeMap::new(),
                    roads: BTreeMap::new(),
                    pool: run.pool,
                    finite_check: FiniteCheck::All,
                };
                // Override pins (spec 62 § 4.1): the trailing `@<setting>` inputs replace their
                // settings for this evaluation and are not passed to the node.
                let declared = ins.len().saturating_sub(node.overrides.len());
                let overridden;
                let settings = if node.overrides.is_empty() {
                    &node.settings
                } else {
                    let mut s = node.settings.clone();
                    for (k, name) in node.overrides.iter().enumerate() {
                        let ty = node
                            .kind
                            .settings()
                            .iter()
                            .find(|d| d.name == name)
                            .map_or(crate::nodes::SettingType::Any, |d| d.ty);
                        let empty = Bundle::empty();
                        let bundle = ins.get(declared + k).unwrap_or(&empty);
                        match crate::graph::override_value(name, ty, bundle) {
                            Ok(Some(v)) => {
                                s.0.insert(name.clone(), v);
                            }
                            Ok(None) => {}
                            Err(message) => {
                                return fail(
                                    out,
                                    RunError::Node {
                                        node: node.id,
                                        code: "override_invalid".to_string(),
                                        message,
                                    },
                                );
                            }
                        }
                    }
                    overridden = s;
                    &overridden
                };
                let r = node.kind.eval_owned(&mut ctx, &mut ins[..declared], settings);
                finite_check = ctx.finite_check;
                inactive = ctx.inactive;
                out.line.warnings = ctx.warnings;
                out.line.counters = ctx.counters;
                out.roads = ctx.roads;
                let mut r = match r {
                    Ok(r) => r,
                    Err(e) if e.code == "cancelled" => return fail(out, RunError::Cancelled),
                    Err(e) => {
                        return fail(
                            out,
                            RunError::Node {
                                node: node.id,
                                code: e.code,
                                message: e.message,
                            },
                        );
                    }
                };
                r.resize(n_out, Bundle::empty());
                if let (Some(k), Some(c)) = (key, &self.cache)
                    && !inactive.iter().any(|x| *x)
                {
                    let bytes: usize = r.iter().map(estimate_bytes).sum();
                    c.lock().put(k, Arc::new(r.clone()), bytes);
                }
                r
            }
        };

        // The per-point limit and the non-finite check on everything this node produced.
        for b in &result {
            for item in &b.0 {
                if let Payload::Points(p) = &item.payload {
                    if p.len() > inputs.limits.points_per_item {
                        return fail(
                            out,
                            limit(node.id, "points_per_item", p.len(), inputs.limits.points_per_item),
                        );
                    }
                    if !points_finite(p, finite_check, run.pool)
                        && let Some((attr, i)) = p.first_non_finite()
                    {
                        return fail(
                            out,
                            RunError::Node {
                                node: node.id,
                                code: "attr_nan".to_string(),
                                message: format!("`{attr}` is not finite at element {i}"),
                            },
                        );
                    }
                }
            }
        }
        out.line.items = result.iter().map(|b| b.0.len()).sum();
        out.line.elements = result.iter().map(Bundle::elements).sum();
        out.line.ms = node_started.elapsed().as_secs_f64() * 1000.0;
        out.result = Ok((result, inactive.iter().map(|x| !x).collect()));
        out
    }
}

/// Floats per job of the output finiteness pre-check.
const FINITE_JOB: usize = 1 << 16;

/// `true` when every float of `v` is finite. No early exit inside, so the loop vectorizes.
fn floats_finite(v: &[f32]) -> bool {
    v.iter().fold(true, |ok, x| ok & x.is_finite())
}

/// The columns of a node's point outputs that can hold a value its input did not. Every set a node
/// reads was checked when its own node produced it, so a column the node carried over stays finite.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum FiniteCheck {
    /// Every float column and attribute (any node that does not say otherwise).
    #[default]
    All,
    /// `position`, `rotation` and `scale`.
    Transform,
    /// `density`.
    Density,
    /// None: the node only moves rows between sets.
    Nothing,
}

/// `true` when `PointSet::first_non_finite` would find nothing in the columns `check` names: they
/// are scanned without branches, in jobs on `pool` when the run has one, then the attribute columns
/// for `FiniteCheck::All` (a superset of what the plain scan reads). `false` only sends the caller
/// to `first_non_finite` for the exact `(attribute, index)`, so the error is that of the plain scan.
fn points_finite(p: &PointSet, check: FiniteCheck, pool: Option<&TaskPool>) -> bool {
    let columns: Vec<&[f32]> = match check {
        FiniteCheck::All => vec![
            p.position.as_flattened(),
            p.rotation.as_flattened(),
            p.scale.as_flattened(),
            p.color.as_flattened(),
            &p.density,
            p.bounds_min.as_flattened(),
            p.bounds_max.as_flattened(),
            &p.hardness,
        ],
        FiniteCheck::Transform => vec![
            p.position.as_flattened(),
            p.rotation.as_flattened(),
            p.scale.as_flattened(),
        ],
        FiniteCheck::Density => vec![&p.density],
        FiniteCheck::Nothing => return true,
    };
    let jobs: Vec<&[f32]> = columns.iter().flat_map(|c| c.chunks(FINITE_JOB)).collect();
    let builtins = match pool {
        Some(pool) if jobs.len() > 1 => pool
            .scope(|s| {
                for job in jobs.iter().copied() {
                    s.spawn(async move { floats_finite(job) });
                }
            })
            .into_iter()
            .all(|ok| ok),
        _ => jobs.iter().all(|job| floats_finite(job)),
    };
    builtins && (check != FiniteCheck::All || p.attrs.values().all(|c| c.first_non_finite().is_none()))
}

/// What one run's schedulers share: the graph, the inputs and the per-run seeds.
struct RunCtx<'a> {
    graph: &'a CompiledGraph,
    inputs: &'a RunInputs,
    cancel: &'a CancelToken,
    frame: &'a Frame,
    plan: Option<&'a [NodePlan]>,
    tier_seed: u32,
    cell_key: u32,
    /// The snapshot's content hash, looked up at most once per frame (world-reading nodes only).
    world_hash: OnceLock<Option<[u8; 32]>>,
    pool: Option<&'a TaskPool>,
    /// Whether a node's output may be handed to its last reader.
    release_outs: bool,
}

/// A node before evaluation.
enum Prepared {
    /// Settled without evaluating (planned skip or injection, inactive inputs).
    Done(NodeOutcome),
    /// Evaluate on these input bundles.
    Eval(Vec<Bundle>),
}

/// One node's result before the in-order reduction.
struct NodeOutcome {
    line: NodeReport,
    roads: BTreeMap<String, RoadReport>,
    /// The output bundles and per-output activity, or the node's failure.
    result: Result<(Vec<Bundle>, Vec<bool>), RunError>,
    /// Whether the outputs count toward `total_elements` / `memory_bytes` (evaluated and bypassed
    /// nodes; injected tier data does not).
    counted: bool,
    /// Served from the node cache.
    cache_hit: bool,
}

impl Default for NodeOutcome {
    fn default() -> Self {
        Self {
            line: NodeReport::default(),
            roads: BTreeMap::new(),
            result: Ok((Vec::new(), Vec::new())),
            counted: false,
            cache_hit: false,
        }
    }
}

/// A node's settled report parts, waiting for the in-order reduction.
struct Pending {
    line: NodeReport,
    roads: BTreeMap<String, RoadReport>,
    error: Option<RunError>,
    cache_hit: bool,
}

/// Per-node state shared by both schedules.
struct SchedState {
    outs: Vec<Option<Vec<Bundle>>>,
    /// Per node and output pin, the edges still to read it; empty when outputs must stay.
    readers: Vec<Vec<u32>>,
    active: Vec<Option<Vec<bool>>>,
    /// Completed (successfully or not).
    done: Vec<bool>,
    pending: Vec<Option<Pending>>,
    /// `(elements, estimated bytes)` of completed counted nodes.
    counts: Vec<Option<(usize, usize)>>,
    report: RunReport,
    live_elements: usize,
    live_bytes: usize,
}

/// A finished schedule.
struct Scheduled {
    output: RunOutput,
    report: RunReport,
    outs: Vec<Vec<Bundle>>,
}

impl SchedState {
    fn new(run: &RunCtx<'_>) -> Self {
        let n = run.graph.nodes.len();
        let mut readers = Vec::new();
        if run.release_outs {
            readers = run
                .graph
                .nodes
                .iter()
                .map(|node| vec![0u32; node.pins.outputs.len()])
                .collect();
            for &(src, pin) in run
                .graph
                .nodes
                .iter()
                .flat_map(|node| node.inputs.iter().flatten())
            {
                if let Some(count) = readers.get_mut(src).and_then(|r: &mut Vec<u32>| r.get_mut(pin)) {
                    *count += 1;
                }
            }
        }
        Self {
            outs: vec![None; n],
            readers,
            active: vec![None; n],
            done: vec![false; n],
            pending: (0..n).map(|_| None).collect(),
            counts: vec![None; n],
            report: RunReport::default(),
            live_elements: 0,
            live_bytes: 0,
        }
    }

    /// Stores node `k`'s outputs (consumers may read them at once) and parks its report parts.
    fn complete(&mut self, k: usize, o: NodeOutcome) {
        self.done[k] = true;
        let error = match o.result {
            Ok((bundles, active)) => {
                if o.counted {
                    let elements = bundles.iter().map(Bundle::elements).sum();
                    let bytes = bundles.iter().map(estimate_bytes).sum();
                    self.counts[k] = Some((elements, bytes));
                }
                self.outs[k] = Some(bundles);
                self.active[k] = Some(active);
                None
            }
            Err(e) => Some(e),
        };
        self.pending[k] = Some(Pending {
            line: o.line,
            roads: o.roads,
            error,
            cache_hit: o.cache_hit,
        });
    }

    /// Reduces node `k` in compiled order: road lines, its error, the cumulative limits, its report line.
    fn reduce(&mut self, run: &RunCtx<'_>, k: usize) -> Result<(), RunError> {
        let Some(p) = self.pending[k].take() else {
            return Ok(());
        };
        for (key, r) in p.roads {
            let slot = self.report.roads.entry(key).or_default();
            slot.length_m = slot.length_m.max(r.length_m);
            slot.junctions += r.junctions;
            slot.flatten_rect = r.flatten_rect.or(slot.flatten_rect);
            slot.flatten_limit_at_m = r.flatten_limit_at_m.or(slot.flatten_limit_at_m);
        }
        if let Some(e) = p.error {
            return Err(e);
        }
        if p.cache_hit {
            self.report.cache_hits += 1;
        }
        if let Some((elements, bytes)) = self.counts[k] {
            let limits = &run.inputs.limits;
            let id = run.graph.nodes[k].id;
            self.live_elements += elements;
            self.live_bytes += bytes;
            if self.live_elements > limits.total_elements {
                return Err(limit(
                    id,
                    "total_elements",
                    self.live_elements,
                    limits.total_elements,
                ));
            }
            if self.live_bytes > limits.memory_bytes {
                return Err(limit(id, "memory_bytes", self.live_bytes, limits.memory_bytes));
            }
        }
        self.report.nodes.push(p.line);
        Ok(())
    }

    fn finish(self, output: RunOutput) -> Scheduled {
        Scheduled {
            output,
            report: self.report,
            outs: self.outs.into_iter().map(Option::unwrap_or_default).collect(),
        }
    }
}

fn limit(node: u32, which: &str, value: usize, cap: usize) -> RunError {
    RunError::Node {
        node,
        code: "limit_exceeded".to_string(),
        message: format!("{which}: {value} > {cap}"),
    }
}

/// The cache key of a node evaluation; `world` is the snapshot's content hash for nodes that read
/// the world (audit H4: a key without it served pre-edit terrain after a sculpt).
fn cache_key(
    node: &crate::graph::CompiledNode,
    ins: &[Bundle],
    node_seed: u32,
    inputs: &RunInputs,
    cell_key: u32,
    world: Option<&[u8; 32]>,
) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    if let Some(w) = world {
        h.update(b"world");
        h.update(w);
    }
    h.update(node.kind.id().as_bytes());
    h.update(&node.kind.settings_version().to_le_bytes());
    h.update(node.settings_text.as_bytes());
    for b in ins {
        h.update(&b.hash());
    }
    h.update(&node_seed.to_le_bytes());
    h.update(&inputs.tier_index.to_le_bytes());
    h.update(&cell_key.to_le_bytes());
    h.update(&(inputs.quality as u8).to_le_bytes());
    // An inactive upstream and a missing edge both give an empty bundle, but `flow.quality` picks by
    // connection: the same bundles with other wires are a different evaluation.
    for pin in &node.inputs {
        h.update(&[u8::from(!pin.is_empty())]);
    }
    *h.finalize().as_bytes()
}

#[cfg(test)]
mod merge_tests {
    use super::GeneratedMesh;

    fn mesh(n: usize, colors: Vec<[f32; 4]>) -> GeneratedMesh {
        GeneratedMesh {
            material: "m".into(),
            positions: vec![[0.0; 3]; n],
            normals: vec![[0.0, 1.0, 0.0]; n],
            uvs: vec![[0.0; 2]; n],
            indices: Vec::new(),
            colors,
            collision: false,
        }
    }

    #[test]
    fn merging_colors_into_an_empty_mesh_keeps_them() {
        let red = [1.0, 0.0, 0.0, 1.0];
        let mut list = vec![mesh(0, Vec::new())];
        GeneratedMesh::merge_into(&mut list, mesh(3, vec![red; 3]));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].colors, vec![red; 3]);
        // A plain mesh merged after a colored one is padded white.
        GeneratedMesh::merge_into(&mut list, mesh(2, Vec::new()));
        assert_eq!(list[0].colors.len(), 5);
        assert_eq!(list[0].colors[4], [1.0; 4]);
    }
}

#[cfg(test)]
mod finite_tests {
    use super::{FiniteCheck, points_finite};
    use crate::data::{AttrColumn, Point, PointSet};

    fn set(n: usize) -> PointSet {
        let mut p = PointSet::new();
        for i in 0..n {
            p.push(Point {
                position: [i as f32, 0.5, -(i as f32)],
                ..Point::default()
            });
        }
        p.attrs.insert("h".into(), AttrColumn::Float(vec![1.0; n].into()));
        p
    }

    /// A narrowed check reads exactly the columns it names, and always catches a bad one of them.
    #[test]
    fn a_narrowed_finite_check_reads_only_its_columns() {
        let pool = bevy_tasks::TaskPoolBuilder::new().num_threads(4).build();
        type Bad = fn(&mut PointSet);
        let poison: [(&str, Bad); 4] = [
            ("position", |p| p.position[3][1] = f32::NAN),
            ("density", |p| p.density[3] = f32::INFINITY),
            ("color", |p| p.color[3][0] = f32::NAN),
            ("attr", |p| {
                p.attrs
                    .insert("h".into(), AttrColumn::Float(vec![f64::NAN; 10].into()));
            }),
        ];
        for (name, f) in poison {
            let mut p = set(10);
            f(&mut p);
            for (check, caught) in [
                (FiniteCheck::All, true),
                (FiniteCheck::Transform, name == "position"),
                (FiniteCheck::Density, name == "density"),
                (FiniteCheck::Nothing, false),
            ] {
                assert_eq!(points_finite(&p, check, None), !caught, "{name} {check:?}");
                assert_eq!(points_finite(&p, check, Some(&pool)), !caught, "{name} {check:?}");
            }
        }
    }

    /// The pre-check says "finite" exactly when `first_non_finite` finds nothing, for a NaN or an
    /// infinity in every float column (first, middle and last element, past one job of floats),
    /// serially and on a pool, so the `attr_nan` error stays the plain scan's.
    #[test]
    fn the_finite_pre_check_agrees_with_first_non_finite_for_every_column() {
        let n = 70_000;
        let pool = bevy_tasks::TaskPoolBuilder::new().num_threads(4).build();
        let clean = set(n);
        assert!(points_finite(&clean, FiniteCheck::All, None));
        assert!(points_finite(&clean, FiniteCheck::All, Some(&pool)));
        type Poison = fn(&mut PointSet, usize, f32);
        let poisons: [(&str, Poison); 9] = [
            ("position", |p, i, v| p.position[i][2] = v),
            ("rotation", |p, i, v| p.rotation[i][3] = v),
            ("scale", |p, i, v| p.scale[i][0] = v),
            ("color", |p, i, v| p.color[i][1] = v),
            ("density", |p, i, v| p.density[i] = v),
            ("bounds_min", |p, i, v| p.bounds_min[i][0] = v),
            ("bounds_max", |p, i, v| p.bounds_max[i][2] = v),
            ("hardness", |p, i, v| p.hardness[i] = v),
            ("h", |p, i, v| {
                if let Some(AttrColumn::Float(c)) = p.attrs.get_mut("h") {
                    c[i] = f64::from(v);
                }
            }),
        ];
        for (name, poison) in poisons {
            for i in [0, n / 2, n - 1] {
                for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                    let mut p = clean.clone();
                    poison(&mut p, i, v);
                    let found = p.first_non_finite();
                    assert!(found.is_some(), "{name}[{i}] = {v}: the plain scan finds it");
                    assert!(
                        !points_finite(&p, FiniteCheck::All, None),
                        "{name}[{i}] = {v}: serial pre-check"
                    );
                    assert!(
                        !points_finite(&p, FiniteCheck::All, Some(&pool)),
                        "{name}[{i}] = {v}: pool pre-check"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod perf_g_tests {
    use super::*;
    use crate::data::GroundData;

    fn proto(mesh: &str) -> InstanceSet {
        InstanceSet {
            mesh: mesh.to_string(),
            material: None,
            cast_shadows: true,
            collision: Collision::None,
            draw_distance_m: 100.0,
            fade_m: 0.0,
            cull_below_px: 2.0,
            instances: Vec::new(),
            layer: None,
        }
    }

    fn inst(x: f32) -> Instance {
        Instance {
            translation: [x, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            color: [1.0; 4],
            custom: [0.0; 4],
        }
    }

    /// Ten thousand inserts of one prototype clone the mesh key at most once.
    #[test]
    fn perf_g_set_for_does_not_clone_mesh_keys() {
        let rock = proto("rock.glb");
        let mut out = RunOutput::default();
        let before = mesh_key_clones();
        for i in 0..10_000 {
            out.set_for(&rock).instances.push(inst(i as f32));
        }
        let clones = mesh_key_clones() - before;
        assert!(clones <= 1, "one prototype cloned its key {clones} times");
        assert_eq!(out.instance_count(), 10_000);

        let tree = proto("tree.glb");
        let before = mesh_key_clones();
        for i in 0..100 {
            let p = if i % 2 == 0 { &rock } else { &tree };
            out.set_for(p);
        }
        let clones = mesh_key_clones() - before;
        assert!(clones <= 2, "two prototypes cloned keys {clones} times");
        assert_eq!(out.instance_sets.len(), 2);
    }

    /// Filling the budget and inserting one more entry inspects a handful of keys, not the whole map.
    #[test]
    fn perf_g_evicting_one_entry_inspects_few_keys() {
        let mut cache = ProcCache::new(64);
        let blob = Arc::new(Vec::new());
        for i in 0..64u8 {
            let mut key = [0u8; 32];
            key[0] = i;
            cache.put(key, Arc::clone(&blob), 1);
        }
        let before = ProcCache::evict_inspections();
        let mut key = [0u8; 32];
        key[0] = 200;
        cache.put(key, blob, 1);
        let inspected = ProcCache::evict_inspections() - before;
        assert!(inspected <= 4, "evicting one entry inspected {inspected} keys");
        assert_eq!(cache.used_bytes(), 64);
    }

    /// Two hashes of one snapshot walk the height buffer once.
    #[test]
    fn perf_g_content_hash_walks_heights_once() {
        let n = 64 * 64;
        let snap = StaticSnapshot {
            ground: Some(Arc::new(GroundData {
                rect: [0.0, 0.0, 64.0, 64.0],
                map_size: [64, 64],
                world_width: 64.0,
                origin: [0, 0],
                size: [64, 64],
                heights: vec![0.25; n],
                layers: Vec::new(),
            })),
            ..StaticSnapshot::default()
        };
        let before = ground_hash_samples();
        let first = snap.content_hash();
        let second = snap.content_hash();
        assert_eq!(first, second);
        assert_eq!(ground_hash_samples() - before, n as u64);
    }

    /// The counter is per-thread (like the astc speed guard): another test thread hashing
    /// ground — exactly what a concurrent lib test that runs a snapshot does — must not
    /// disturb this thread's count, or the exact deltas above flake under parallel tests.
    #[test]
    fn ground_hash_samples_counts_the_calling_thread_only() {
        let n = 64 * 64;
        let mk = || {
            Arc::new(StaticSnapshot {
                ground: Some(Arc::new(GroundData {
                    rect: [0.0, 0.0, 64.0, 64.0],
                    map_size: [64, 64],
                    world_width: 64.0,
                    origin: [0, 0],
                    size: [64, 64],
                    heights: vec![0.5; n],
                    layers: Vec::new(),
                })),
                ..StaticSnapshot::default()
            })
        };
        let mine = mk();
        let other = mk();
        let child = std::thread::spawn(move || {
            let _ = other.content_hash();
        });
        let before = ground_hash_samples();
        let _ = mine.content_hash();
        child.join().expect("hashing thread");
        assert_eq!(
            ground_hash_samples() - before,
            n as u64,
            "another thread's walk leaked into this thread's count"
        );
    }
}

#[cfg(test)]
mod hash_sink_tests {
    use super::HashSink;

    /// ADR-7238: buffering never changes the digest. Every split of one byte stream (single bytes,
    /// 4-byte floats, pieces across the flush size, one piece larger than it) hashes like the plain
    /// hasher fed the same calls.
    #[test]
    fn the_buffered_hash_equals_the_hasher_for_any_split() {
        let stream: Vec<u8> = (0..(HashSink::FLUSH * 3 + 777))
            .map(|i| (i.wrapping_mul(2_654_435_761usize) >> 13) as u8)
            .collect();
        let splits: [&[usize]; 5] = [
            &[1],
            &[4, 8, 4, 12],
            &[HashSink::FLUSH - 3, 5, 7],
            &[HashSink::FLUSH * 2 + 1],
            &[3, HashSink::FLUSH, 1, HashSink::FLUSH + 9],
        ];
        let want = *blake3::hash(&stream).as_bytes();
        for split in splits {
            let mut sink = HashSink::new();
            let mut plain = blake3::Hasher::new();
            let mut at = 0;
            let mut k = 0;
            while at < stream.len() {
                let n = split[k % split.len()].min(stream.len() - at);
                sink.update(&stream[at..at + n]);
                plain.update(&stream[at..at + n]);
                at += n;
                k += 1;
            }
            assert_eq!(*sink.finalize().as_bytes(), want, "split {split:?}");
            assert_eq!(*plain.finalize().as_bytes(), want);
        }
        assert_eq!(
            *HashSink::new().finalize().as_bytes(),
            *blake3::hash(&[]).as_bytes()
        );
    }
}
