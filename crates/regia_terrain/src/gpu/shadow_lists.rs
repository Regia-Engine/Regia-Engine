// SPDX-License-Identifier: MPL-2.0
//! Terrain node lists for the sun cascades of the shared frame.
//!
//! A cascade texel is far coarser than the CDLOD cells near the eye, so each cascade
//! draws its own selection, [`select_nodes_min_level`] stopped at the level whose grid
//! cell is at most two shadow texels. That selection is made from an *anchored* eye: the
//! eye of the last reselection, kept until the camera moves more than one node of that
//! level away (and morphed around the same eye in `vs_shadow`, so the cascade's mesh is
//! crack-free). The list is then culled to the cascade box once per cascade placement.
//! A moving camera therefore changes a cascade's list (and its
//! [`regia_render::SharedPassDraw::cascade_shadow_key`]) only when it leaves that
//! tolerance or the cascade is placed anew; before, every LOD split under the camera
//! changed one shared key and redrew all four cascades.
//!
//! A runtime height edit (ADR-7585) invalidates only the cascades it can reach: each applied
//! band's world box ([`EditLog`]) is tested against the cascade's selected nodes, each grown by
//! its own reach (cell, morph and the pyramid level its vertices read; ADR-7638), and a cascade
//! reselects (and changes its key) only when the part of a node the edit can move lands inside
//! it. Whole-map changes (holes, a host's `update_heights`) still bump the shared
//! generation and reselect every cascade.

use std::collections::VecDeque;

use crate::cdlod::{CdlodConfig, LodSelection, MinMaxPyramid, node_world_size, select_nodes_min_level};
use crate::sampler::TerrainMap;

/// World AABB of a node, `(min, max)`.
pub(super) type Bounds = ([f32; 3], [f32; 3]);

/// Most edit boxes [`EditLog`] keeps; a cascade (or a texture view's cascade) that has not
/// looked at the log for longer reselects as for a whole-map change.
pub(super) const EDIT_LOG_CAP: usize = 256;

/// World boxes of the runtime height bands applied so far (ADR-7585), newest last, each with
/// its serial number. Every cascade of every view remembers the last serial it tested.
#[derive(Debug, Default)]
pub(super) struct EditLog {
    /// Serial of the newest box (0 = none yet).
    pub serial: u64,
    boxes: VecDeque<(u64, Bounds)>,
}

impl EditLog {
    /// Records the world box (old and new heights included) of one applied band.
    pub fn push(&mut self, bounds: Bounds) {
        self.serial += 1;
        self.boxes.push_back((self.serial, bounds));
        while self.boxes.len() > EDIT_LOG_CAP {
            self.boxes.pop_front();
        }
    }

    /// The boxes newer than `seen`, or `None` when some of them were dropped already.
    pub fn since(&self, seen: u64) -> Option<impl Iterator<Item = &Bounds>> {
        let oldest = self.boxes.front().map_or(self.serial + 1, |(serial, _)| *serial);
        (seen + 1 >= oldest || seen >= self.serial).then(|| {
            self.boxes
                .iter()
                .filter(move |(serial, _)| *serial > seen)
                .map(|(_, b)| b)
        })
    }
}

/// What every cascade's reselection needs.
pub(super) struct ShadowInputs<'a> {
    pub map: &'a TerrainMap,
    pub pyramid: &'a MinMaxPyramid,
    pub config: &'a CdlodConfig,
    pub root_level: u32,
    /// Bumped by whole-map changes only (holes, a host's heights): every cascade reselects.
    pub heights_generation: u64,
    /// The runtime height bands applied so far: a cascade reselects for the ones in its box.
    pub edits: &'a EditLog,
}

/// One cascade's anchored selection and culled list.
#[derive(Default)]
pub(super) struct CascadeShadow {
    /// Eye (x, z) the selection was made from; `None` = inactive.
    eye: Option<[f32; 2]>,
    level: u32,
    generation: u64,
    /// The anchored selection; node uniform slots `base..base + len`.
    pub selection: LodSelection,
    bounds: Vec<Bounds>,
    /// Cascade view-projection the list was culled with.
    stamp: Option<[[f32; 4]; 4]>,
    /// Indices into `selection` inside the cascade box.
    pub list: Vec<u32>,
    /// Shadow-map UV footprint (`[u0, v0, u1, v1]`) of each `list` entry.
    rects: Vec<[f32; 4]>,
    /// Identity of the list: nodes, morph bands, eye, height generation, edits that hit it.
    pub key: u64,
    /// Newest [`EditLog`] serial this cascade has tested.
    edits_seen: u64,
    /// Edit boxes that landed in this cascade so far (each one reselected and re-keyed it).
    pub edit_hits: u64,
}

impl CascadeShadow {
    /// The anchored eye (the cascade's morph centre), when active.
    pub fn eye(&self) -> Option<[f32; 2]> {
        self.eye
    }

    /// Updates the cascade for this frame. Returns `true` when the selection changed
    /// (node uniform slots must be rebuilt).
    pub fn update(
        &mut self,
        inputs: &ShadowInputs<'_>,
        eye: [f32; 2],
        texel: f32,
        vp: &[[f32; 4]; 4],
    ) -> bool {
        let world = inputs.map.quadtree_width();
        // Shadow passes draw the decimated grid (`SHADOW_GRID_SIZE` cells per node).
        let grid = inputs.config.grid_size.clamp(1, super::SHADOW_GRID_SIZE);
        let level = shadow_level(world, inputs.root_level, grid, texel);
        let tolerance = node_world_size(world, inputs.root_level, level);
        // Each selected node is tested with its own reach: the coarse nodes away from the eye
        // read further than the cascade level's cell (`edit_moves_inside`).
        let edit_hit = self.eye.is_some()
            && inputs.edits.serial != self.edits_seen
            && inputs.edits.since(self.edits_seen).is_none_or(|mut boxes| {
                boxes.any(|edit| self.edit_moves_inside(inputs, edit, vp, tolerance))
            });
        self.edits_seen = inputs.edits.serial;
        if edit_hit {
            self.edit_hits += 1;
        }
        let reselect = match self.eye {
            None => true,
            Some(anchor) => {
                let (dx, dz) = (anchor[0] - eye[0], anchor[1] - eye[1]);
                level != self.level
                    || self.generation != inputs.heights_generation
                    || edit_hit
                    || dx * dx + dz * dz > tolerance * tolerance
            }
        };
        if reselect {
            let _span = tracing::info_span!("terrain_shadow_select").entered();
            self.eye = Some(eye);
            self.level = level;
            self.generation = inputs.heights_generation;
            self.selection = select_nodes_min_level(
                inputs.map,
                inputs.pyramid,
                inputs.config,
                inputs.root_level,
                eye,
                None,
                level,
            );
            self.bounds = self
                .selection
                .nodes
                .iter()
                .map(|draw| {
                    // The drawn rectangle (ADR-8291): the shadow bounds match the node the raster draws.
                    let (min_x, min_z, size) =
                        draw.node
                            .draw_rect(inputs.map, inputs.root_level, inputs.config.grid_size);
                    let [lo, hi] = inputs.pyramid.node_min_max(draw.node);
                    // Cliff relief moves steep-face vertices up to `CLIFF_RELIEF_MAX_M`.
                    let pad = super::CLIFF_RELIEF_MAX_M;
                    (
                        [min_x - pad, lo - pad, min_z - pad],
                        [min_x + size + pad, hi + pad, min_z + size + pad],
                    )
                })
                .collect();
            self.stamp = None;
        }
        if self.stamp != Some(*vp) {
            self.list.clear();
            self.rects.clear();
            for (i, (lo, hi)) in self.bounds.iter().enumerate() {
                if let Some(rect) = footprint(*lo, *hi, vp) {
                    self.list.push(i as u32);
                    self.rects.push(rect);
                }
            }
            self.key = self.list_key();
            self.stamp = Some(*vp);
        }
        reselect
    }

    /// Whether the edit box `(lo, hi)` (changed samples, old and new heights) can move a drawn
    /// point of this cascade's anchored selection inside the cascade box `vp`. `camera_slack`:
    /// how far the camera may be from the anchored eye (the reselection tolerance).
    ///
    /// Each selected node is tested with its own reach ([`NodeReach::reach`]), from its own cell, its
    /// morph factor and the pyramid level its vertices read: a coarse node far from the eye reads
    /// samples tens of metres away, a near one a few metres. The part of the node the edit can
    /// move (its rectangle clipped to the edit box grown by that reach) is projected with the
    /// node's culling heights joined with the edit's; the cascade is hit when that box lands in
    /// it. This is exact per node, where one reach for the whole selection is not: the finest
    /// level's reach misses coarse nodes at the cascade border (the stale strip this fixes), and
    /// the coarsest node's reach (often hundreds of metres) would redraw near cascades for edits
    /// that move nothing in them.
    fn edit_moves_inside(
        &self,
        inputs: &ShadowInputs<'_>,
        (lo, hi): &Bounds,
        vp: &[[f32; 4]; 4],
        camera_slack: f32,
    ) -> bool {
        let Some(eye) = self.eye else {
            return false;
        };
        let span = crate::cdlod::quadtree_span(
            inputs.map.quadtree_width(),
            inputs.map.quadtree_samples(),
            inputs.config.grid_size,
            inputs.root_level,
        );
        let texel = inputs.map.sample_spacing().max(inputs.map.sample_spacing_z());
        let grid = inputs.config.grid_size.max(1) as f32;
        let shadow_grid = inputs.config.grid_size.clamp(1, super::SHADOW_GRID_SIZE) as f32;
        self.selection.nodes.iter().any(|draw| {
            // Origin-centred, same frame as `rect_distance` below. `span` is the map width
            // except on the full-detail round-up (ADR-8291).
            let (min_x, min_z, size) = draw.node.world_rect(span, inputs.root_level);
            let clip = |reach: f32| {
                let r = [
                    (lo[0] - reach).max(min_x),
                    (lo[2] - reach).max(min_z),
                    (hi[0] + reach).min(min_x + size),
                    (hi[2] + reach).min(min_z + size),
                ];
                (r[0] <= r[2] && r[1] <= r[3]).then_some(r)
            };
            // The fine grid is chosen around the camera, which stays within `camera_slack` (one
            // node of the cascade level) of the anchored eye.
            let near = draw.node.rect_distance(span, inputs.root_level, eye)
                < super::CLIFF_DENSE_RADIUS_M + camera_slack;
            let node = NodeReach {
                step: size / grid,
                cell: size / shadow_grid,
                texel,
                near,
            };
            // The morph grows with the distance to the eye: the farthest point the edit can reach
            // at full morph bounds every vertex it moves.
            let Some(wide) = clip(node.reach(1.0)) else {
                return false;
            };
            let t = crate::cdlod::vertex_morph_t(draw.morph_start, draw.morph_end, farthest(wide, eye));
            let Some(r) = clip(node.reach(t)) else {
                return false;
            };
            let pad = if node.cliff() {
                super::CLIFF_RELIEF_MAX_M
            } else {
                0.0
            };
            let [node_lo, node_hi] = inputs.pyramid.node_min_max(draw.node);
            footprint(
                [r[0] - pad, lo[1].min(node_lo) - pad, r[1] - pad],
                [r[2] + pad, hi[1].max(node_hi) + pad, r[3] + pad],
                vp,
            )
            .is_some()
        })
    }

    /// Deactivates the cascade. Returns `true` when it held a selection. The edit-hit count
    /// survives (a counter for the whole run).
    pub fn clear(&mut self) -> bool {
        let had = self.eye.is_some();
        let edit_hits = self.edit_hits;
        *self = Self {
            edit_hits,
            ..Self::default()
        };
        had
    }

    /// Entries of `list` whose footprint overlaps `rect` (shadow-map UV, V down).
    pub fn region(&self, rect: [f32; 4], out: &mut Vec<u32>) {
        out.clear();
        if rect[2] <= rect[0] || rect[3] <= rect[1] {
            return;
        }
        for (slot, r) in self.list.iter().zip(&self.rects) {
            if r[2] >= rect[0] && r[0] <= rect[2] && r[3] >= rect[1] && r[1] <= rect[3] {
                out.push(*slot);
            }
        }
    }

    fn list_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.list.len().hash(&mut hasher);
        for &i in &self.list {
            if let Some(draw) = self.selection.nodes.get(i as usize) {
                draw.node.hash(&mut hasher);
                draw.morph_start.to_bits().hash(&mut hasher);
                draw.morph_end.to_bits().hash(&mut hasher);
            }
        }
        if let Some(eye) = self.eye {
            eye[0].to_bits().hash(&mut hasher);
            eye[1].to_bits().hash(&mut hasher);
        }
        self.generation.hash(&mut hasher);
        self.edit_hits.hash(&mut hasher);
        hasher.finish()
    }
}

/// One selected node's inputs to [`NodeReach::reach`].
#[derive(Clone, Copy, Debug)]
struct NodeReach {
    /// Vertex spacing, metres (`node size / grid_size`; the pyramid level follows it).
    step: f32,
    /// Drawn shadow-mesh cell, metres (`node size / SHADOW_GRID_SIZE` at most).
    cell: f32,
    /// Height sample spacing, metres.
    texel: f32,
    /// Within `CLIFF_DENSE_RADIUS_M` of the camera: the node may draw the fine cliff grid.
    near: bool,
}

impl NodeReach {
    /// Cliff relief can act on the node: `vertex_core` runs it where the read level's spacing
    /// (`texel · 2^m`, at least the texel, at least the vertex step, half of it on the fine
    /// grid) is under 4 m.
    fn cliff(self) -> bool {
        self.texel < 4.0 && (self.step < 4.0 || (self.near && self.step < 8.0))
    }

    /// Farthest (metres, along X or Z) a changed height sample can be from a point of the node's
    /// drawn shadow mesh it moves, for vertices with morph factor up to `t` (`vs_shadow`):
    ///
    /// - the morph moves a vertex up to `t · step` off its grid point;
    /// - its height is a bilinear read of pyramid levels `⌊m⌋` and `⌈m⌉`
    ///   (`m = vertex_mip(step, texel, t)`); a level-`k` texel (`T = texel · 2^k` wide) averages
    ///   the samples under it, so a read at a point reaches `1.5 T` past it, and `2 T` with the
    ///   extra taps of an odd side's last texel (`height_mips::parent_taps`);
    /// - with cliff relief, its normal taps sit up to `T` further (`normal_mip`), and the fine
    ///   grid's own morph moves a vertex up to half a step;
    /// - a drawn triangle spans one shadow cell, so its inside moves when any corner does.
    fn reach(self, t: f32) -> f32 {
        let m = crate::height_mips::vertex_mip(self.step, self.texel, t);
        let level_texel = self.texel * m.ceil().exp2();
        let (normal_taps, fine_morph) = if self.cliff() {
            (level_texel, if self.near { 0.5 * self.step } else { 0.0 })
        } else {
            (0.0, 0.0)
        };
        t * self.step + fine_morph + 2.0 * level_texel + normal_taps + self.cell
    }
}

/// [`NodeReach::reach`] and [`NodeReach::cliff`] of a node drawing `step`-metre vertices in
/// `cell`-metre shadow cells over `texel`-metre samples, `near` the camera, at morph `t`: how far
/// (metres, along X or Z) a height sample can be from a point of the drawn shadow mesh it moves,
/// and whether cliff relief can act on it. The sun cover (`crate::sun_cover`) bounds the drawn
/// mesh with it (ADR-10161).
pub(crate) fn shadow_vertex_reach(step: f32, cell: f32, texel: f32, near: bool, t: f32) -> (f32, bool) {
    let node = NodeReach {
        step,
        cell,
        texel,
        near,
    };
    (node.reach(t), node.cliff())
}

/// Distance from `eye` to the farthest corner of the XZ rectangle `[x0, z0, x1, z1]`.
fn farthest(rect: [f32; 4], eye: [f32; 2]) -> f32 {
    let dx = (rect[0] - eye[0]).abs().max((rect[2] - eye[0]).abs());
    let dz = (rect[1] - eye[1]).abs().max((rect[3] - eye[1]).abs());
    dx.hypot(dz)
}

/// The finest level whose grid cell (`node size / grid`) is at most two shadow texels.
pub(super) fn shadow_level(world: f32, root_level: u32, grid: u32, texel: f32) -> u32 {
    crate::cdlod::cascade_min_level(world, root_level, grid, texel)
}

/// The box's shadow-map UV footprint under an orthographic `vp`, or `None` when the box
/// is outside one of the four side planes (depth is not tested: casters beyond the depth
/// range still shadow, as in the camera cull).
fn footprint(lo: [f32; 3], hi: [f32; 3], vp: &[[f32; 4]; 4]) -> Option<[f32; 4]> {
    let (mut x0, mut y0, mut x1, mut y1) =
        (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for corner in 0..8u8 {
        let p = [
            if corner & 1 == 0 { lo[0] } else { hi[0] },
            if corner & 2 == 0 { lo[1] } else { hi[1] },
            if corner & 4 == 0 { lo[2] } else { hi[2] },
        ];
        let clip = |row: usize| vp[0][row] * p[0] + vp[1][row] * p[1] + vp[2][row] * p[2] + vp[3][row];
        let w = clip(3);
        let w = if w.abs() < 1.0e-6 { 1.0e-6 } else { w };
        let (x, y) = (clip(0) / w, clip(1) / w);
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    if x1 < -1.0 || x0 > 1.0 || y1 < -1.0 || y0 > 1.0 {
        return None;
    }
    Some([x0 * 0.5 + 0.5, 0.5 - y1 * 0.5, x1 * 0.5 + 0.5, 0.5 - y0 * 0.5])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> TerrainMap {
        let n = 257;
        let heights = crate::synthetic_mountains(n, 2048.0, 220.0, 7);
        TerrainMap::from_heights(n, n, heights, 2048.0, 1.0, 0.0).expect("map")
    }

    /// An orthographic box `±r` around the origin looking straight down (column-major).
    fn top_down(r: f32) -> [[f32; 4]; 4] {
        // x → x / r, z → -y / r, y → depth.
        [
            [1.0 / r, 0.0, 0.0, 0.0],
            [0.0, 0.0, -1.0 / 2000.0, 0.0],
            [0.0, -1.0 / r, 0.0, 0.0],
            [0.0, 0.0, 0.5, 1.0],
        ]
    }

    #[test]
    fn a_moving_eye_reselects_only_past_one_node_of_the_cascade_level() {
        let map = map();
        let config = CdlodConfig::default();
        let root = 6;
        let pyramid = MinMaxPyramid::build(&map, root).expect("pyramid");
        let inputs = ShadowInputs {
            map: &map,
            pyramid: &pyramid,
            config: &config,
            root_level: root,
            heights_generation: 0,
            edits: &EditLog::default(),
        };
        let vp = top_down(600.0);
        let texel = 2.0 * 600.0 / 2048.0;
        let level = shadow_level(map.world_width(), root, config.grid_size, texel);
        let node = node_world_size(map.world_width(), root, level);
        let mut cascade = CascadeShadow::default();
        assert!(cascade.update(&inputs, [0.0, 0.0], texel, &vp));
        let key = cascade.key;
        let mut reselections = 0;
        // 60 frames at 0.25 m: 15 m, under one node of the cascade level.
        for frame in 1..=60 {
            let eye = [frame as f32 * 0.25 * (node / 15.0).min(1.0), 0.0];
            if cascade.update(&inputs, eye, texel, &vp) {
                reselections += 1;
            }
        }
        assert_eq!(reselections, 0, "moved less than one {node} m node");
        assert_eq!(cascade.key, key, "the list key changed without a reselection");
        assert!(cascade.update(&inputs, [node * 1.5, 0.0], texel, &vp));
        assert_ne!(cascade.key, key);
    }

    #[test]
    fn the_shadow_selection_stops_at_the_cascade_level_and_tiles_the_map() {
        let map = map();
        let config = CdlodConfig::default();
        let root = 6;
        let pyramid = MinMaxPyramid::build(&map, root).expect("pyramid");
        let level = 2;
        let selection = select_nodes_min_level(&map, &pyramid, &config, root, [10.0, -30.0], None, level);
        let world = map.world_width();
        let area: f32 = selection
            .nodes
            .iter()
            .map(|d| {
                assert!(d.node.level >= level, "{:?} finer than the cascade level", d.node);
                node_world_size(world, root, d.node.level).powi(2)
            })
            .sum();
        assert!(
            (area - world * world).abs() < 1.0,
            "selection covers {area} of {}",
            world * world
        );
        // Coarser than the camera selection near the eye, identical far away.
        let full = crate::cdlod::select_nodes(&map, &pyramid, &config, root, [10.0, -30.0], None);
        assert!(selection.nodes.len() < full.nodes.len());
        let coarse_full: Vec<_> = full
            .nodes
            .iter()
            .filter(|d| d.node.level > level)
            .map(|d| d.node)
            .collect();
        for node in coarse_full {
            assert!(selection.nodes.iter().any(|d| d.node == node), "{node:?} missing");
        }
    }

    #[test]
    fn a_region_keeps_only_nodes_under_it() {
        let map = map();
        let config = CdlodConfig::default();
        let root = 6;
        let pyramid = MinMaxPyramid::build(&map, root).expect("pyramid");
        let inputs = ShadowInputs {
            map: &map,
            pyramid: &pyramid,
            config: &config,
            root_level: root,
            heights_generation: 0,
            edits: &EditLog::default(),
        };
        let vp = top_down(1024.0);
        let mut cascade = CascadeShadow::default();
        cascade.update(&inputs, [0.0, 0.0], 1.0, &vp);
        let mut region = Vec::new();
        cascade.region([0.0, 0.0, 0.25, 0.25], &mut region);
        assert!(!region.is_empty() && region.len() < cascade.list.len());
        cascade.region([0.0; 4], &mut region);
        assert!(region.is_empty());
    }

    /// ADR-7585: an edit box reselects (and re-keys) only the cascades whose box it lands in;
    /// a one-sample edit far from the camera touches none, and the per-cascade hit counters
    /// scale with the cascade boxes, not the map.
    #[test]
    fn an_edit_invalidates_only_the_cascades_it_lands_in() {
        let map = map();
        let config = CdlodConfig::default();
        let root = 6;
        let pyramid = MinMaxPyramid::build(&map, root).expect("pyramid");
        let mut log = EditLog::default();
        // Four nested cascades around the origin, 2048² texels each.
        let radii = [50.0f32, 150.0, 400.0, 900.0];
        let mut cascades: [CascadeShadow; 4] = Default::default();
        let update_all = |cascades: &mut [CascadeShadow; 4], log: &EditLog| {
            let inputs = ShadowInputs {
                map: &map,
                pyramid: &pyramid,
                config: &config,
                root_level: root,
                heights_generation: 0,
                edits: log,
            };
            std::array::from_fn::<bool, 4, _>(|i| {
                let r = radii[i];
                cascades[i].update(&inputs, [0.0, 0.0], 2.0 * r / 2048.0, &top_down(r))
            })
        };
        assert_eq!(update_all(&mut cascades, &log), [true; 4], "first placement");
        let keys = cascades.each_ref().map(|c| c.key);
        // One sample, 1 m deep, 980 m out (the map is 2048 m): outside every cascade box.
        log.push(([980.0, 10.0, 980.0], [981.0, 11.0, 981.0]));
        assert_eq!(update_all(&mut cascades, &log), [false; 4]);
        assert_eq!(cascades.each_ref().map(|c| c.key), keys, "no cascade re-keyed");
        assert_eq!(cascades.each_ref().map(|c| c.edit_hits), [0; 4]);
        // A crater 300 m out: inside the two wide cascades only.
        log.push(([300.0, 0.0, -10.0], [310.0, 220.0, 0.0]));
        assert_eq!(update_all(&mut cascades, &log), [false, false, true, true]);
        assert_eq!(cascades[0].key, keys[0]);
        assert_eq!(cascades[1].key, keys[1]);
        assert_ne!(
            cascades[2].key, keys[2],
            "the hit cascade redraws even with the same nodes"
        );
        assert_ne!(cascades[3].key, keys[3]);
        // A crater under the camera: every cascade.
        log.push(([-3.0, 0.0, -3.0], [3.0, 220.0, 3.0]));
        assert_eq!(update_all(&mut cascades, &log), [true; 4]);
        assert_eq!(cascades.each_ref().map(|c| c.edit_hits), [1, 1, 2, 2]);
        // Nothing new: nothing reselects.
        assert_eq!(update_all(&mut cascades, &log), [false; 4]);
        // A cascade that missed more boxes than the log keeps reselects as for a whole-map change.
        let mut late = CascadeShadow::default();
        let inputs = ShadowInputs {
            map: &map,
            pyramid: &pyramid,
            config: &config,
            root_level: root,
            heights_generation: 0,
            edits: &log,
        };
        late.update(&inputs, [0.0, 0.0], 0.05, &top_down(50.0));
        for _ in 0..=EDIT_LOG_CAP {
            log.push(([980.0, 10.0, 980.0], [981.0, 11.0, 981.0]));
        }
        let inputs = ShadowInputs {
            map: &map,
            pyramid: &pyramid,
            config: &config,
            root_level: root,
            heights_generation: 0,
            edits: &log,
        };
        assert!(late.update(&inputs, [0.0, 0.0], 0.05, &top_down(50.0)));
    }

    fn shadow_inputs<'a>(
        map: &'a TerrainMap,
        pyramid: &'a MinMaxPyramid,
        config: &'a CdlodConfig,
        root_level: u32,
        edits: &'a EditLog,
    ) -> ShadowInputs<'a> {
        ShadowInputs {
            map,
            pyramid,
            config,
            root_level,
            heights_generation: 0,
            edits,
        }
    }

    /// `map` with sample `(col, row)` raised by `dh`, and the band's world box as
    /// `TerrainSharedDraw::note_edit_box` records it (the sample and one around, old and new
    /// heights).
    fn raise_sample(map: &TerrainMap, col: u32, row: u32, dh: f32) -> (TerrainMap, Bounds) {
        let w = map.width();
        let mut heights = map.heights().to_vec();
        heights[(row * w + col) as usize] += dh;
        let edited =
            TerrainMap::from_heights(w, map.height(), heights, map.world_width(), 1.0, 0.0).expect("map");
        let (mut lo_h, mut hi_h) = (f32::INFINITY, f32::NEG_INFINITY);
        for r in row.saturating_sub(1)..=(row + 1).min(map.height() - 1) {
            for c in col.saturating_sub(1)..=(col + 1).min(w - 1) {
                for m in [map, &edited] {
                    let h = m.height_at_sample(i64::from(c), i64::from(r));
                    lo_h = lo_h.min(h);
                    hi_h = hi_h.max(h);
                }
            }
        }
        let s = map.sample_spacing();
        let [x, z] = map.sample_to_world(col, row);
        (edited, ([x - s, lo_h, z - s], [x + s, hi_h, z + s]))
    }

    /// Whether a drawn shadow vertex of `selection` inside the `±r` box moves between `before`
    /// and `after`: the CPU mirror of `vs_shadow` (`eval_node_vertex_filtered`, morph `t` from
    /// the anchored eye at the origin, the shadow grid's vertex stride).
    fn moves_a_vertex_inside(
        selection: &LodSelection,
        (before, mips_before): (&TerrainMap, &crate::height_mips::HeightMips),
        (after, mips_after): (&TerrainMap, &crate::height_mips::HeightMips),
        (config, root): (&CdlodConfig, u32),
        r: f32,
        near: [f32; 2],
    ) -> bool {
        let stride = (config.grid_size / super::super::SHADOW_GRID_SIZE).max(1);
        for draw in &selection.nodes {
            let (min_x, min_z, size) = draw.node.draw_rect(before, root, config.grid_size);
            // Only nodes within 400 m of the edit can read it.
            if near[0] < min_x - 400.0 || near[0] > min_x + size + 400.0 {
                continue;
            }
            if near[1] < min_z - 400.0 || near[1] > min_z + size + 400.0 {
                continue;
            }
            let step = size / config.grid_size as f32;
            for gz in (0..=config.grid_size).step_by(stride as usize) {
                for gx in (0..=config.grid_size).step_by(stride as usize) {
                    let (x0, z0) = (min_x + gx as f32 * step, min_z + gz as f32 * step);
                    let t = crate::cdlod::vertex_morph_t(draw.morph_start, draw.morph_end, x0.hypot(z0));
                    let a = crate::cdlod::eval_node_vertex_filtered(
                        before,
                        mips_before,
                        config,
                        draw.node,
                        root,
                        gx,
                        gz,
                        t,
                    );
                    if a[0].abs() >= r || a[2].abs() >= r {
                        continue;
                    }
                    let b = crate::cdlod::eval_node_vertex_filtered(
                        after, mips_after, config, draw.node, root, gx, gz, t,
                    );
                    if (a[1] - b[1]).abs() > 1.0e-3 {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// v0.5.1 follow-up: `select_nodes_min_level` returns coarser nodes away from the eye, whose
    /// cells, morph and box-filtered height levels reach much further than the cascade level's
    /// cell. An edit just outside a wide cascade's box, past the finest level's reach, still
    /// moves a vertex of such a node inside the box (proved here with the CPU mirror of the
    /// shadow vertex): the cascade must reselect and re-key, or a stale strip stays at its
    /// border. Every edit along the row that moves a vertex inside must invalidate it.
    #[test]
    fn an_edit_outside_a_wide_cascade_that_moves_a_coarse_vertex_inside_it_invalidates_it() {
        // 4 km map, 8 m samples: nodes 1.8-2 km from the eye are 256 m (16 m cells) and morph.
        let n = 513;
        let map = TerrainMap::from_heights(
            n,
            n,
            crate::synthetic_mountains(n, 4096.0, 220.0, 7),
            4096.0,
            1.0,
            0.0,
        )
        .expect("map");
        let config = CdlodConfig::default();
        let root = 7;
        let world = map.world_width();
        let pyramid = MinMaxPyramid::build(&map, root).expect("pyramid");
        let empty = EditLog::default();
        let mips = crate::height_mips::HeightMips::build(&map);
        let grid = config.grid_size.clamp(1, super::super::SHADOW_GRID_SIZE);
        // Box edges 1.89-1.92 km out (inside the 256 m nodes' morph band), at several offsets
        // to the 16 m vertex grid and the 32 m texels of the level those vertices read; and
        // 40-70 m out, on the finest nodes (the near cascades' reach must hold too).
        let cascades: Vec<(f32, CascadeShadow)> = (0..32)
            .map(|i| {
                let r = if i < 16 {
                    1890.0 + 2.0 * i as f32
                } else {
                    40.0 + 2.0 * (i - 16) as f32
                };
                let mut cascade = CascadeShadow::default();
                let texel = 2.0 * r / 2048.0;
                assert!(cascade.update(
                    &shadow_inputs(&map, &pyramid, &config, root, &empty),
                    [0.0, 0.0],
                    texel,
                    &top_down(r)
                ));
                (r, cascade)
            })
            .collect();
        let row = (n - 1) / 2; // z = 0
        let mut moved_past_finest = 0;
        for col in 0..n {
            let [x, z] = map.sample_to_world(col, row);
            if !(1890.0..=2010.0).contains(&x) && !(40.0..=130.0).contains(&x) {
                continue;
            }
            let (edited, edit_box) = raise_sample(&map, col, row, 40.0);
            let edited_mips = crate::height_mips::HeightMips::build(&edited);
            let edited_pyramid = MinMaxPyramid::build(&edited, root).expect("pyramid");
            for (r, placed) in &cascades {
                let r = *r;
                if x <= r || x > r + 120.0 {
                    continue;
                }
                let moved = moves_a_vertex_inside(
                    &placed.selection,
                    (&map, &mips),
                    (&edited, &edited_mips),
                    (&config, root),
                    r,
                    [x, z],
                );
                if !moved {
                    continue;
                }
                // The reach the cascade's finest level alone gives.
                let texel = 2.0 * r / 2048.0;
                let finest = node_world_size(world, root, shadow_level(world, root, grid, texel));
                let finest_reach = 4.0 * finest / grid as f32 + super::super::CLIFF_RELIEF_MAX_M;
                if edit_box.0[0] - finest_reach > r {
                    moved_past_finest += 1;
                }
                let mut log = EditLog::default();
                log.push(edit_box);
                let mut cascade = CascadeShadow::default();
                cascade.update(
                    &shadow_inputs(&map, &pyramid, &config, root, &empty),
                    [0.0, 0.0],
                    texel,
                    &top_down(r),
                );
                let key = cascade.key;
                assert!(
                    cascade.update(
                        &shadow_inputs(&edited, &edited_pyramid, &config, root, &log),
                        [0.0, 0.0],
                        texel,
                        &top_down(r)
                    ),
                    "an edit at x = {x} m moves a vertex inside the ±{r} m cascade but did not reselect it"
                );
                assert_eq!(cascade.edit_hits, 1);
                assert_ne!(cascade.key, key, "the cascade must redraw");
            }
        }
        assert!(
            moved_past_finest > 0,
            "the row must hold an edit past the finest level's reach that moves a vertex inside a cascade"
        );
        // Far from a small cascade nothing reselects (coarse reach is still bounded).
        let small = top_down(400.0);
        let small_texel = 800.0 / 2048.0;
        let mut cascade = CascadeShadow::default();
        cascade.update(
            &shadow_inputs(&map, &pyramid, &config, root, &empty),
            [0.0, 0.0],
            small_texel,
            &small,
        );
        let col = ((700.0 + world * 0.5) / map.sample_spacing()) as u32;
        let (edited, edit_box) = raise_sample(&map, col, row, 40.0);
        let edited_pyramid = MinMaxPyramid::build(&edited, root).expect("pyramid");
        let mut log = EditLog::default();
        log.push(edit_box);
        assert!(!cascade.update(
            &shadow_inputs(&edited, &edited_pyramid, &config, root, &log),
            [0.0, 0.0],
            small_texel,
            &small
        ));
        assert_eq!(cascade.edit_hits, 0, "an edit 300 m outside a ±400 m cascade");
    }
}
