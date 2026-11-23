// SPDX-License-Identifier: MPL-2.0
//! CDLOD-style continuous distance-dependent level of detail over the heightfield.
//!
//! Design derived from Filip Strugar, "Continuous Distance-Dependent Level of Detail for
//! Rendering Heightmaps" (2010; public paper): a quadtree of nodes over the world square, each
//! node drawn with one shared regular grid mesh, and vertices morphed onto the parent's coarser
//! grid inside a distance band so that LOD changes never pop and never crack.
//!
//! Level numbering: level 0 is the finest (leaf), `root_level` the coarsest (root, one node for
//! the whole map). A node at level `l` has world size `world_width / 2^(root_level - l)` and is
//! subdivided while the camera is closer than `range(l) = range_factor * size(l)`.
//!
//! On a rectangular map ([`TerrainMap::with_world_depth`], ADR-8369) the quadtree covers the
//! square of the longer side ([`TerrainMap::quadtree_width`]) centred on the map: nodes wholly
//! past the shorter side are never selected, and the draw clamps the vertices of a node that
//! straddles it onto the map's edge.
//!
//! # Crack-free guarantee
//!
//! With `range_factor >= sqrt(2)` (the config enforces `>= 2`), two edge-adjacent selected
//! nodes can never differ by more than one level: a coarse node at level `l+1` needs camera
//! distance `>= range(l+1)`, any shared edge point is within one diagonal of its subdivided
//! neighbour at level `l`, and `range(l+1) - range(l) > diag(size(l))` rules the violating
//! arrangement out. For a difference of exactly one, the fine node's vertices on the shared
//! edge are farther than `range(l+1)` — past the end of their morph band — so they sit exactly
//! on the coarse lattice at the same bilinearly sampled heights as the coarse node's vertices.
//!
//! The coarse node's vertices on that edge must not morph either. The finer node's parent was
//! split, so it is closer than `range(l+1)`, and every point of its border is closer than
//! `range(l+1) + sqrt(2) * size(l+1)` (one diagonal further). The coarse node's morph band
//! therefore starts no earlier than that: [`CdlodConfig::effective_morph_fraction`] caps the
//! morph fraction at `1 - sqrt(2) / range_factor`, which only binds for wide bands at small
//! range factors (`range_factor` 2 with `morph_fraction` above about 0.29; ADR-873).
//! No T-vertices, no cracks. This is verified in the tests below and in
//! [`crate::lod_seams`], which measures the gap along every shared edge of a selection.
//!
//! All selection math is CPU-side and f32-deterministic; the WGSL vertex shader mirrors
//! [`eval_node_vertex`] and [`vertex_morph_t`].

use regia_core::math::{Aabb, Frustum};
use thiserror::Error;

use crate::deform::DirtyRect;
use crate::sampler::TerrainMap;

/// Default grid resolution of the shared node mesh (cells per node edge).
pub const DEFAULT_GRID_SIZE: u32 = 16;

/// Errors returned for invalid CDLOD configurations.
#[derive(Clone, Debug, Error)]
pub enum CdlodError {
    /// The grid size must be a power of two in `4..=256`.
    #[error("grid size must be a power of two in 4..=256, got {0}")]
    BadGridSize(u32),
    /// The range factor must be `>= 2` (crack-free guarantee, module docs).
    #[error("range factor must be >= 2.0, got {0}")]
    BadRangeFactor(f32),
    /// The morph fraction must lie in `(0, 0.5]`.
    #[error("morph fraction must lie in (0, 0.5], got {0}")]
    BadMorphFraction(f32),
    /// The quadtree root level must be `<= 16`.
    #[error("root level must be <= 16, got {0}")]
    RootLevelTooDeep(u32),
}

/// CDLOD tuning: shared grid mesh resolution, LOD ranges and morph band.
#[derive(Clone, Debug)]
pub struct CdlodConfig {
    /// Grid cells per node edge (power of two). The shared grid mesh has
    /// `(grid_size + 1)^2` vertices.
    pub grid_size: u32,
    /// `range(l) = range_factor * node_size(l)`: the distance under which level-`l` nodes
    /// subdivide. Must be `>= 2` (module docs).
    pub range_factor: f32,
    /// Width of the morph band as a fraction of the node's distance band, in `(0, 0.5]`.
    /// Larger bands start morphing (softening detail) earlier. Selection uses
    /// [`CdlodConfig::effective_morph_fraction`], which caps it so neighbouring levels never
    /// crack.
    pub morph_fraction: f32,
}

impl Default for CdlodConfig {
    fn default() -> Self {
        Self {
            grid_size: DEFAULT_GRID_SIZE,
            range_factor: 4.0,
            morph_fraction: 0.25,
        }
    }
}

impl CdlodConfig {
    /// A configuration with the given grid size and default ranges.
    pub fn with_grid_size(grid_size: u32) -> Self {
        Self {
            grid_size,
            ..Default::default()
        }
    }

    /// Validates the configuration.
    ///
    /// # Errors
    ///
    /// See [`CdlodError`].
    pub fn validate(&self) -> Result<(), CdlodError> {
        if self.grid_size < 4 || self.grid_size > 256 || !self.grid_size.is_power_of_two() {
            return Err(CdlodError::BadGridSize(self.grid_size));
        }
        if !self.range_factor.is_finite() || self.range_factor < 2.0 {
            return Err(CdlodError::BadRangeFactor(self.range_factor));
        }
        if !self.morph_fraction.is_finite() || self.morph_fraction <= 0.0 || self.morph_fraction > 0.5 {
            return Err(CdlodError::BadMorphFraction(self.morph_fraction));
        }
        Ok(())
    }

    /// The morph fraction selection actually uses: [`CdlodConfig::morph_fraction`], capped at
    /// `1 - sqrt(2) / range_factor` so a coarse node's border never starts to morph where a
    /// finer neighbour can touch it (module docs, "Crack-free guarantee").
    ///
    /// Equal to `morph_fraction` for every configuration with
    /// `(1 - morph_fraction) * range_factor >= sqrt(2)`, which includes the default and every
    /// `range_factor >= 2` with the default band.
    #[must_use]
    pub fn effective_morph_fraction(&self) -> f32 {
        let cap = 1.0 - std::f32::consts::SQRT_2 / self.range_factor.max(2.0);
        self.morph_fraction.min(cap)
    }
}

/// World-space size of a quadtree node at `level` (level 0 = finest).
pub fn node_world_size(world_width: f32, root_level: u32, level: u32) -> f32 {
    world_width / (1u32 << (root_level - level)) as f32
}

/// Smallest root whose level-0 cell count covers `intervals` sample gaps at `grid_size`.
fn full_detail_root(intervals: u32, grid_size: u32) -> u32 {
    let grid = grid_size.max(1);
    let mut root = 0u32;
    while root < 16 && (1u32 << root).saturating_mul(grid) < intervals {
        root += 1;
    }
    root
}

/// World span of the CDLOD quadtree, metres (ADR-8291).
///
/// The lattice pitch is `world_width / (samples - 1)`. Level 0 of the full-detail root has
/// `2^root * grid_size` cells. When that count is larger than the sample intervals (a 4096-sample
/// map rounds up to 4096 cells), the span stretches so the level-0 pitch equals the lattice pitch.
/// When the cells already equal the intervals, when the root is coarser than full detail, or when
/// the root is deeper than full detail, the span is `world_width` with no extra arithmetic, so
/// those draws stay bit for bit.
///
/// `samples` is the heightfield's sample count along X (the side `world_width` measures). A
/// non-square heightfield follows that count; the quadtree nodes stay square.
#[must_use]
pub fn quadtree_span(world_width: f32, samples: u32, grid_size: u32, root_level: u32) -> f32 {
    let intervals = samples.saturating_sub(1).max(1);
    let grid = grid_size.max(1);
    let full = full_detail_root(intervals, grid);
    if root_level == full && root_level < 31 {
        let cells = (1u32 << root_level).saturating_mul(grid);
        if cells > intervals {
            return world_width * (cells as f32) / (intervals as f32);
        }
    }
    world_width
}

/// Finest cascade level whose grid cell is at most two shadow texels.
///
/// This is the rule `gpu::shadow_lists` uses to stop [`select_nodes_min_level`]. `world_width` is
/// the map width (not [`quadtree_span`]): the comparison is the historical cell size, so a cascade
/// that sat on the boundary keeps its level.
#[must_use]
pub fn cascade_min_level(world_width: f32, root_level: u32, grid_size: u32, texel_m: f32) -> u32 {
    let texel = texel_m.max(1.0e-4);
    let grid = grid_size.max(1) as f32;
    let mut level = 0u32;
    for candidate in 0..=root_level {
        let cell = node_world_size(world_width, root_level, candidate) / grid;
        if cell <= 2.0 * texel {
            level = candidate;
        }
    }
    level
}

/// One quadtree node: `x`/`z` index within its level's grid of `2^(root_level - level)` nodes.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct QuadNode {
    /// LOD level (0 = finest, `root_level` = root).
    pub level: u32,
    /// Node column index at this level.
    pub x: u32,
    /// Node row index at this level.
    pub z: u32,
}

impl QuadNode {
    /// The root node covering the whole map.
    pub fn root(root_level: u32) -> Self {
        Self {
            level: root_level,
            x: 0,
            z: 0,
        }
    }

    /// This node's four children (one level finer). Only valid when `level > 0`.
    pub fn children(self) -> [QuadNode; 4] {
        [
            QuadNode {
                level: self.level - 1,
                x: self.x * 2,
                z: self.z * 2,
            },
            QuadNode {
                level: self.level - 1,
                x: self.x * 2 + 1,
                z: self.z * 2,
            },
            QuadNode {
                level: self.level - 1,
                x: self.x * 2,
                z: self.z * 2 + 1,
            },
            QuadNode {
                level: self.level - 1,
                x: self.x * 2 + 1,
                z: self.z * 2 + 1,
            },
        ]
    }

    /// World rectangle `(min_x, min_z, size)` of the node on a map centred on the world
    /// origin. A map with a grid origin ([`TerrainMap::with_grid_origin`]) places its nodes with
    /// [`Self::map_rect`] or [`Self::world_rect_at`].
    pub fn world_rect(self, world_width: f32, root_level: u32) -> (f32, f32, f32) {
        let size = node_world_size(world_width, root_level, self.level);
        let half = world_width * 0.5;
        (-half + self.x as f32 * size, -half + self.z as f32 * size, size)
    }

    /// World rectangle `(min_x, min_z, size)` of the node on a map centred on world XZ
    /// `origin` ([`TerrainMap::grid_origin`], ADR-8179). `origin = [0, 0]` is
    /// [`Self::world_rect`], bit for bit.
    #[must_use]
    pub fn world_rect_at(self, world_width: f32, root_level: u32, origin: [f32; 2]) -> (f32, f32, f32) {
        let (min_x, min_z, size) = self.world_rect(world_width, root_level);
        (min_x + origin[0], min_z + origin[1], size)
    }

    /// World rectangle `(min_x, min_z, size)` of the node on `map` (its quadtree square,
    /// [`TerrainMap::quadtree_width`], and grid origin).
    #[must_use]
    pub fn map_rect(self, map: &TerrainMap, root_level: u32) -> (f32, f32, f32) {
        self.world_rect_at(map.quadtree_width(), root_level, map.grid_origin())
    }

    /// Whether the node's rectangle overlaps `map`'s world rectangle with a positive area.
    /// Every node of a square map does; on a rectangular map ([`TerrainMap::with_world_depth`],
    /// ADR-8369) the nodes wholly past the shorter side do not, and are never drawn.
    #[must_use]
    pub fn overlaps_map(self, map: &TerrainMap, root_level: u32) -> bool {
        let (min_x, min_z, size) = self.map_rect(map, root_level);
        let ([lo_x, lo_z], [hi_x, hi_z]) = (map.world_min(), map.world_max());
        min_x < hi_x && min_x + size > lo_x && min_z < hi_z && min_z + size > lo_z
    }

    /// World rectangle `(min_x, min_z, size)` of the node as the draw places it (ADR-8291).
    ///
    /// Equals [`Self::map_rect`] when [`quadtree_span`] is the map width. Selection, the vertex
    /// mirror, the seam report and the GPU node uniform all use this rectangle.
    #[must_use]
    pub fn draw_rect(self, map: &TerrainMap, root_level: u32, grid_size: u32) -> (f32, f32, f32) {
        let span = quadtree_span(
            map.quadtree_width(),
            map.quadtree_samples(),
            grid_size,
            root_level,
        );
        self.world_rect_at(span, root_level, map.grid_origin())
    }

    /// Horizontal distance from `camera` to the nearest point of the node's rectangle.
    pub fn rect_distance(self, world_width: f32, root_level: u32, camera: [f32; 2]) -> f32 {
        let (min_x, min_z, size) = self.world_rect(world_width, root_level);
        let dx = (min_x - camera[0]).max(camera[0] - (min_x + size)).max(0.0);
        let dz = (min_z - camera[1]).max(camera[1] - (min_z + size)).max(0.0);
        (dx * dx + dz * dz).sqrt()
    }
}

/// Per-level min/max height over the quadtree, for culling and selection bounds (the classic
/// CDLOD min/max map).
#[derive(Clone, Debug)]
pub struct MinMaxPyramid {
    root_level: u32,
    /// `levels[l]` is a `2^(root_level-l)` squared grid of `[min, max]` heights.
    levels: Vec<Vec<[f32; 2]>>,
}

impl MinMaxPyramid {
    /// Builds the pyramid by scanning the leaf nodes' sample coverage once, then merging
    /// four children per coarser level.
    ///
    /// # Errors
    ///
    /// Returns [`CdlodError::RootLevelTooDeep`] when `root_level > 16`.
    pub fn build(map: &TerrainMap, root_level: u32) -> Result<Self, CdlodError> {
        if root_level > 16 {
            return Err(CdlodError::RootLevelTooDeep(root_level));
        }
        let w = map.width();
        let h = map.height();
        let mut levels: Vec<Vec<[f32; 2]>> = Vec::with_capacity(root_level as usize + 1);

        let leaf_dim = 1usize << root_level;
        let mut leaf = vec![[0.0f32; 2]; leaf_dim * leaf_dim];
        for nz in 0..leaf_dim as u32 {
            for nx in 0..leaf_dim as u32 {
                let node = QuadNode {
                    level: 0,
                    x: nx,
                    z: nz,
                };
                leaf[nz as usize * leaf_dim + nx as usize] = node_height_range(map, node, root_level, w, h);
            }
        }
        levels.push(leaf);

        for level in 1..=root_level as usize {
            let finer_dim = 1usize << (root_level as usize - (level - 1));
            let dim = finer_dim / 2;
            let finer = &levels[level - 1];
            let mut coarse = vec![[0.0f32; 2]; dim * dim];
            for z in 0..dim {
                for x in 0..dim {
                    let a = finer[z * 2 * finer_dim + x * 2];
                    let b = finer[z * 2 * finer_dim + x * 2 + 1];
                    let c = finer[(z * 2 + 1) * finer_dim + x * 2];
                    let d = finer[(z * 2 + 1) * finer_dim + x * 2 + 1];
                    coarse[z * dim + x] = [
                        a[0].min(b[0]).min(c[0]).min(d[0]),
                        a[1].max(b[1]).max(c[1]).max(d[1]),
                    ];
                }
            }
            levels.push(coarse);
        }

        Ok(Self { root_level, levels })
    }

    /// Recomputes the nodes whose sample coverage overlaps `dirty` (after a brush stroke or a
    /// crater), every level up to the root. The result equals [`MinMaxPyramid::build`] on the
    /// edited map.
    pub fn update_region(&mut self, map: &TerrainMap, dirty: &DirtyRect) {
        if dirty.is_empty() || self.levels.is_empty() {
            return;
        }
        let (w, h) = (map.width(), map.height());
        let leaf_dim = 1u32 << self.root_level;
        // Leaf nodes covering the dirty samples, one node of margin for the shared edges.
        let [x0, z0] = map.sample_to_world(dirty.col_start, dirty.row_start);
        let [x1, z1] = map.sample_to_world(dirty.col_end.saturating_sub(1), dirty.row_end.saturating_sub(1));
        let leaf_size = node_world_size(map.quadtree_width(), self.root_level, 0);
        // Leaf nodes are counted from the quadtree square's corner (the map's −X/−Z corner on
        // a square map).
        let (min_x, min_z, _) = QuadNode::root(self.root_level).map_rect(map, self.root_level);
        let to_node = |v: f32, min: f32| ((v - min) / leaf_size).floor() as i64;
        let clamp = |v: i64| v.clamp(0, i64::from(leaf_dim) - 1) as u32;
        let (mut nx0, mut nx1) = (clamp(to_node(x0, min_x) - 1), clamp(to_node(x1, min_x) + 1));
        let (mut nz0, mut nz1) = (clamp(to_node(z0, min_z) - 1), clamp(to_node(z1, min_z) + 1));
        for nz in nz0..=nz1 {
            for nx in nx0..=nx1 {
                let node = QuadNode {
                    level: 0,
                    x: nx,
                    z: nz,
                };
                self.levels[0][nz as usize * leaf_dim as usize + nx as usize] =
                    node_height_range(map, node, self.root_level, w, h);
            }
        }
        for level in 1..=self.root_level as usize {
            let finer_dim = 1usize << (self.root_level as usize - (level - 1));
            let dim = finer_dim / 2;
            (nx0, nx1, nz0, nz1) = (nx0 / 2, nx1 / 2, nz0 / 2, nz1 / 2);
            for z in nz0 as usize..=nz1 as usize {
                for x in nx0 as usize..=nx1 as usize {
                    let finer = &self.levels[level - 1];
                    let a = finer[z * 2 * finer_dim + x * 2];
                    let b = finer[z * 2 * finer_dim + x * 2 + 1];
                    let c = finer[(z * 2 + 1) * finer_dim + x * 2];
                    let d = finer[(z * 2 + 1) * finer_dim + x * 2 + 1];
                    self.levels[level][z * dim + x] = [
                        a[0].min(b[0]).min(c[0]).min(d[0]),
                        a[1].max(b[1]).max(c[1]).max(d[1]),
                    ];
                }
            }
        }
    }

    /// The `[min, max]` height of a node's coverage.
    pub fn node_min_max(&self, node: QuadNode) -> [f32; 2] {
        let dim = (1usize << (self.root_level - node.level)).max(1);
        let idx = node.z as usize * dim + node.x as usize;
        self.levels[node.level as usize][idx.min(self.levels[node.level as usize].len() - 1)]
    }

    /// The world-space AABB of a node (heights from the pyramid) on a map centred on the
    /// world origin; [`Self::node_aabb_at`] for a map with a grid origin.
    pub fn node_aabb(&self, node: QuadNode, world_width: f32, root_level: u32) -> Aabb {
        self.node_aabb_at(node, world_width, root_level, [0.0, 0.0])
    }

    /// The world-space AABB of a node on a map centred on world XZ `origin`
    /// ([`TerrainMap::grid_origin`], ADR-8179).
    #[must_use]
    pub fn node_aabb_at(&self, node: QuadNode, world_width: f32, root_level: u32, origin: [f32; 2]) -> Aabb {
        let (min_x, min_z, size) = node.world_rect_at(world_width, root_level, origin);
        let [hmin, hmax] = self.node_min_max(node);
        Aabb::new(
            glam::Vec3::new(min_x, hmin, min_z),
            glam::Vec3::new(min_x + size, hmax, min_z + size),
        )
    }
}

/// Scans the `[min, max]` height over a node's sample coverage (inclusive, border-clamped).
fn node_height_range(map: &TerrainMap, node: QuadNode, root_level: u32, w: u32, h: u32) -> [f32; 2] {
    let (min_x, min_z, size) = node.map_rect(map, root_level);
    let [u0, v0] = map.world_to_grid(min_x, min_z);
    let [u1, v1] = map.world_to_grid(min_x + size, min_z + size);
    let c0 = (u0.floor().max(0.0) as u32).min(w - 1);
    let c1 = (u1.ceil().min((w - 1) as f32) as u32).min(w - 1).max(c0);
    let r0 = (v0.floor().max(0.0) as u32).min(h - 1);
    let r1 = (v1.ceil().min((h - 1) as f32) as u32).min(h - 1).max(r0);
    let mut min_h = f32::INFINITY;
    let mut max_h = f32::NEG_INFINITY;
    for row in r0..=r1 {
        let base = row as usize * w as usize;
        for col in c0..=c1 {
            let hv = map.heights()[base + col as usize];
            min_h = min_h.min(hv);
            max_h = max_h.max(hv);
        }
    }
    if min_h.is_finite() {
        [min_h, max_h]
    } else {
        // Degenerate empty coverage: sample the clamped corner.
        let [x, z] = map.sample_to_world(c0, r0);
        let hv = map.sample_bilinear(x, z);
        [hv, hv]
    }
}

/// One selected node plus its morph band for this frame.
#[derive(Copy, Clone, Debug)]
pub struct NodeDraw {
    /// The selected quadtree node.
    pub node: QuadNode,
    /// Distance where morphing starts (fine geometry).
    pub morph_start: f32,
    /// Distance where morphing completes (parent-grid geometry). `<= morph_start` disables
    /// morphing (root level).
    pub morph_end: f32,
}

impl NodeDraw {
    /// World rectangle `(min_x, min_z, size)` of the drawn node on a map centred on the world
    /// origin ([`QuadNode::map_rect`] for a map with a grid origin).
    pub fn world_rect(&self, world_width: f32, root_level: u32) -> (f32, f32, f32) {
        self.node.world_rect(world_width, root_level)
    }
}

/// The selected set of nodes for one frame.
#[derive(Clone, Debug, Default)]
pub struct LodSelection {
    /// Draw list (one shared grid mesh per node, per-node uniforms from these entries).
    pub nodes: Vec<NodeDraw>,
}

/// Morph factor for a vertex at `distance`, in `0..=1` (`1` = fully on the parent grid).
pub fn vertex_morph_t(morph_start: f32, morph_end: f32, distance: f32) -> f32 {
    if morph_end <= morph_start {
        return 0.0;
    }
    ((distance - morph_start) / (morph_end - morph_start)).clamp(0.0, 1.0)
}

/// Selects the CDLOD nodes for one frame.
///
/// Recursion starts at the root; a node subdivides while the camera is closer than its level's
/// range, and is frustum-culled through its height AABB when a frustum is given. The output is
/// deterministic for identical inputs.
pub fn select_nodes(
    map: &TerrainMap,
    pyramid: &MinMaxPyramid,
    config: &CdlodConfig,
    root_level: u32,
    camera_xz: [f32; 2],
    frustum: Option<&Frustum>,
) -> LodSelection {
    select_nodes_min_level(map, pyramid, config, root_level, camera_xz, frustum, 0)
}

/// [`select_nodes`] that stops subdividing at `min_level`: a node of that level is kept
/// where the full selection would split it.
///
/// Within its split range every vertex of such a node is closer than its morph band, so it
/// draws its own unmorphed grid — the same geometry as the `min_level` ancestor of the
/// finer nodes [`select_nodes`] returns there — and the rest of the selection (and its
/// morph bands) is unchanged. Shadow cascades use it to draw no cell finer than about two
/// shadow texels.
pub fn select_nodes_min_level(
    map: &TerrainMap,
    pyramid: &MinMaxPyramid,
    config: &CdlodConfig,
    root_level: u32,
    camera_xz: [f32; 2],
    frustum: Option<&Frustum>,
    min_level: u32,
) -> LodSelection {
    let mut out = LodSelection::default();
    let mut stack = Vec::new();
    fill_lod_selection(
        &LodWalk {
            map,
            pyramid,
            config,
            root_level,
            camera_xz,
            frustum,
            min_level,
        },
        &mut out,
        &mut stack,
    );
    out
}

/// Inputs of one LOD walk. The caller keeps the node list and the walk stack so a held
/// view can reuse both allocations.
pub(crate) struct LodWalk<'a> {
    /// Heightfield the nodes are measured on.
    pub map: &'a TerrainMap,
    /// Min/max pyramid used for frustum bounds.
    pub pyramid: &'a MinMaxPyramid,
    /// Split ranges and the morph band.
    pub config: &'a CdlodConfig,
    /// Coarsest quadtree level.
    pub root_level: u32,
    /// Camera XZ in world metres.
    pub camera_xz: [f32; 2],
    /// Planes that reject nodes, when the caller has a frustum.
    pub frustum: Option<&'a Frustum>,
    /// Nodes at this level are kept even when they are inside the split range.
    pub min_level: u32,
}

/// Fills `out` from a cleared node list, reusing `out.nodes` and `stack`.
/// Same selection as [`select_nodes_min_level`].
pub(crate) fn fill_lod_selection(walk: &LodWalk<'_>, out: &mut LodSelection, stack: &mut Vec<QuadNode>) {
    out.nodes.clear();
    stack.clear();
    // Level 0 of a full-detail round-up (4096 samples) uses the lattice pitch (ADR-8291).
    // Every other root keeps the quadtree width (ADR-8369), bit for bit.
    let span = quadtree_span(
        walk.map.quadtree_width(),
        walk.map.quadtree_samples(),
        walk.config.grid_size,
        walk.root_level,
    );
    let origin = walk.map.grid_origin();
    // Node distances in the map's own frame: the camera moved by the grid origin (ADR-8179).
    let camera_local = [walk.camera_xz[0] - origin[0], walk.camera_xz[1] - origin[1]];
    let morph_fraction = walk.config.effective_morph_fraction();
    stack.push(QuadNode::root(walk.root_level));
    while let Some(node) = stack.pop() {
        // A rectangular map's quadtree square overhangs its shorter side (ADR-8369): nothing
        // is drawn there. Every node of a square map overlaps it.
        if !node.overlaps_map(walk.map, walk.root_level) {
            continue;
        }
        if let Some(frustum) = walk.frustum {
            let aabb = walk.pyramid.node_aabb_at(node, span, walk.root_level, origin);
            if !frustum.intersects_aabb(&aabb) {
                continue;
            }
        }
        let size = node_world_size(span, walk.root_level, node.level);
        let range = walk.config.range_factor * size;
        let distance = node.rect_distance(span, walk.root_level, camera_local);
        if node.level > walk.min_level && distance < range {
            stack.extend(node.children());
        } else {
            let (morph_start, morph_end) = if node.level < walk.root_level {
                let coarser =
                    walk.config.range_factor * node_world_size(span, walk.root_level, node.level + 1);
                let band = coarser - range;
                (range + (1.0 - morph_fraction) * band, coarser)
            } else {
                (0.0, 0.0)
            };
            out.nodes.push(NodeDraw {
                node,
                morph_start,
                morph_end,
            });
        }
    }
}

/// CPU evaluation of one grid vertex of a node — the exact math the WGSL vertex shader applies.
///
/// Grid coordinate `(gx, gz)` in `0..=grid_size` is placed in the node's rectangle, morphed
/// toward the parent grid by `t` (see [`vertex_morph_t`]), and its height is the bilinear
/// sample of the heightfield at the resulting world XZ.
pub fn eval_node_vertex(
    map: &TerrainMap,
    config: &CdlodConfig,
    node: QuadNode,
    root_level: u32,
    gx: u32,
    gz: u32,
    t: f32,
) -> [f32; 3] {
    let (min_x, min_z, size) = node.draw_rect(map, root_level, config.grid_size);
    let step = size / config.grid_size as f32;
    let coarse_x = gx.div_ceil(2) * 2;
    let coarse_z = gz.div_ceil(2) * 2;
    let mx = gx as f32 + (coarse_x as f32 - gx as f32) * t;
    let mz = gz as f32 + (coarse_z as f32 - gz as f32) * t;
    let [x, z] = map.clamp_to_rect(min_x + mx * step, min_z + mz * step);
    [x, map.sample_bilinear(x, z), z]
}

/// CPU mirror of the WGSL vertex shader: [`eval_node_vertex`]'s morphed grid position with
/// the height read from the prefiltered pyramid at [`crate::height_mips::vertex_mip`].
///
/// At cells no larger than a heightfield texel the level is 0 and this equals
/// [`eval_node_vertex`]; coarser cells read the box-filtered surface, so features narrower
/// than the cell are averaged instead of aliased into a saw-tooth.
#[allow(clippy::too_many_arguments)] // mirrors the shader's inputs one for one
pub fn eval_node_vertex_filtered(
    map: &TerrainMap,
    mips: &crate::height_mips::HeightMips,
    config: &CdlodConfig,
    node: QuadNode,
    root_level: u32,
    gx: u32,
    gz: u32,
    t: f32,
) -> [f32; 3] {
    // Same morphed position as `eval_node_vertex`, without its (unused) bilinear height.
    let (min_x, min_z, size) = node.draw_rect(map, root_level, config.grid_size);
    let step = size / config.grid_size as f32;
    let coarse_x = gx.div_ceil(2) * 2;
    let coarse_z = gz.div_ceil(2) * 2;
    let mx = gx as f32 + (coarse_x as f32 - gx as f32) * t;
    let mz = gz as f32 + (coarse_z as f32 - gz as f32) * t;
    let [x, z] = map.clamp_to_rect(min_x + mx * step, min_z + mz * step);
    let cell = size / config.grid_size as f32;
    let mip = crate::height_mips::vertex_mip(cell, map.sample_spacing(), t);
    [x, mips.sample_world(map, x, z, mip), z]
}

/// Height of the fully-detailed rendered surface (finest uniform grid of `grid_step` metres,
/// same cell diagonal as [`regia_asset::mesh_from_heightfield`]) at world XZ.
///
/// This is what the renderer's triangles interpolate. On a smooth heightfield it stays
/// within 1 cm of [`TerrainMap::sample_bilinear`] (spec 26 § 1: the 513² map over 256 m).
/// A non-coplanar cell can leave that surface by up to a quarter of the corner span. The
/// centre of a 2×2 cell whose corners are 0, 0, 0 and 1 m is height 0 on the triangle and
/// 0.25 m on the bilinear sample (ADR-5083). The renderer stays triangulated.
pub fn terrain_surface_height(map: &TerrainMap, grid_step: f32, x: f32, z: f32) -> f32 {
    // The cell grid in the map's own frame (centred on its grid origin, ADR-8179), counted
    // from the quadtree square's corner; a rectangular map's cells end at its shorter side's
    // edge, where the draw clamps its vertices (ADR-8369). A square map: one `half` for all.
    let half = map.quadtree_width() * 0.5;
    let (half_x, half_z) = (map.world_width() * 0.5, map.world_depth() * 0.5);
    let [ox, oz] = map.grid_origin();
    let x = (x - ox).clamp(-half_x, half_x);
    let z = (z - oz).clamp(-half_z, half_z);
    let fx = (x + half) / grid_step;
    let fz = (z + half) / grid_step;
    let cx = fx.floor() * grid_step - half;
    let cz = fz.floor() * grid_step - half;
    let x0 = cx.max(-half_x);
    let z0 = cz.max(-half_z);
    let x1 = (cx + grid_step).min(half_x);
    let z1 = (cz + grid_step).min(half_z);
    let h00 = map.sample_bilinear(x0 + ox, z0 + oz);
    let h10 = map.sample_bilinear(x1 + ox, z0 + oz);
    let h01 = map.sample_bilinear(x0 + ox, z1 + oz);
    let h11 = map.sample_bilinear(x1 + ox, z1 + oz);
    let u = if x1 > x0 { (x - x0) / (x1 - x0) } else { 0.0 };
    let v = if z1 > z0 { (z - z0) / (z1 - z0) } else { 0.0 };
    // Cell diagonal from (x0, z1) to (x1, z0), matching the cooked heightfield mesh split
    // [v00, v01, v10, v10, v01, v11].
    if u + v <= 1.0 {
        h00 + (h01 - h00) * v + (h10 - h00) * u
    } else {
        h11 + (h01 - h11) * (1.0 - u) + (h10 - h11) * (1.0 - v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Rng;

    /// ADR-8291: a 4096-sample full-detail root draws level 0 at the lattice pitch. Maps that
    /// already nest, coarse roots and roots deeper than full detail keep the map width bit for bit.
    #[test]
    fn quadtree_span_matches_the_lattice_only_on_the_round_up() {
        let w = 1280.0f32;
        let span = quadtree_span(w, 4096, 16, 8);
        let pitch = node_world_size(span, 8, 0) / 16.0;
        let lattice = w / 4095.0;
        assert!(
            (pitch - lattice).abs() <= 1e-5,
            "level-0 pitch {pitch} m, lattice pitch {lattice} m"
        );
        assert_eq!(quadtree_span(w, 1025, 16, 6).to_bits(), w.to_bits());
        assert_eq!(quadtree_span(256.0, 513, 16, 3).to_bits(), 256.0f32.to_bits());
        assert_eq!(quadtree_span(256.0, 257, 16, 5).to_bits(), 256.0f32.to_bits());
        assert_eq!(quadtree_span(256.0, 513, 32, 4).to_bits(), 256.0f32.to_bits());
    }

    /// Smooth analytic island-style heightfield, 513x513 samples over 256 m (0.5 m spacing):
    /// curvature keeps the triangle-vs-bilinear error well under 1 cm.
    fn smooth_map() -> TerrainMap {
        let n = 513u32;
        let width = 256.0f32;
        let mut heights = Vec::with_capacity((n * n) as usize);
        for row in 0..n {
            for col in 0..n {
                let [x, z] = {
                    let half = width * 0.5;
                    [
                        col as f32 / (n - 1) as f32 * width - half,
                        row as f32 / (n - 1) as f32 * width - half,
                    ]
                };
                let h = 8.0 * (x * 0.0349).cos() * (z * 0.0419).sin()
                    + 5.0 * (x * 0.0676 + 1.3).sin() * (z * 0.0519).sin()
                    + 3.0 * ((x + z) * 0.0940 + 0.4).sin();
                heights.push(h);
            }
        }
        TerrainMap::from_heights(n, n, heights, width, 1.0, 0.0).expect("valid map")
    }

    #[test]
    fn cdlod_root_level_17_is_rejected() {
        let map = TerrainMap::from_heights(2, 2, vec![0.0; 4], 8.0, 1.0, 0.0).expect("valid map");
        assert!(matches!(
            MinMaxPyramid::build(&map, 17),
            Err(CdlodError::RootLevelTooDeep(17))
        ));
    }

    fn setup(map: &TerrainMap) -> (CdlodConfig, u32, MinMaxPyramid) {
        // Leaf node size 16 m, grid 32 -> 0.5 m leaf grid step == sample spacing.
        let config = CdlodConfig::with_grid_size(32);
        let root_level = 4u32; // 256 / 2^4 = 16 m leaves
        let pyramid = MinMaxPyramid::build(map, root_level).expect("pyramid");
        (config, root_level, pyramid)
    }

    #[test]
    fn config_validation_rejects_bad_values() {
        assert!(CdlodConfig::default().validate().is_ok());
        assert!(matches!(
            CdlodConfig {
                grid_size: 12,
                ..Default::default()
            }
            .validate(),
            Err(CdlodError::BadGridSize(_))
        ));
        assert!(matches!(
            CdlodConfig {
                range_factor: 1.5,
                ..Default::default()
            }
            .validate(),
            Err(CdlodError::BadRangeFactor(_))
        ));
        assert!(matches!(
            CdlodConfig {
                morph_fraction: 0.75,
                ..Default::default()
            }
            .validate(),
            Err(CdlodError::BadMorphFraction(_))
        ));
    }

    #[test]
    fn selection_partitions_the_map_exactly_once() {
        let map = smooth_map();
        let (config, root_level, pyramid) = setup(&map);
        let selection = select_nodes(&map, &pyramid, &config, root_level, [0.0, 0.0], None);
        assert!(!selection.nodes.is_empty());

        // Every point of the world square lies in exactly one selected node rectangle.
        let half = map.world_width() * 0.5;
        let mut rng = Rng::new(11);
        for _ in 0..2000 {
            let x = rng.range(-half..half);
            let z = rng.range(-half..half);
            let covers = selection
                .nodes
                .iter()
                .filter(|n| {
                    let (min_x, min_z, size) = n.world_rect(map.world_width(), root_level);
                    x >= min_x && x <= min_x + size && z >= min_z && z <= min_z + size
                })
                .count();
            assert_eq!(covers, 1, "point ({x}, {z}) covered {covers} times");
        }
    }

    #[test]
    fn adjacent_selected_levels_differ_by_at_most_one() {
        let map = smooth_map();
        let (config, root_level, pyramid) = setup(&map);
        let mut rng = Rng::new(42);
        for case in 0..60 {
            let camera = [rng.range(-600.0..600.0), rng.range(-600.0..600.0)];
            let selection = select_nodes(&map, &pyramid, &config, root_level, camera, None);
            let eps = 1e-3;
            for (i, a) in selection.nodes.iter().enumerate() {
                for b in selection.nodes.iter().skip(i + 1) {
                    let (ax0, az0, asz) = a.world_rect(map.world_width(), root_level);
                    let (bx0, bz0, bsz) = b.world_rect(map.world_width(), root_level);
                    // Shared vertical edge?
                    let v_edge = ((ax0 + asz - bx0).abs() < eps || (bx0 + bsz - ax0).abs() < eps)
                        && az0 < bz0 + bsz - eps
                        && bz0 < az0 + asz - eps;
                    // Shared horizontal edge?
                    let h_edge = ((az0 + asz - bz0).abs() < eps || (bz0 + bsz - az0).abs() < eps)
                        && ax0 < bx0 + bsz - eps
                        && bx0 < ax0 + asz - eps;
                    if v_edge || h_edge {
                        assert!(
                            a.node.level.abs_diff(b.node.level) <= 1,
                            "case {case}: adjacent nodes at levels {} and {} (camera {camera:?})",
                            a.node.level,
                            b.node.level
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn minimum_range_factor_still_avoids_two_level_jumps() {
        let map = smooth_map();
        let (mut config, root_level, pyramid) = setup(&map);
        config.range_factor = 2.0; // enforced minimum
        let mut rng = Rng::new(7);
        for _ in 0..40 {
            let camera = [rng.range(-400.0..400.0), rng.range(-400.0..400.0)];
            let selection = select_nodes(&map, &pyramid, &config, root_level, camera, None);
            for (i, a) in selection.nodes.iter().enumerate() {
                for b in selection.nodes.iter().skip(i + 1) {
                    let (ax0, az0, asz) = a.world_rect(map.world_width(), root_level);
                    let (bx0, bz0, bsz) = b.world_rect(map.world_width(), root_level);
                    let touching = az0 < bz0 + bsz && bz0 < az0 + asz && ax0 < bx0 + bsz && bx0 < ax0 + asz;
                    let close = (ax0 - bx0).abs() < 1e-3
                        || (ax0 + asz - bx0).abs() < 1e-3
                        || (bx0 + bsz - ax0).abs() < 1e-3
                        || (az0 + asz - bz0).abs() < 1e-3
                        || (bz0 + bsz - az0).abs() < 1e-3;
                    if touching && close {
                        assert!(
                            a.node.level.abs_diff(b.node.level) <= 1,
                            "levels {} vs {}",
                            a.node.level,
                            b.node.level
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn fully_morphed_fine_edge_matches_coarse_grid_exactly() {
        let map = smooth_map();
        let (config, root_level, pyramid) = setup(&map);
        let selection = select_nodes(&map, &pyramid, &config, root_level, [24.0, -8.0], None);
        // For every fine node (level l) find the covering coarse node at l+1: at morph t = 1
        // every even-grid vertex of the fine node must equal the coarse node's vertex.
        for fine in &selection.nodes {
            let l = fine.node.level;
            if l >= root_level {
                continue;
            }
            let coarse = QuadNode {
                level: l + 1,
                x: fine.node.x / 2,
                z: fine.node.z / 2,
            };
            let g = config.grid_size;
            // The fine node is one quadrant of the coarse node: its grid (gx, gz) maps to the
            // coarse node's grid (gx + (x % 2) * G, gz + (z % 2) * G).
            for gz in (0..=g).step_by(2) {
                for gx in (0..=g).step_by(2) {
                    let vf = eval_node_vertex(&map, &config, fine.node, root_level, gx, gz, 1.0);
                    // Even fine-grid coordinate (gx, gz) coincides with the coarse node's
                    // grid point (gx/2 + (x%2) * G/2, gz/2 + (z%2) * G/2): the coarse node
                    // spans twice the area with the same grid resolution.
                    let coarse_gx = gx / 2 + (fine.node.x % 2) * (g / 2);
                    let coarse_gz = gz / 2 + (fine.node.z % 2) * (g / 2);
                    let vc = eval_node_vertex(&map, &config, coarse, root_level, coarse_gx, coarse_gz, 0.0);
                    for k in 0..3 {
                        assert!(
                            (vf[k] - vc[k]).abs() < 1e-5,
                            "level {l} node ({},{}) grid ({gx},{gz}): {vf:?} vs {vc:?}",
                            fine.node.x,
                            fine.node.z
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn morph_moves_vertices_monotonically_onto_the_coarse_position() {
        let map = smooth_map();
        let (config, root_level, _) = setup(&map);
        let fine = QuadNode { level: 2, x: 3, z: 1 };
        let coarse = QuadNode { level: 3, x: 1, z: 0 };
        let start = eval_node_vertex(&map, &config, fine, root_level, 5, 3, 0.0);
        // Odd grid coordinate (5, 3) morphs onto the even coordinate (6, 4), which is the
        // coarse node's grid point (6/2 + 16, 4/2 + 16) for quadrant (1, 1) with G = 32.
        let target = eval_node_vertex(&map, &config, coarse, root_level, 6 / 2 + 16, 4 / 2 + 16, 0.0);
        let dir_x = (target[0] - start[0]).signum();
        let dir_z = (target[2] - start[2]).signum();
        let mut last = start;
        let mut steps = 0;
        for step in 1..=10 {
            let t = step as f32 / 10.0;
            let v = eval_node_vertex(&map, &config, fine, root_level, 5, 3, t);
            assert!((v[0] - last[0]) * dir_x >= -1e-6, "x must not move backwards");
            assert!((v[2] - last[2]) * dir_z >= -1e-6, "z must not move backwards");
            last = v;
            steps += 1;
        }
        assert_eq!(steps, 10);
        let done = eval_node_vertex(&map, &config, fine, root_level, 5, 3, 1.0);
        assert!((done[0] - target[0]).abs() < 1e-5);
        assert!((done[2] - target[2]).abs() < 1e-5);
    }

    /// Spec 26 § 1: the base rendered surface must match the bilinear sampler within 1 cm.
    #[test]
    fn base_surface_within_1cm_of_bilinear_sampler() {
        let map = smooth_map();
        let (_config, _root_level, _pyramid) = setup(&map);
        let step = map.sample_spacing(); // finest rendered grid == sample spacing
        let half = map.world_width() * 0.5;
        let mut rng = Rng::new(2026);
        let mut max_err = 0.0f32;
        for _ in 0..10_000 {
            let x = rng.range(-half..half);
            let z = rng.range(-half..half);
            let rendered = terrain_surface_height(&map, step, x, z);
            let sampled = map.sample_bilinear(x, z);
            let err = (rendered - sampled).abs();
            max_err = max_err.max(err);
            assert!(
                err <= 0.01,
                "sim/render mismatch {err:.4} m at ({x:.2}, {z:.2}); max so far {max_err:.4}"
            );
        }
        // Sanity: the field actually varies (the test is not measuring a flat map).
        assert!(max_err > 1e-4, "field too flat to exercise the comparison");
    }

    /// Spec 26 § 1's 1 cm bound is the smooth map. A non-coplanar cell's triangle leaves
    /// the bilinear surface (ADR-5083). The renderer stays triangulated.
    #[test]
    fn a_non_coplanar_cell_leaves_the_bilinear_surface() {
        let map = TerrainMap::from_heights(2, 2, vec![0.0, 0.0, 0.0, 1.0], 1.0, 1.0, 0.0).expect("map");
        let rendered = terrain_surface_height(&map, map.sample_spacing(), 0.0, 0.0);
        let sampled = map.sample_bilinear(0.0, 0.0);
        assert!((rendered - 0.0).abs() < 1e-5, "triangle {rendered}");
        assert!((sampled - 0.25).abs() < 1e-5, "bilinear {sampled}");
        assert!((rendered - sampled).abs() > 0.2);
    }

    #[test]
    fn far_camera_selects_the_single_root_node() {
        let map = smooth_map();
        let (config, root_level, pyramid) = setup(&map);
        let selection = select_nodes(&map, &pyramid, &config, root_level, [50_000.0, 50_000.0], None);
        assert_eq!(selection.nodes.len(), 1);
        assert_eq!(selection.nodes[0].node.level, root_level);
        assert_eq!(selection.nodes[0].morph_start, 0.0);
    }

    #[test]
    fn frustum_culling_drops_nodes_behind_the_camera() {
        let map = smooth_map();
        let (config, root_level, pyramid) = setup(&map);
        let view = regia_core::glam::camera::rh::view::look_at_mat4(
            regia_core::glam::Vec3::new(0.0, 50.0, 120.0),
            regia_core::glam::Vec3::ZERO,
            regia_core::glam::Vec3::Y,
        );
        let proj = regia_core::glam::camera::rh::proj::directx::perspective(1.2f32, 1.0, 0.5, 400.0);
        let frustum = Frustum::from_view_projection(&(proj * view));
        let culled = select_nodes(&map, &pyramid, &config, root_level, [0.0, 120.0], Some(&frustum));
        let all = select_nodes(&map, &pyramid, &config, root_level, [0.0, 120.0], None);
        assert!(culled.nodes.len() < all.nodes.len());
        assert!(!culled.nodes.is_empty());
    }

    #[test]
    fn minmax_pyramid_matches_bruteforce_ranges() {
        let map = smooth_map();
        let (_config, root_level, pyramid) = setup(&map);
        let mut rng = Rng::new(99);
        for _ in 0..40 {
            let level = rng.range(0..=root_level);
            let dim = 1u32 << (root_level - level);
            let node = QuadNode {
                level,
                x: rng.range(0..dim),
                z: rng.range(0..dim),
            };
            let [mn, mx] = pyramid.node_min_max(node);
            let brute = node_height_range(&map, node, root_level, map.width(), map.height());
            assert_eq!(mn, brute[0]);
            assert_eq!(mx, brute[1]);
        }
        // Coarser levels bound finer ones.
        let root = pyramid.node_min_max(QuadNode::root(root_level));
        let (gmin, gmax) = map.min_max_height();
        assert_eq!(root, [gmin, gmax]);
    }
    /// `update_region` equals a rebuild after edits of any size anywhere, borders and corners
    /// included (the incremental path the brush and craters take).
    #[test]
    fn minmax_update_region_matches_a_rebuild_for_random_edits() {
        let mut map = smooth_map();
        let (_config, root_level, mut pyramid) = setup(&map);
        let (w, h) = (map.width(), map.height());
        let mut rng = Rng::new(1234);
        for case in 0..60 {
            // Small rects, single samples, whole edges and corners.
            let (c0, r0, cw, rh) = match case % 4 {
                0 => (
                    rng.range(0..w),
                    rng.range(0..h),
                    rng.range(1..24u32),
                    rng.range(1..24u32),
                ),
                1 => (rng.range(0..w), rng.range(0..h), 1, 1),
                2 => (0, rng.range(0..h), w, rng.range(1..4u32)),
                _ => (w - rng.range(1..6u32), h - rng.range(1..6u32), 8, 8),
            };
            let rect = DirtyRect {
                col_start: c0,
                col_end: (c0 + cw).min(w),
                row_start: r0,
                row_end: (r0 + rh).min(h),
            };
            let delta = rng.range(-30.0..30.0f32);
            for row in rect.row_start..rect.row_end {
                for col in rect.col_start..rect.col_end {
                    let i = row as usize * w as usize + col as usize;
                    map.set_height(i, map.height_at_index(i) + delta);
                }
            }
            pyramid.update_region(&map, &rect);
            let rebuilt = MinMaxPyramid::build(&map, root_level).expect("pyramid");
            assert_eq!(pyramid.levels, rebuilt.levels, "case {case}: rect {rect:?}");
        }
    }

    /// A straight 20 m cliff, 0.6 m thick, running at 37° to the grid over a 0.3125 m
    /// heightfield (the DO27 visual spacing): the shape of the trench walls in the parity
    /// plates.
    fn diagonal_wall_map() -> (TerrainMap, [f32; 2], [f32; 2]) {
        let n = 513u32;
        let width = 160.0f32;
        let angle = 37.0f32.to_radians();
        let normal = [angle.cos(), angle.sin()];
        let tangent = [-normal[1], normal[0]];
        let mut heights = Vec::with_capacity((n * n) as usize);
        for row in 0..n {
            for col in 0..n {
                let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
                let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
                let d = x * normal[0] + z * normal[1] - 3.1;
                heights.push(20.0 * (0.5 + d / 0.6).clamp(0.0, 1.0));
            }
        }
        let map = TerrainMap::from_heights(n, n, heights, width, 1.0, 0.0).expect("valid map");
        (map, normal, tangent)
    }

    /// Height of the rendered (unmorphed) grid of the level-`level` node covering `(x, z)`,
    /// bilinear between its four grid corners; `vertex` evaluates one grid vertex.
    fn rendered_height(
        map: &TerrainMap,
        config: &CdlodConfig,
        root_level: u32,
        level: u32,
        x: f32,
        z: f32,
        vertex: &dyn Fn(QuadNode, u32, u32) -> [f32; 3],
    ) -> f32 {
        let size = node_world_size(map.world_width(), root_level, level);
        let half = map.world_width() * 0.5;
        let node = QuadNode {
            level,
            x: ((x + half) / size) as u32,
            z: ((z + half) / size) as u32,
        };
        let (min_x, min_z, _) = node.world_rect(map.world_width(), root_level);
        let cell = size / config.grid_size as f32;
        let fx = (x - min_x) / cell;
        let fz = (z - min_z) / cell;
        let gx = (fx.floor() as u32).min(config.grid_size - 1);
        let gz = (fz.floor() as u32).min(config.grid_size - 1);
        let (u, v) = (fx - gx as f32, fz - gz as f32);
        let h = |a: u32, b: u32| vertex(node, a, b)[1];
        let top = h(gx, gz) + (h(gx + 1, gz) - h(gx, gz)) * u;
        let bottom = h(gx, gz + 1) + (h(gx + 1, gz + 1) - h(gx, gz + 1)) * u;
        top + (bottom - top) * v
    }

    /// Standard deviation of the rendered height along a line through the middle of the
    /// wall, parallel to it, as a fraction of the wall height. A straight wall is constant
    /// there; point-sampled coarse vertices alternate between its top and its foot.
    fn wall_ripple(level: u32, vertex: &dyn Fn(&TerrainMap, QuadNode, u32, u32) -> [f32; 3]) -> f32 {
        let (map, normal, tangent) = diagonal_wall_map();
        let config = CdlodConfig::with_grid_size(16);
        let root_level = 3; // 20 m leaves: level 0 cell 1.25 m, level 2 cell 5 m
        let mut samples = Vec::new();
        let mut s = -55.0f32;
        while s <= 55.0 {
            let x = normal[0] * 3.1 + tangent[0] * s;
            let z = normal[1] * 3.1 + tangent[1] * s;
            let f = |node: QuadNode, gx: u32, gz: u32| vertex(&map, node, gx, gz);
            samples.push(rendered_height(&map, &config, root_level, level, x, z, &f));
            s += 0.05;
        }
        let mean = samples.iter().sum::<f32>() / samples.len() as f32;
        let var = samples.iter().map(|h| (h - mean).powi(2)).sum::<f32>() / samples.len() as f32;
        var.sqrt() / 20.0
    }

    #[test]
    fn coarse_vertices_point_sampling_a_diagonal_wall_saw_tooth() {
        // The pre-pyramid vertex shader (exact bilinear height at every grid vertex): at a
        // 5 m cell the wall line swings by about a seventh of the wall height (rms) — the stair-step
        // terraces of the parity plates. Kept as the reference the filtered path is held to.
        let config = CdlodConfig::with_grid_size(16);
        let ripple = wall_ripple(2, &|map, node, gx, gz| {
            eval_node_vertex(map, &config, node, 3, gx, gz, 0.0)
        });
        assert!(ripple > 0.1, "unfiltered ripple {ripple}");
    }

    #[test]
    fn filtered_coarse_vertices_keep_a_diagonal_wall_straight() {
        let config = CdlodConfig::with_grid_size(16);
        let (map, _, _) = diagonal_wall_map();
        let mips = crate::height_mips::HeightMips::build(&map);
        for level in 1..=2 {
            let raw = wall_ripple(level, &|map, node, gx, gz| {
                eval_node_vertex(map, &config, node, 3, gx, gz, 0.0)
            });
            let filtered = wall_ripple(level, &|map, node, gx, gz| {
                eval_node_vertex_filtered(map, &mips, &config, node, 3, gx, gz, 0.0)
            });
            eprintln!("level {level}: ripple raw {raw:.4} filtered {filtered:.4}");
            assert!(
                filtered < 0.04 && filtered < raw * 0.3,
                "level {level}: filtered ripple {filtered} vs unfiltered {raw}"
            );
        }
    }

    #[test]
    fn filtered_vertices_are_exact_at_the_finest_cells_and_crack_free() {
        let map = smooth_map();
        let (config, root_level, pyramid) = setup(&map);
        let mips = crate::height_mips::HeightMips::build(&map);
        // Leaf cells equal the sample spacing: level 0 of the pyramid, the exact map.
        let leaf = QuadNode { level: 0, x: 5, z: 9 };
        for (gx, gz) in [(0, 0), (7, 3), (32, 32)] {
            let a = eval_node_vertex(&map, &config, leaf, root_level, gx, gz, 0.0);
            let b = eval_node_vertex_filtered(&map, &mips, &config, leaf, root_level, gx, gz, 0.0);
            assert!((a[1] - b[1]).abs() < 1e-5);
        }
        // A fully morphed fine vertex equals its coarse neighbour's unmorphed vertex.
        let selection = select_nodes(&map, &pyramid, &config, root_level, [24.0, -8.0], None);
        let g = config.grid_size;
        for fine in &selection.nodes {
            let l = fine.node.level;
            if l >= root_level {
                continue;
            }
            let coarse = QuadNode {
                level: l + 1,
                x: fine.node.x / 2,
                z: fine.node.z / 2,
            };
            for gz in (0..=g).step_by(4) {
                for gx in (0..=g).step_by(4) {
                    let vf =
                        eval_node_vertex_filtered(&map, &mips, &config, fine.node, root_level, gx, gz, 1.0);
                    let cgx = gx / 2 + (fine.node.x % 2) * (g / 2);
                    let cgz = gz / 2 + (fine.node.z % 2) * (g / 2);
                    let vc =
                        eval_node_vertex_filtered(&map, &mips, &config, coarse, root_level, cgx, cgz, 0.0);
                    assert!((vf[1] - vc[1]).abs() < 1e-4, "level {l}: {vf:?} vs {vc:?}");
                }
            }
        }
    }
}
