// SPDX-License-Identifier: MPL-2.0
//! Per-tile terrain collision for streamed levels (spec 78 § 13.1, ADR-2317).
//!
//! A heightfield collider with [`TerrainTileCollision`] is not built as one collider over the whole
//! map. Its heights are cut into square tiles (neighbors share their border samples, so the pieces
//! meet without a gap), and a tile is a collider in the physics world only while something holds it:
//!
//! - every spawned cell of the terrain's level holds the tiles under its rectangle, from the step
//!   the cell spawned until it unloaded (the `terrain_collision` gate's list of awaiting cells, then
//!   its marks);
//! - every streaming source holds the tiles within `view_radius_m` of its ground position, so the
//!   ground of cells with no content (no record, nothing spawns) is there around the player.
//!
//! The holders of a tile are a set (a reference count), so a tile shared by four cells stays while any
//! of them is loaded. The gate marks a spawned cell once all its tiles exist, and marks a cell with no
//! content while all its tiles exist (and takes that mark back when a tile goes). New tiles are built
//! in tile order, at most `max_tiles_per_step` per fixed step, so a lockstep run marks the same cells
//! on the same steps.
//!
//! The collider sits on the terrain's own static body (a heightfield under another entity's body is
//! not tiled), and neither the body nor the collider offset may be rotated: tiles are laid out on the
//! world axes. Use one tiled terrain per level; a cell is marked once every tiled terrain over it
//! has its tiles.
//!
//! Tile colliders hang off the terrain's static body, so a floating-origin rebase moves them with it,
//! and a hit on a tile reports the terrain entity. A changed `Collider` (a terrain edit) rebuilds the
//! kept tiles from the new heights.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use rapier3d::geometry::ColliderHandle as RapierColliderHandle;
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_scene::level::{LevelId, LevelMember};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{
    GateId, OriginOffset, Readiness, StreamGates, StreamLevelLayouts, StreamViews, TERRAIN_COLLISION_GATE,
    TerrainTileCollision,
};
use tracing::instrument;

use crate::components::{Collider, ColliderShape, RigidBodyHandle};
use crate::convert::from_rapier_vec3;
use crate::layers::PhysicsLayers;
use crate::world::PhysicsWorld;
use regia_scene::Transform;

/// Triangle pieces that follow a tiled terrain's tiles (spec 26 § 2a.8, ADR-8537): the canonical
/// triangles of a terrain's mesh layers (`regia_terrain::TerrainMeshLayers::collider_pieces`), for
/// hosts that do not lockstep. Put it on the terrain entity next to its `Collider` heightfield and
/// [`TerrainTileCollision`]; without the component nothing changes.
///
/// Coordinates are the heightfield's frame: metres relative to the field's center (the body plus
/// the collider offset), the frame the tiles are laid out in. A piece is a `TriMesh` collider on the
/// terrain's static body while any built tile overlaps its rectangle (a piece without one follows
/// any built tile); it is built in the same fixed step as that tile and counts toward
/// `max_tiles_per_step`. Hits report the terrain entity. A changed component rebuilds the pieces.
/// A lockstep sim does not use these: it queries `regia_world_geom` (spec 121: an engine physics
/// step that is not bit-identical is not the collision).
///
/// A terrain without [`TerrainTileCollision`] (an untiled body) gets one `TriMesh` per layer
/// instead: the triangles of every piece with the same [`TerrainColliderPiece::layer`], in piece
/// order, on the terrain's body while it has one (rects are not used). They are built in the
/// fixed step after the component, the collider or the body changed, all in that step (an untiled
/// heightfield is built whole too), and are not counted in [`TerrainTileStats`] (ADR-8872).
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct TerrainLayerColliders {
    /// The pieces, in the order the host gives them.
    pub pieces: Vec<TerrainColliderPiece>,
}

/// One piece of [`TerrainLayerColliders`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerrainColliderPiece {
    /// The mesh layer the triangles belong to (the canonical `TriRef::layer`). On an untiled body
    /// the pieces of one layer are one collider; a tiled terrain ignores it.
    pub layer: u16,
    /// `[min_x, min_z, size]` of the square the piece belongs to (a layer chunk), relative to the
    /// field's center; `None` follows any built tile.
    pub rect: Option<[f32; 3]>,
    /// Triangles relative to the field's center, metres.
    pub triangles: Vec<[[f32; 3]; 3]>,
}

/// What holds a tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Holder {
    /// A spawned cell.
    Cell(LevelId, CellCoord),
    /// The streaming sources.
    Views,
}

/// The tiling of one heightfield: sample cells per tile and the tile counts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Tiling {
    /// Samples along Z (rows of the height matrix).
    pub(crate) nx: usize,
    /// Samples along X (columns of the height matrix).
    pub(crate) nz: usize,
    /// Heightfield cells per tile side.
    pub(crate) k: usize,
    /// Tiles along X and Z.
    pub(crate) tiles: [u32; 2],
    /// Sample spacing along X and Z, meters.
    pub(crate) step: [f32; 2],
    /// `[width along X, height scale, width along Z]`.
    pub(crate) scale: [f32; 3],
}

impl Tiling {
    /// The tiling of a field of `nx × nz` samples spanning `scale`, with tiles about `tile_m` wide.
    pub(crate) fn new(nx: usize, nz: usize, scale: [f32; 3], tile_m: f32) -> Option<Self> {
        if nx < 2 || nz < 2 || !scale.iter().all(|s| s.is_finite()) || scale[0] <= 0.0 || scale[2] <= 0.0 {
            return None;
        }
        let step = [scale[0] / (nz - 1) as f32, scale[2] / (nx - 1) as f32];
        let tile = if tile_m.is_finite() && tile_m > 0.0 {
            tile_m
        } else {
            128.0
        };
        let k = ((tile / step[0].max(step[1])).round() as usize).clamp(1, (nx - 1).max(nz - 1));
        Some(Self {
            nx,
            nz,
            k,
            tiles: [(nz - 1).div_ceil(k) as u32, (nx - 1).div_ceil(k) as u32],
            step,
            scale,
        })
    }

    /// Column (X) and row (Z) sample ranges of tile `t`, inclusive.
    pub(crate) fn samples(&self, t: [u32; 2]) -> ([usize; 2], [usize; 2]) {
        let (c0, r0) = (t[0] as usize * self.k, t[1] as usize * self.k);
        (
            [c0, (c0 + self.k).min(self.nz - 1)],
            [r0, (r0 + self.k).min(self.nx - 1)],
        )
    }

    /// The tile's heights (`[row + col * rows]`, Z fastest), its `nx`, `nz`, scale and center
    /// relative to the field's center.
    pub(crate) fn cut(&self, heights: &[f32], t: [u32; 2]) -> Option<(ColliderShape, Vec3)> {
        if t[0] >= self.tiles[0] || t[1] >= self.tiles[1] || heights.len() != self.nx * self.nz {
            return None;
        }
        let ([c0, c1], [r0, r1]) = self.samples(t);
        let (rows, cols) = (r1 - r0 + 1, c1 - c0 + 1);
        let mut out = Vec::with_capacity(rows * cols);
        for col in c0..=c1 {
            out.extend_from_slice(&heights[r0 + col * self.nx..=r1 + col * self.nx]);
        }
        let shape = ColliderShape::HeightField {
            nx: rows,
            nz: cols,
            heights: out,
            scale: Vec3::new(
                (cols - 1) as f32 * self.step[0],
                self.scale[1],
                (rows - 1) as f32 * self.step[1],
            ),
        };
        let center = Vec3::new(
            -0.5 * self.scale[0] + (c0 + c1) as f32 * 0.5 * self.step[0],
            0.0,
            -0.5 * self.scale[2] + (r0 + r1) as f32 * 0.5 * self.step[1],
        );
        Some((shape, center))
    }

    /// The tiles holding any point of the rectangle `[min, max]` (XZ, relative to the field's center),
    /// as inclusive index ranges; `None` when the rectangle misses the field.
    pub(crate) fn tiles_over(&self, min: [f64; 2], max: [f64; 2]) -> Option<([u32; 2], [u32; 2])> {
        if !min.iter().chain(&max).all(|v| v.is_finite()) {
            return None;
        }
        let half = [f64::from(self.scale[0]) * 0.5, f64::from(self.scale[2]) * 0.5];
        if max[0] < -half[0] || max[1] < -half[1] || min[0] > half[0] || min[1] > half[1] {
            return None;
        }
        let tile = [
            f64::from(self.step[0]) * self.k as f64,
            f64::from(self.step[1]) * self.k as f64,
        ];
        // Half-open: a rectangle that ends on a tile border does not reach into the next tile.
        let last = |axis: usize| f64::from(self.tiles[axis] - 1);
        let range = |lo: f64, hi: f64, axis: usize| -> [u32; 2] {
            let first = ((lo + half[axis]) / tile[axis]).floor().clamp(0.0, last(axis));
            let end = (((hi + half[axis]) / tile[axis]).ceil() - 1.0).clamp(first, last(axis));
            [first as u32, end as u32]
        };
        Some((range(min[0], max[0], 0), range(min[1], max[1], 1)))
    }
}

/// One kept tile.
#[derive(Debug, Default)]
struct Tile {
    holders: BTreeSet<Holder>,
    handle: Option<RapierColliderHandle>,
    /// The tile has nothing to build (every sample a hole, or a window rapier refuses): no
    /// collider can exist, so it counts as done for readiness and is tried again only when the
    /// heights or settings change (corr-physics audit: such a tile kept its cells from ever being
    /// marked).
    empty: bool,
}

/// The tiles of one terrain entity.
#[derive(Debug)]
struct TerrainRt {
    settings: TerrainTileCollision,
    /// The body the tiles hang on. A rebuilt body (its layer parked and unparked, `RigidBody`
    /// re-added) took the old tiles with it, so their handles are dropped and the tiles rebuilt.
    body: Option<rapier3d::dynamics::RigidBodyHandle>,
    tiling: Tiling,
    tiles: BTreeMap<[u32; 2], Tile>,
    /// Spawned cells and the tiles they hold.
    cells: BTreeMap<(LevelId, CellCoord), Vec<[u32; 2]>>,
    /// Cells with no content this terrain marked.
    marked_empty: BTreeSet<(LevelId, CellCoord)>,
    /// The collider of each [`TerrainLayerColliders`] piece, by index (`None`: not built).
    pieces: Vec<Option<RapierColliderHandle>>,
}

/// What a settled step compares so it can leave the tiles alone.
///
/// Stored only after a step that walked the holders and finished every held tile. A later
/// step with the same views, layouts, origin, gate revision, awaiting cells, body
/// translations and collider ticks writes the cached counters and returns. A step that
/// stopped at `max_tiles_per_step` leaves this empty, so the next step keeps building.
#[derive(Debug, Clone)]
struct TileWatch {
    views: Option<StreamViews>,
    layouts: Option<StreamLevelLayouts>,
    origin: [f64; 3],
    revision: u64,
    awaiting: Vec<(LevelId, CellCoord)>,
    /// Body translation bits and handle, so a moved terrain retiles even when its collider tick
    /// is quiet, and a rebuilt body rebuilds its tiles.
    body_bits: BTreeMap<Entity, [u32; 5]>,
    /// The level each terrain is a member of, so a `LevelMember` change retiles without waiting
    /// for another change.
    levels: BTreeMap<Entity, Option<LevelId>>,
    stats: TerrainTileStats,
}

/// The layer colliders of one untiled terrain (no [`TerrainTileCollision`]).
#[derive(Debug)]
struct UntiledRt {
    /// The body the colliders hang on; another handle means the old body took them along.
    body: rapier3d::dynamics::RigidBodyHandle,
    /// One collider per layer, in layer order.
    handles: Vec<RapierColliderHandle>,
}

/// The tiled terrains, by entity.
#[derive(Resource, Debug, Default)]
pub(crate) struct TerrainTiles {
    terrains: BTreeMap<Entity, TerrainRt>,
    /// Untiled terrains with [`TerrainLayerColliders`], by entity (ADR-8872).
    untiled: BTreeMap<Entity, UntiledRt>,
    watch: Option<TileWatch>,
    /// Tile-index buffers filled by this world's steps. The tile system runs on a worker,
    /// so the settled-step test reads this instead of a counter on the test thread.
    index_buffers: u64,
}

/// Counters of the tiled terrains (read by `physics.terrain_tiles`).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerrainTileStats {
    /// Terrains built as tiles.
    pub terrains: u32,
    /// Tile colliders in the physics world now.
    pub resident_tiles: u32,
    /// Tiles held but not built yet (over the per-step limit), plus [`TerrainLayerColliders`]
    /// pieces waiting for that limit.
    pub pending_tiles: u32,
    /// Tile colliders built so far.
    pub built: u64,
    /// Tile colliders removed so far.
    pub removed: u64,
    /// Cells the terrain gate holds (spawned cells of tiled terrains).
    pub held_cells: u32,
}

/// The tiled terrain's data each step: its collider, body and level.
type TiledTerrain = (
    Entity,
    Ref<'static, Collider>,
    &'static TerrainTileCollision,
    Option<&'static RigidBodyHandle>,
    Option<&'static LevelMember>,
    Option<Ref<'static, TerrainLayerColliders>>,
);

/// An untiled terrain with layer pieces: its collider, body and pieces (ADR-8872).
type UntiledTerrain = (
    Entity,
    Ref<'static, Collider>,
    Option<&'static RigidBodyHandle>,
    Ref<'static, TerrainLayerColliders>,
);

/// What the tiles read of the streaming runtime: the sources, the open levels' grids, the origin.
type StreamInputs<'w> = (
    Option<Res<'w, StreamViews>>,
    Option<Res<'w, StreamLevelLayouts>>,
    Option<Res<'w, OriginOffset>>,
);

/// The streaming inputs a settled-tile check compares.
struct WatchScene<'a> {
    gates: &'a StreamGates,
    gate: GateId,
    views: &'a Option<Res<'a, StreamViews>>,
    layouts: &'a Option<Res<'a, StreamLevelLayouts>>,
    origin: [f64; 3],
}

/// True when `watch` still describes this step, so the tile walk can be skipped.
///
/// Reads every collider's change tick: a mutation this frame must be seen now, not on the
/// next step that happens to compare ticks.
fn tile_watch_holds(
    watch: &TileWatch,
    terrains_rt: &BTreeMap<Entity, TerrainRt>,
    terrains: &Query<TiledTerrain>,
    physics: &PhysicsWorld,
    scene: &WatchScene<'_>,
) -> bool {
    if watch.origin != scene.origin || scene.gates.revision() != watch.revision {
        return false;
    }
    if watch.views.as_ref() != scene.views.as_ref().map(|v| &**v) {
        return false;
    }
    if watch.layouts.as_ref() != scene.layouts.as_ref().map(|v| &**v) {
        return false;
    }
    let mut awaiting_n = 0usize;
    for cell in scene.gates.awaiting(scene.gate) {
        if watch.awaiting.get(awaiting_n) != Some(&cell) {
            return false;
        }
        awaiting_n += 1;
    }
    if awaiting_n != watch.awaiting.len() {
        return false;
    }
    let mut seen = 0usize;
    for (entity, collider, settings, body, member, pieces) in terrains.iter() {
        seen += 1;
        if collider.is_changed() {
            return false;
        }
        // Pieces added, changed or taken away rebuild them.
        let piece_count = pieces.as_ref().map_or(0, |p| p.pieces.len());
        if pieces.as_ref().is_some_and(|p| p.is_changed())
            || terrains_rt
                .get(&entity)
                .is_some_and(|rt| rt.pieces.len() != piece_count)
        {
            return false;
        }
        if watch.levels.get(&entity) != Some(&member.map(LevelMember::level)) {
            return false;
        }
        let Some(rt) = terrains_rt.get(&entity) else {
            return false;
        };
        if rt.settings != *settings {
            return false;
        }
        let Some(handle) = body.map(RigidBodyHandle::to_handle) else {
            return false;
        };
        let Some(rb) = physics.bodies.get(handle) else {
            return false;
        };
        let pos = from_rapier_vec3(rb.translation());
        let (index, generation) = handle.into_raw_parts();
        let bits = [
            pos.x.to_bits(),
            pos.y.to_bits(),
            pos.z.to_bits(),
            index,
            generation,
        ];
        if watch.body_bits.get(&entity) != Some(&bits) {
            return false;
        }
    }
    seen == terrains_rt.len() && seen == watch.body_bits.len()
}

/// `FixedUpdate`, after the collider sync: keeps each tiled terrain's tiles for its holders and marks
/// the `terrain_collision` gate (module docs).
#[instrument(skip_all, target = "regia_physics")]
pub(crate) fn step_terrain_tiles(
    (mut state, mut stats): (ResMut<TerrainTiles>, ResMut<TerrainTileStats>),
    mut physics: ResMut<PhysicsWorld>,
    layers: Res<PhysicsLayers>,
    (terrains, untiled): (
        Query<TiledTerrain>,
        Query<UntiledTerrain, Without<TerrainTileCollision>>,
    ),
    gates: Option<ResMut<StreamGates>>,
    (views, layouts, origin): StreamInputs,
) {
    // Untiled terrains' layer pieces do not depend on the tiles, the gates or the views.
    if !untiled.is_empty() || !state.untiled.is_empty() {
        step_untiled_pieces(&mut state.untiled, &mut physics, &layers, &untiled);
    }
    // A terrain that went away: its colliders went with its body, or go now. An empty `gone`
    // does not allocate; the live-entity set used to.
    let mut gone = Vec::new();
    for entity in state.terrains.keys().copied() {
        if terrains.get(entity).is_err() {
            gone.push(entity);
        }
    }
    let removed_any = !gone.is_empty();
    for entity in gone {
        if let Some(rt) = state.terrains.remove(&entity) {
            for tile in rt.tiles.into_values() {
                if let Some(handle) = tile.handle {
                    remove_tile(&mut physics, handle);
                }
            }
            for handle in rt.pieces.into_iter().flatten() {
                remove_tile(&mut physics, handle);
            }
        }
    }
    if terrains.is_empty() {
        *stats = TerrainTileStats {
            built: stats.built,
            removed: stats.removed,
            ..TerrainTileStats::default()
        };
        state.watch = None;
        return;
    }
    let Some(mut gates) = gates else {
        state.watch = None;
        return;
    };
    let gate = gates.register_for_claimed_levels(TERRAIN_COLLISION_GATE, Readiness::SimReady);
    let offset = origin.map_or([0.0; 3], |o| o.0);
    let skip = if removed_any {
        false
    } else {
        let TerrainTiles {
            terrains: terrains_rt,
            watch,
            index_buffers: _,
            untiled: _,
        } = &mut *state;
        watch.as_ref().is_some_and(|watch| {
            tile_watch_holds(
                watch,
                terrains_rt,
                &terrains,
                &physics,
                &WatchScene {
                    gates: &gates,
                    gate,
                    views: &views,
                    layouts: &layouts,
                    origin: offset,
                },
            )
        })
    };
    if skip {
        if let Some(watch) = &state.watch {
            *stats = watch.stats;
        }
        return;
    }
    state.watch = None;
    // The system runs on a worker. A counter on the test thread would not see these buffers.
    let buffers = Cell::new(0u64);
    // Read once: a terrain that marks a cell takes it off this list, and every terrain over the
    // cell must still see it.
    let awaiting_all: Vec<(LevelId, CellCoord)> = gates.awaiting(gate).collect();
    let mut body_bits: BTreeMap<Entity, [u32; 5]> = BTreeMap::new();
    let mut levels: BTreeMap<Entity, Option<LevelId>> = BTreeMap::new();
    // Spawned cells every in-scope terrain has the tiles of (`false` once one does not).
    let mut verdict: BTreeMap<(LevelId, CellCoord), bool> = BTreeMap::new();
    let mut built = 0u64;
    let mut removed = 0u64;
    let (mut resident, mut pending, mut held_cells) = (0u32, 0u32, 0u32);

    for (entity, collider, settings, body, member, layer_pieces) in &terrains {
        let ColliderShape::HeightField {
            nx,
            nz,
            heights,
            scale,
        } = &collider.shape
        else {
            continue;
        };
        let Some(body) = body.map(RigidBodyHandle::to_handle) else {
            continue;
        };
        let Some(tiling) = Tiling::new(*nx, *nz, scale.to_array(), settings.tile_m) else {
            continue;
        };
        let level = member.map(LevelMember::level);
        let fresh = !state.terrains.contains_key(&entity);
        let rt = state.terrains.entry(entity).or_insert_with(|| TerrainRt {
            settings: *settings,
            body: Some(body),
            tiling,
            tiles: BTreeMap::new(),
            cells: BTreeMap::new(),
            marked_empty: BTreeSet::new(),
            pieces: Vec::new(),
        });
        // New heights or settings: every kept tile is rebuilt this step, outside the per-step
        // limit, so loaded cells never lose their ground. A new tiling also re-maps the cells.
        if rt.body != Some(body) {
            // The old body's removal took its tile colliders along: forget the dead handles
            // (`remove_tile` only cleans what is left) so the tiles are built on the new body
            // (corr-physics audit: they were kept and the terrain had no collision).
            for tile in rt.tiles.values_mut() {
                if let Some(handle) = tile.handle.take() {
                    remove_tile(&mut physics, handle);
                }
            }
            for handle in rt.pieces.iter_mut().filter_map(Option::take) {
                remove_tile(&mut physics, handle);
            }
            rt.body = Some(body);
        }
        let retile = !fresh && rt.tiling != tiling;
        let rebuild = !fresh && (collider.is_changed() || retile || rt.settings != *settings);
        rt.settings = *settings;
        if retile {
            for tile in std::mem::take(&mut rt.tiles).into_values() {
                if let Some(handle) = tile.handle {
                    remove_tile(&mut physics, handle);
                    removed += 1;
                }
            }
        }
        rt.tiling = tiling;
        if let Some(level) = level {
            gates.claim_level(gate, level);
        }

        // The field's center in world meters: the body plus the collider's offset. Tiles are laid
        // out on the world axes: a tiled terrain's body and offset are not rotated (module docs).
        let Some(body_pos) = physics
            .bodies
            .get(body)
            .map(|b| from_rapier_vec3(b.translation()))
        else {
            continue;
        };
        let (body_index, body_generation) = body.into_raw_parts();
        body_bits.insert(
            entity,
            [
                body_pos.x.to_bits(),
                body_pos.y.to_bits(),
                body_pos.z.to_bits(),
                body_index,
                body_generation,
            ],
        );
        levels.insert(entity, level);
        let center_local = body_pos + collider.offset.translation;
        let center = [
            f64::from(center_local.x) + offset[0],
            f64::from(center_local.z) + offset[2],
        ];
        let tiles_of = |rt: &TerrainRt, min: [f64; 2], max: [f64; 2]| -> Vec<[u32; 2]> {
            buffers.set(buffers.get().saturating_add(1));
            let (min, max) = (
                [min[0] - center[0], min[1] - center[1]],
                [max[0] - center[0], max[1] - center[1]],
            );
            let Some(([x0, x1], [z0, z1])) = rt.tiling.tiles_over(min, max) else {
                return Vec::new();
            };
            let mut out = Vec::new();
            for z in z0..=z1 {
                for x in x0..=x1 {
                    out.push([x, z]);
                }
            }
            out
        };
        let rect = |l: LevelId, cell: CellCoord| {
            layouts
                .as_ref()
                .and_then(|ls| ls.0.get(&l))
                .map(|layout| (layout.grid.cell_min(cell), layout.grid.cell_max(cell)))
        };
        if retile {
            // The same cells hold the tiles of the new tiling.
            let keys: Vec<(LevelId, CellCoord)> = rt.cells.keys().copied().collect();
            for (l, cell) in keys {
                let tiles = rect(l, cell).map_or_else(Vec::new, |(min, max)| tiles_of(&*rt, min, max));
                for t in &tiles {
                    rt.tiles
                        .entry(*t)
                        .or_default()
                        .holders
                        .insert(Holder::Cell(l, cell));
                }
                rt.cells.insert((l, cell), tiles);
            }
        }

        // Spawned cells take their tiles; unloaded ones let them go.
        let in_scope = |l: LevelId| level.is_none_or(|own| own == l);
        let awaiting: Vec<(LevelId, CellCoord)> = awaiting_all
            .iter()
            .copied()
            .filter(|(l, _)| in_scope(*l))
            .collect();
        for (l, cell) in &awaiting {
            if rt.cells.contains_key(&(*l, *cell)) {
                continue;
            }
            let Some((min, max)) = rect(*l, *cell) else {
                continue;
            };
            let tiles = tiles_of(&*rt, min, max);
            for t in &tiles {
                rt.tiles
                    .entry(*t)
                    .or_default()
                    .holders
                    .insert(Holder::Cell(*l, *cell));
            }
            rt.cells.insert((*l, *cell), tiles);
        }
        let unloaded: Vec<(LevelId, CellCoord)> = rt
            .cells
            .keys()
            .copied()
            .filter(|(l, c)| !gates.is_marked(gate, *l, *c) && !awaiting.contains(&(*l, *c)))
            .collect();
        for key in unloaded {
            for t in rt.cells.remove(&key).unwrap_or_default() {
                if let Some(tile) = rt.tiles.get_mut(&t) {
                    tile.holders.remove(&Holder::Cell(key.0, key.1));
                }
            }
        }

        // The sources hold the tiles around them.
        let radius = f64::from(settings.view_radius_m.max(0.0));
        let mut around: BTreeSet<[u32; 2]> = BTreeSet::new();
        if let Some(views) = &views
            && radius > 0.0
        {
            for view in &views.0 {
                let (x, z) = (view.pos[0], view.pos[2]);
                around.extend(tiles_of(&*rt, [x - radius, z - radius], [x + radius, z + radius]));
            }
        }
        for (t, tile) in &mut rt.tiles {
            if !around.contains(t) {
                tile.holders.remove(&Holder::Views);
            }
        }
        for t in &around {
            rt.tiles.entry(*t).or_default().holders.insert(Holder::Views);
        }

        // Unheld tiles go; held ones are built in tile order within the step's limit (a rebuild of
        // a resident tile is a swap in place and does not count).
        let limit = if settings.max_tiles_per_step == 0 {
            usize::MAX
        } else {
            settings.max_tiles_per_step as usize
        };
        let mut built_now = 0usize;
        let mut drop_tiles = Vec::new();
        for (t, tile) in &mut rt.tiles {
            if tile.holders.is_empty() {
                if let Some(handle) = tile.handle.take() {
                    remove_tile(&mut physics, handle);
                    removed += 1;
                }
                drop_tiles.push(*t);
                continue;
            }
            let swap = tile.handle.is_some() && rebuild;
            if tile.handle.is_some() && !swap {
                continue;
            }
            if tile.empty && !rebuild {
                continue;
            }
            if !swap && built_now >= limit {
                pending += 1;
                continue;
            }
            let source = TileSource {
                collider: &collider,
                tiling: &rt.tiling,
                heights: heights.as_slice(),
                body,
                entity,
            };
            let Some(handle) = build_tile(&mut physics, &layers, &source, *t) else {
                tile.empty = tile.handle.is_none();
                continue;
            };
            tile.empty = false;
            if let Some(old) = tile.handle.replace(handle) {
                remove_tile(&mut physics, old);
                removed += 1;
            } else {
                built_now += 1;
            }
            built += 1;
        }
        for t in drop_tiles {
            rt.tiles.remove(&t);
        }

        // Layer pieces follow the built tiles under their squares (spec 26 § 2a.8, ADR-8537).
        let source = TileSource {
            collider: &collider,
            tiling: &rt.tiling,
            heights: heights.as_slice(),
            body,
            entity,
        };
        // Pieces are not tiles: only a pending one shows in the tile counters (it keeps the step
        // from settling).
        let (_, _, piece_pending) = step_layer_pieces(
            &mut physics,
            &layers,
            &source,
            (&rt.tiles, &mut rt.pieces),
            layer_pieces.as_ref().map(|c| (&**c, rebuild || c.is_changed())),
            (&mut built_now, limit),
        );
        pending += piece_pending;

        let ready = |rt: &TerrainRt, tiles: &[[u32; 2]]| {
            tiles.iter().all(|t| {
                rt.tiles
                    .get(t)
                    .is_some_and(|tile| tile.handle.is_some() || tile.empty)
            })
        };
        // A spawned cell is marked once every in-scope terrain has its tiles (after the loop).
        for (key, tiles) in &rt.cells {
            let ok = ready(&*rt, tiles);
            verdict.entry(*key).and_modify(|v| *v &= ok).or_insert(ok);
        }
        // A cell with no content is marked while the sources keep its tiles.
        let mut empty_now: BTreeSet<(LevelId, CellCoord)> = BTreeSet::new();
        if let Some(layouts) = &layouts
            && let Some(views) = &views
            && radius > 0.0
        {
            for (l, layout) in layouts.0.iter().filter(|(l, _)| in_scope(**l)) {
                for view in &views.0 {
                    let (x, z) = (view.pos[0], view.pos[2]);
                    let (Ok(lo), Ok(hi)) = (
                        layout.grid.cell_of(x - radius, z - radius),
                        layout.grid.cell_of(x + radius, z + radius),
                    ) else {
                        continue;
                    };
                    let [b0, b1] = layout.bounds;
                    for cx in lo.x.max(b0.x)..=hi.x.min(b1.x) {
                        for cz in lo.z.max(b0.z)..=hi.z.min(b1.z) {
                            let cell = CellCoord::new(cx, cz);
                            if layout.records.contains(&cell) || empty_now.contains(&(*l, cell)) {
                                continue;
                            }
                            let tiles =
                                tiles_of(&*rt, layout.grid.cell_min(cell), layout.grid.cell_max(cell));
                            if ready(&*rt, &tiles) {
                                empty_now.insert((*l, cell));
                            }
                        }
                    }
                }
            }
        }
        for key in rt.marked_empty.difference(&empty_now) {
            gates.clear(gate, key.0, key.1);
        }
        for key in empty_now.difference(&rt.marked_empty) {
            gates.set_ready(gate, key.0, key.1);
        }
        rt.marked_empty = empty_now;

        resident += rt.tiles.values().filter(|t| t.handle.is_some()).count() as u32;
        held_cells += rt.cells.len() as u32;
    }
    for ((l, cell), ok) in verdict {
        if ok && !gates.is_marked(gate, l, cell) {
            gates.set_ready(gate, l, cell);
        }
    }
    state.index_buffers = state.index_buffers.saturating_add(buffers.get());
    *stats = TerrainTileStats {
        terrains: state.terrains.len() as u32,
        resident_tiles: resident,
        pending_tiles: pending,
        built: stats.built + built,
        removed: stats.removed + removed,
        held_cells,
    };
    // A budget-capped step has not finished. Storing the watch would skip the rest.
    if pending == 0 {
        // Revision is read after this step's marks, so the next unchanged step can skip.
        state.watch = Some(TileWatch {
            views: views.as_ref().map(|v| (*v).clone()),
            layouts: layouts.as_ref().map(|v| (*v).clone()),
            origin: offset,
            revision: gates.revision(),
            // The list after this step's marks, so the next quiet step compares equal.
            awaiting: gates.awaiting(gate).collect(),
            body_bits,
            levels,
            stats: *stats,
        });
    }
}

/// What tiles are cut from: the terrain's collider, its tiling and heights, its body and entity.
struct TileSource<'a> {
    collider: &'a Collider,
    tiling: &'a Tiling,
    heights: &'a [f32],
    body: rapier3d::dynamics::RigidBodyHandle,
    entity: Entity,
}

/// Builds tile `t` of a terrain's heightfield as a collider on the terrain's body.
fn build_tile(
    physics: &mut PhysicsWorld,
    layers: &PhysicsLayers,
    source: &TileSource<'_>,
    t: [u32; 2],
) -> Option<RapierColliderHandle> {
    let TileSource {
        collider,
        tiling,
        heights,
        body,
        entity,
    } = *source;
    let (shape, center) = tiling.cut(heights, t)?;
    // The cut already copied this tile's window. Cloning `collider` would copy the whole
    // heightfield and drop it on the next line.
    let tile = Collider {
        shape,
        offset: Transform {
            translation: collider.offset.translation + collider.offset.rotation * center,
            ..collider.offset
        },
        sensor: collider.sensor,
        friction: collider.friction,
        restitution: collider.restitution,
        density: collider.density,
        friction_combine: collider.friction_combine,
        restitution_combine: collider.restitution_combine,
        layer: collider.layer,
        active_events: collider.active_events,
        contact_force_threshold: collider.contact_force_threshold,
    };
    // Out-of-domain material numbers are substituted here as for any collider; not counted,
    // as every tile of one terrain would count the same field again.
    let (builder, _) = crate::step::build_rapier_collider(&tile, layers)?;
    let PhysicsWorld {
        bodies,
        colliders,
        collider_entities,
        ..
    } = physics;
    let handle = colliders.insert_with_parent(builder.build(), body, bodies);
    // A hit on a tile is a hit on the terrain.
    collider_entities.insert(handle, entity);
    Some(handle)
}

/// Builds, keeps or removes the [`TerrainLayerColliders`] pieces of one terrain after its tiles:
/// a piece is wanted while a built tile overlaps its square (any built tile without a square). New
/// pieces count toward the step's limit like tiles. `pieces` is the component and whether it (or
/// the heights) changed this step; `None` removes every piece. Returns `(built, removed, pending)`.
fn step_layer_pieces(
    physics: &mut PhysicsWorld,
    layers: &PhysicsLayers,
    source: &TileSource<'_>,
    (tiles, handles): (&BTreeMap<[u32; 2], Tile>, &mut Vec<Option<RapierColliderHandle>>),
    pieces: Option<(&TerrainLayerColliders, bool)>,
    (built_now, limit): (&mut usize, usize),
) -> (u64, u64, u32) {
    let (mut built, mut removed, mut pending) = (0u64, 0u64, 0u32);
    let count = pieces.map_or(0, |(p, _)| p.pieces.len());
    if pieces.is_none_or(|(_, changed)| changed) || handles.len() != count {
        for handle in handles.drain(..).flatten() {
            remove_tile(physics, handle);
            removed += 1;
        }
        handles.resize(count, None);
    }
    let Some((component, _)) = pieces else {
        return (built, removed, pending);
    };
    let built_tile = |t: &[u32; 2]| tiles.get(t).is_some_and(|tile| tile.handle.is_some());
    for (piece, handle) in component.pieces.iter().zip(handles.iter_mut()) {
        let wanted = match piece.rect {
            None => tiles.values().any(|t| t.handle.is_some()),
            Some([x, z, size]) => source
                .tiling
                .tiles_over(
                    [f64::from(x), f64::from(z)],
                    [f64::from(x + size), f64::from(z + size)],
                )
                .is_some_and(|([x0, x1], [z0, z1])| {
                    (z0..=z1).any(|tz| (x0..=x1).any(|tx| built_tile(&[tx, tz])))
                }),
        };
        match (wanted, handle.is_some()) {
            (true, false) => {
                if *built_now >= limit {
                    pending += 1;
                    continue;
                }
                if let Some(h) = build_piece(
                    physics,
                    layers,
                    (source.collider, source.body, source.entity),
                    &piece.triangles,
                ) {
                    *handle = Some(h);
                    *built_now += 1;
                    built += 1;
                }
            }
            (false, true) => {
                if let Some(h) = handle.take() {
                    remove_tile(physics, h);
                    removed += 1;
                }
            }
            _ => {}
        }
    }
    (built, removed, pending)
}

/// Keeps the layer colliders of untiled terrains (ADR-8872): one `TriMesh` per layer on the
/// terrain's body, rebuilt when the component, the collider or the body changes, removed when the
/// component, the body or the entity goes (or the terrain becomes tiled).
fn step_untiled_pieces(
    state: &mut BTreeMap<Entity, UntiledRt>,
    physics: &mut PhysicsWorld,
    layers: &PhysicsLayers,
    untiled: &Query<UntiledTerrain, Without<TerrainTileCollision>>,
) {
    let _span = tracing::info_span!(target: "regia_physics", "untiled_layer_pieces").entered();
    // Gone: no entity, no component, or tiled now. Dead handles are a no-op in `remove_tile`.
    if state.keys().any(|e| untiled.get(*e).is_err()) {
        let gone: Vec<Entity> = state
            .keys()
            .copied()
            .filter(|e| untiled.get(*e).is_err())
            .collect();
        for entity in gone {
            if let Some(rt) = state.remove(&entity) {
                for handle in rt.handles {
                    remove_tile(physics, handle);
                }
            }
        }
    }
    // A terrain whose pieces, collider or body changed (or that has none built yet) is built again.
    let fresh = |state: &BTreeMap<Entity, UntiledRt>,
                 (entity, collider, body, pieces): &(
        Entity,
        Ref<'_, Collider>,
        Option<&RigidBodyHandle>,
        Ref<'_, TerrainLayerColliders>,
    )| {
        match (state.get(entity), body.map(|b| b.to_handle())) {
            (Some(rt), Some(b)) => rt.body != b || pieces.is_changed() || collider.is_changed(),
            _ => true,
        }
    };
    // The settled step allocates nothing.
    if !untiled.iter().any(|row| fresh(state, &row)) {
        return;
    }
    // Entity order, so rapier hands out the same handles on every run.
    let mut rows: Vec<_> = untiled.iter().filter(|row| fresh(state, row)).collect();
    rows.sort_unstable_by_key(|row| row.0);
    for (entity, collider, body, pieces) in rows {
        let body = body.map(RigidBodyHandle::to_handle);
        if let Some(rt) = state.remove(&entity) {
            for handle in rt.handles {
                remove_tile(physics, handle);
            }
        }
        let Some(body) = body.filter(|b| physics.bodies.get(*b).is_some()) else {
            continue;
        };
        let mut by_layer: BTreeMap<u16, Vec<[[f32; 3]; 3]>> = BTreeMap::new();
        for piece in &pieces.pieces {
            by_layer
                .entry(piece.layer)
                .or_default()
                .extend_from_slice(&piece.triangles);
        }
        let handles = by_layer
            .values()
            .filter_map(|triangles| build_piece(physics, layers, (&*collider, body, entity), triangles))
            .collect();
        state.insert(entity, UntiledRt { body, handles });
    }
}

/// Builds one layer piece as a `TriMesh` collider on the terrain's body, in the field's frame:
/// `(collider, body, entity)` are the terrain's heightfield collider (its offset and material),
/// its body and the entity a hit reports.
fn build_piece(
    physics: &mut PhysicsWorld,
    layers: &PhysicsLayers,
    (collider, body, entity): (&Collider, rapier3d::dynamics::RigidBodyHandle, Entity),
    triangles: &[[[f32; 3]; 3]],
) -> Option<RapierColliderHandle> {
    let vertices: Vec<[f32; 3]> = triangles.iter().flatten().copied().collect();
    let indices: Vec<u32> = (0..vertices.len() as u32).collect();
    let shape = ColliderShape::trimesh_from_triangles(&vertices, &indices)?;
    let piece = Collider {
        shape,
        offset: collider.offset,
        sensor: collider.sensor,
        friction: collider.friction,
        restitution: collider.restitution,
        density: collider.density,
        friction_combine: collider.friction_combine,
        restitution_combine: collider.restitution_combine,
        layer: collider.layer,
        active_events: collider.active_events,
        contact_force_threshold: collider.contact_force_threshold,
    };
    let (builder, _) = crate::step::build_rapier_collider(&piece, layers)?;
    let PhysicsWorld {
        bodies,
        colliders,
        collider_entities,
        ..
    } = physics;
    let handle = colliders.insert_with_parent(builder.build(), body, bodies);
    collider_entities.insert(handle, entity);
    Some(handle)
}

/// Removes a tile collider. Like `PhysicsWorld::remove_collider`, it keeps the tile's entity and
/// sensor flag for the next step, whose `Stopped` pairs of this collider the event flush must
/// still map to the terrain (a body resting on an unloaded tile gets its `CollisionEnded`).
fn remove_tile(physics: &mut PhysicsWorld, handle: RapierColliderHandle) {
    let PhysicsWorld {
        bodies,
        colliders,
        islands,
        soft_bodies,
        collider_entities,
        removed_awaiting_step,
        ..
    } = physics;
    let sensor = colliders.get(handle).map(|c| c.is_sensor());
    if colliders.contains(handle) {
        colliders.remove(handle, islands, bodies, soft_bodies, true);
    }
    if let Some(entity) = collider_entities.remove(&handle)
        && let Some(sensor) = sensor
    {
        removed_awaiting_step.insert(handle, (entity, sensor));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiling(n: usize, width: f32, tile_m: f32) -> Tiling {
        Tiling::new(n, n, [width, 1.0, width], tile_m).expect("tiling")
    }

    #[test]
    fn tiles_cover_the_field_and_share_their_borders() {
        // 9 x 9 samples over 64 m: 8 m cells, 2 cells (16 m) per tile -> 4 x 4 tiles.
        let t = tiling(9, 64.0, 16.0);
        assert_eq!(t.k, 2);
        assert_eq!(t.tiles, [4, 4]);
        let heights: Vec<f32> = (0..81).map(|i| i as f32).collect();
        let (a, ca) = t.cut(&heights, [0, 0]).expect("tile");
        let (b, cb) = t.cut(&heights, [1, 0]).expect("tile");
        let (
            ColliderShape::HeightField {
                heights: ha,
                nx,
                nz,
                scale,
            },
            ColliderShape::HeightField { heights: hb, .. },
        ) = (a, b)
        else {
            panic!("heightfields");
        };
        assert_eq!((nx, nz), (3, 3));
        assert_eq!(scale, Vec3::new(16.0, 1.0, 16.0));
        // Tile (0, 0)'s last column is tile (1, 0)'s first: the pieces meet.
        assert_eq!(ha[6..9], hb[0..3]);
        assert_eq!(ca, Vec3::new(-24.0, 0.0, -24.0));
        assert_eq!(cb, Vec3::new(-8.0, 0.0, -24.0));
        assert!(t.cut(&heights, [4, 0]).is_none());
    }

    #[test]
    fn a_rectangle_maps_to_the_tiles_under_it() {
        let t = tiling(9, 64.0, 16.0);
        // The field spans [-32, 32]; tiles are 16 m.
        assert_eq!(
            t.tiles_over([-32.0, -32.0], [-17.0, -17.0]),
            Some(([0, 0], [0, 0]))
        );
        assert_eq!(t.tiles_over([-20.0, 0.0], [5.0, 31.0]), Some(([0, 2], [2, 3])));
        // Past the edge clamps; outside misses.
        assert_eq!(t.tiles_over([20.0, 20.0], [90.0, 90.0]), Some(([3, 3], [3, 3])));
        assert_eq!(t.tiles_over([40.0, 0.0], [50.0, 1.0]), None);
    }

    #[test]
    fn a_bad_field_has_no_tiling() {
        assert!(Tiling::new(1, 9, [64.0, 1.0, 64.0], 16.0).is_none());
        assert!(Tiling::new(9, 9, [f32::NAN, 1.0, 64.0], 16.0).is_none());
        // A tile wider than the field is one tile.
        assert_eq!(tiling(9, 64.0, 1000.0).tiles, [1, 1]);
        // A non-finite tile size falls back to 128 m.
        assert_eq!(tiling(9, 64.0, f32::NAN).tiles, [1, 1]);
    }

    /// A physics app with one 256 m tiled terrain (flat at y = 1) and a view at (-100, -100).
    fn tiled_terrain_app(guid: u128) -> (regia_app::App, Entity) {
        use std::time::Duration;

        use regia_app::{App, FixedTime, TimeUpdateStrategy};
        use regia_core::id::Guid;
        use regia_scene::stream_plan::{SourceId, SourceView};
        use regia_scene::streaming::StreamLevelLayout;

        const N: usize = 65;
        const WIDTH: f32 = 256.0;
        let level = LevelId(Guid::from_u128(guid));
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed).add_plugins(crate::PhysicsPlugin);
        app.init_resource::<StreamGates>();
        app.init_resource::<StreamViews>();
        let mut layouts = StreamLevelLayouts::default();
        layouts.0.insert(
            level,
            StreamLevelLayout {
                grid: regia_scene::stream_grid::StreamGrid::new(64.0).expect("grid"),
                bounds: [CellCoord::new(-4, -4), CellCoord::new(3, 3)],
                records: BTreeSet::new(),
            },
        );
        app.insert_resource(layouts);
        let terrain = app
            .world_mut()
            .spawn((
                regia_scene::Transform::IDENTITY,
                crate::RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::HeightField {
                        nx: N,
                        nz: N,
                        heights: vec![1.0; N * N],
                        scale: Vec3::new(WIDTH, 1.0, WIDTH),
                    },
                    ..Collider::default()
                },
                TerrainTileCollision {
                    tile_m: 64.0,
                    view_radius_m: 80.0,
                    max_tiles_per_step: 0,
                },
                LevelMember::from_level(level),
            ))
            .id();
        app.world_mut().resource_mut::<StreamViews>().0 =
            vec![SourceView::at(SourceId::Pin(1), [-100.0, 0.0, -100.0])];
        (app, terrain)
    }

    /// Corr-physics audit: a tiled terrain whose body was rebuilt (its layer parked and
    /// unparked, or `RigidBody` removed and re-added) kept the tile handles the old body took
    /// along, so no tile was rebuilt and the terrain had no collision while the stats counted it
    /// resident.
    #[test]
    fn a_rebuilt_terrain_body_gets_its_tiles_back() {
        for park in [false, true] {
            let (mut app, terrain) = tiled_terrain_app(0x71E6);
            for _ in 0..4 {
                app.update();
            }
            let live_tiles = |app: &regia_app::App| {
                let physics = app.world().resource::<PhysicsWorld>();
                let tiles = app.world().resource::<TerrainTiles>();
                let rt = tiles.terrains.get(&terrain).expect("terrain state");
                let handles: Vec<_> = rt.tiles.values().filter_map(|t| t.handle).collect();
                let live = handles.iter().filter(|h| physics.colliders.contains(**h)).count();
                (handles.len(), live)
            };
            let (held, live) = live_tiles(&app);
            assert!(
                held >= 1 && live == held,
                "park {park}: tiles before: {held} held, {live} live"
            );
            if park {
                app.world_mut()
                    .entity_mut(terrain)
                    .insert(regia_scene::layers::LayerParked {
                        restore: regia_scene::components::Visibility::default(),
                        inherited: false,
                    });
            } else {
                app.world_mut().entity_mut(terrain).remove::<crate::RigidBody>();
            }
            for _ in 0..2 {
                app.update();
            }
            if park {
                app.world_mut()
                    .entity_mut(terrain)
                    .remove::<regia_scene::layers::LayerParked>();
            } else {
                app.world_mut()
                    .entity_mut(terrain)
                    .insert(crate::RigidBody::static_body());
            }
            for _ in 0..3 {
                app.update();
            }
            let (held, live) = live_tiles(&app);
            assert!(
                held >= 1 && live == held,
                "park {park}: after the rebuild {held} tiles held, {live} live"
            );
        }
    }

    /// Corr-physics audit: a tiled terrain has tile colliders but no single collider handle, so
    /// `contact_sample` and `collider_body_motion` answered `None` for every contact with it (no
    /// impact or rolling sounds on streamed ground).
    #[test]
    fn contact_queries_answer_for_a_tiled_terrain() {
        use crate::contact_query::{collider_body_motion, contact_sample};
        let (mut app, terrain) = tiled_terrain_app(0x71E7);
        let crate_box = app
            .world_mut()
            .spawn((
                regia_scene::Transform::from_translation(Vec3::new(-100.0, 1.6, -100.0)),
                crate::RigidBody::dynamic(),
                Collider::default(),
            ))
            .id();
        for _ in 0..60 {
            app.update();
        }
        let physics = app.world().resource::<PhysicsWorld>();
        let motion = collider_body_motion(physics, terrain).expect("the terrain's body moves");
        assert!(!motion.dynamic);
        let sample = contact_sample(physics, terrain, crate_box).expect("the box rests on the terrain");
        assert!(
            sample.normal.y > 0.9,
            "terrain → box points up: {:?}",
            sample.normal
        );
        let box_y = physics.bodies[physics.body_handles[&crate_box]].translation().y;
        assert!(
            (sample.point.y - (box_y - 0.5)).abs() < 0.05,
            "touching under the box at {box_y}: {:?}",
            sample.point
        );
        let back = contact_sample(physics, crate_box, terrain).expect("either order");
        assert!(
            back.normal.y < -0.9,
            "box → terrain points down: {:?}",
            back.normal
        );
        assert_eq!(back.motion_b.body, Some(terrain));
    }

    /// After the view is still and the tiles exist, further fixed steps fill no tile-index
    /// buffers. Moving the view by more than the radius still builds the tiles that enter it.
    #[test]
    fn settled_tiles_do_not_allocate_per_step() {
        use std::time::Duration;

        use regia_app::{App, FixedTime, TimeUpdateStrategy};
        use regia_core::id::Guid;
        use regia_scene::stream_plan::{SourceId, SourceView};
        use regia_scene::streaming::StreamLevelLayout;

        const N: usize = 65;
        const WIDTH: f32 = 256.0;
        let level = LevelId(Guid::from_u128(0x71E5));
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed).add_plugins(crate::PhysicsPlugin);
        app.init_resource::<StreamGates>();
        app.init_resource::<StreamViews>();
        let mut layouts = StreamLevelLayouts::default();
        layouts.0.insert(
            level,
            StreamLevelLayout {
                grid: regia_scene::stream_grid::StreamGrid::new(64.0).expect("grid"),
                bounds: [CellCoord::new(-4, -4), CellCoord::new(3, 3)],
                records: BTreeSet::new(),
            },
        );
        app.insert_resource(layouts);
        app.world_mut().spawn((
            regia_scene::Transform::IDENTITY,
            crate::RigidBody::static_body(),
            Collider {
                shape: ColliderShape::HeightField {
                    nx: N,
                    nz: N,
                    heights: vec![1.0; N * N],
                    scale: Vec3::new(WIDTH, 1.0, WIDTH),
                },
                ..Collider::default()
            },
            TerrainTileCollision {
                tile_m: 64.0,
                view_radius_m: 80.0,
                max_tiles_per_step: 0,
            },
            LevelMember::from_level(level),
        ));
        let place = |app: &mut App, x: f64, z: f64| {
            app.world_mut().resource_mut::<StreamViews>().0 =
                vec![SourceView::at(SourceId::Pin(1), [x, 0.0, z])];
        };
        place(&mut app, -100.0, -100.0);
        for _ in 0..4 {
            app.update();
        }
        let ready = app.world().resource::<TerrainTileStats>().resident_tiles;
        assert!(ready >= 1, "the view holds ground tiles, resident {ready}");
        app.world_mut().resource_mut::<TerrainTiles>().index_buffers = 0;
        for _ in 0..3 {
            app.update();
        }
        let filled = app.world().resource::<TerrainTiles>().index_buffers;
        assert_eq!(
            filled, 0,
            "a still view filled {filled} tile-index buffers over 3 settled steps"
        );
        assert_eq!(
            app.world().resource::<TerrainTileStats>().resident_tiles,
            ready,
            "a still view keeps the same tiles"
        );

        place(&mut app, 100.0, 100.0);
        app.world_mut().resource_mut::<TerrainTiles>().index_buffers = 0;
        for _ in 0..4 {
            app.update();
        }
        let moved = app.world().resource::<TerrainTileStats>().resident_tiles;
        let walked = app.world().resource::<TerrainTiles>().index_buffers;
        assert!(
            moved >= 1,
            "the moved view builds the tiles that enter its radius"
        );
        assert!(
            walked > 0 || moved != ready,
            "moving the view must retile (walked {walked} buffers, resident {ready} -> {moved})"
        );
    }

    /// corr-low: `tile_watch_holds` compared the body, the settings, the collider tick and the
    /// views, but not `LevelMember`: a terrain moved to another level after a settled step kept
    /// claiming the old level's cells (and never the new one's) until something else changed.
    #[test]
    fn a_level_member_change_claims_the_new_level_without_waiting_for_another_change() {
        use std::time::Duration;

        use regia_app::{App, FixedTime, TimeUpdateStrategy};
        use regia_core::id::Guid;
        use regia_scene::stream_plan::{SourceId, SourceView};
        use regia_scene::streaming::StreamLevelLayout;

        const N: usize = 65;
        const WIDTH: f32 = 256.0;
        let level_a = LevelId(Guid::from_u128(0xA1));
        let level_b = LevelId(Guid::from_u128(0xB2));
        let cell = CellCoord::new(0, 0);
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed).add_plugins(crate::PhysicsPlugin);
        app.init_resource::<StreamGates>();
        app.init_resource::<StreamViews>();
        let mut layouts = StreamLevelLayouts::default();
        for level in [level_a, level_b] {
            let mut records = BTreeSet::new();
            records.insert(cell);
            layouts.0.insert(
                level,
                StreamLevelLayout {
                    grid: regia_scene::stream_grid::StreamGrid::new(64.0).expect("grid"),
                    bounds: [CellCoord::new(-1, -1), CellCoord::new(1, 1)],
                    records,
                },
            );
        }
        app.insert_resource(layouts);
        let terrain = app
            .world_mut()
            .spawn((
                regia_scene::Transform::IDENTITY,
                crate::RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::HeightField {
                        nx: N,
                        nz: N,
                        heights: vec![1.0; N * N],
                        scale: Vec3::new(WIDTH, 1.0, WIDTH),
                    },
                    ..Collider::default()
                },
                TerrainTileCollision {
                    tile_m: 64.0,
                    view_radius_m: 80.0,
                    max_tiles_per_step: 0,
                },
                LevelMember::from_level(level_a),
            ))
            .id();
        app.world_mut().resource_mut::<StreamViews>().0 =
            vec![SourceView::at(SourceId::Pin(1), [32.0, 0.0, 32.0])];
        // One step so the plugin registers its stream gate; `note_spawned` before any gate
        // exists would go nowhere.
        app.update();
        app.world_mut()
            .resource_mut::<StreamGates>()
            .note_spawned(level_a, cell);
        for _ in 0..4 {
            app.update();
        }
        assert_eq!(
            app.world().resource::<TerrainTileStats>().held_cells,
            1,
            "the terrain holds its own level's spawned cell"
        );

        // The entity moves to level B. The next step claims level B for the terrain gate (the
        // watch sees the member change); only then does level B's cell spawn — the streaming
        // runtime notes spawned cells of claimed levels.
        app.world_mut()
            .entity_mut(terrain)
            .insert(LevelMember::from_level(level_b));
        app.update();
        app.world_mut()
            .resource_mut::<StreamGates>()
            .note_spawned(level_b, cell);
        for _ in 0..2 {
            app.update();
        }
        assert_eq!(
            app.world().resource::<TerrainTileStats>().held_cells,
            2,
            "the terrain now also holds level B's spawned cell; the old level's marked cell \
             stays held while it is spawned"
        );
    }

    /// Plan W-17 item 5 (spec 26 § 2a.8, ADR-8537): layer pieces on a tiled terrain exist while a
    /// built tile is under their square, sit on the terrain's body (hits report the terrain) and go
    /// when their tiles go or the component is removed. A terrain without pieces steps exactly as
    /// one with an empty component (the same counters and gate marks each step).
    #[test]
    fn colliders_follow_the_tiles() {
        use crate::queries::{QueryFilter, WorldQueries};
        use regia_scene::stream_plan::{SourceId, SourceView};

        // A floor quad 5 m down, 20 m wide, centred on `c`.
        let floor = |c: [f32; 2]| -> Vec<[[f32; 3]; 3]> {
            let (a, b) = ([c[0] - 10.0, c[1] - 10.0], [c[0] + 10.0, c[1] + 10.0]);
            vec![
                [[a[0], -5.0, a[1]], [b[0], -5.0, a[1]], [b[0], -5.0, b[1]]],
                [[a[0], -5.0, a[1]], [b[0], -5.0, b[1]], [a[0], -5.0, b[1]]],
            ]
        };
        let pieces = TerrainLayerColliders {
            pieces: vec![
                TerrainColliderPiece {
                    layer: 0,
                    rect: Some([-128.0, -128.0, 64.0]),
                    triangles: floor([-100.0, -100.0]),
                },
                TerrainColliderPiece {
                    layer: 0,
                    rect: Some([64.0, 64.0, 64.0]),
                    triangles: floor([100.0, 100.0]),
                },
            ],
        };
        let (mut app, terrain) = tiled_terrain_app(0x71EA);
        // Holes in the heightfield over both floors (samples every 4 m from -128 m).
        {
            let mut collider = app.world_mut().get_mut::<Collider>(terrain).expect("collider");
            if let ColliderShape::HeightField { nx, heights, .. } = &mut collider.shape {
                let n = *nx;
                for c in 0..n {
                    for r in 0..n {
                        let (x, z) = (-128.0 + 4.0 * c as f32, -128.0 + 4.0 * r as f32);
                        let near = |v: f32, at: f32| (v - at).abs() <= 16.0;
                        if (near(x, -100.0) && near(z, -100.0)) || (near(x, 100.0) && near(z, 100.0)) {
                            heights[r + c * n] = f32::NAN;
                        }
                    }
                }
            }
        }
        app.world_mut().entity_mut(terrain).insert(pieces);
        let down = |app: &regia_app::App, x: f32, z: f32| {
            let world = app.world().resource::<PhysicsWorld>();
            WorldQueries { world, layers: None }.raycast(
                Vec3::new(x, 10.0, z),
                Vec3::NEG_Y,
                50.0,
                QueryFilter::new(),
            )
        };
        for _ in 0..4 {
            app.update();
        }
        // The view at (-100, -100) holds the tiles under piece 0, not piece 1's.
        let hit = down(&app, -100.0, -100.0).expect("piece 0 under its held tile");
        assert_eq!(hit.entity, terrain, "a piece hit reports the terrain");
        assert!(
            (hit.distance - 15.0).abs() < 1e-3,
            "the floor 5 m down: {}",
            hit.distance
        );
        assert!(down(&app, 100.0, 100.0).is_none(), "piece 1's tile is not held");
        // The view moves: piece 1 comes with its tile, piece 0 goes with its tile.
        app.world_mut().resource_mut::<StreamViews>().0 =
            vec![SourceView::at(SourceId::Pin(1), [100.0, 0.0, 100.0])];
        for _ in 0..4 {
            app.update();
        }
        assert!((down(&app, 100.0, 100.0).expect("piece 1").distance - 15.0).abs() < 1e-3);
        assert!(down(&app, -100.0, -100.0).is_none(), "piece 0 left with its tile");
        // Removing the component removes every piece.
        app.world_mut()
            .entity_mut(terrain)
            .remove::<TerrainLayerColliders>();
        for _ in 0..2 {
            app.update();
        }
        assert!(down(&app, 100.0, 100.0).is_none(), "no component, no pieces");
        let tiles = app.world().resource::<TerrainTiles>();
        assert!(tiles.terrains.get(&terrain).expect("terrain").pieces.is_empty());

        // Without pieces the tiles step as with an empty component: same counters, same marks.
        let run = |pieces: Option<TerrainLayerColliders>| {
            let (mut app, terrain) = tiled_terrain_app(0x71EB);
            if let Some(p) = pieces {
                app.world_mut().entity_mut(terrain).insert(p);
            }
            let mut trace = Vec::new();
            for step in 0..8 {
                if step == 4 {
                    app.world_mut().resource_mut::<StreamViews>().0 =
                        vec![SourceView::at(SourceId::Pin(1), [60.0, 0.0, -20.0])];
                }
                app.update();
                let stats = *app.world().resource::<TerrainTileStats>();
                let revision = app.world().resource::<StreamGates>().revision();
                trace.push((stats, revision));
            }
            trace
        };
        assert_eq!(run(None), run(Some(TerrainLayerColliders::default())));
    }

    /// ADR-8872 (spec 26 § 2a.8 "Without tiling, one trimesh per layer"): an untiled terrain with
    /// [`TerrainLayerColliders`] gets one `TriMesh` per layer on its body, whatever the views hold;
    /// hits report the terrain; a changed component rebuilds them; a removed one (or the terrain
    /// turning tiled) removes them. The tile counters do not see them.
    #[test]
    fn an_untiled_terrain_takes_one_trimesh_per_layer() {
        use std::time::Duration;

        use crate::queries::{QueryFilter, WorldQueries};
        use regia_app::{App, FixedTime, TimeUpdateStrategy};

        const N: usize = 65;
        // A floor quad `depth` m down, 20 m wide, centred on `c`.
        let floor = |c: [f32; 2], depth: f32| -> Vec<[[f32; 3]; 3]> {
            let (a, b) = ([c[0] - 10.0, c[1] - 10.0], [c[0] + 10.0, c[1] + 10.0]);
            vec![
                [[a[0], -depth, a[1]], [b[0], -depth, a[1]], [b[0], -depth, b[1]]],
                [[a[0], -depth, a[1]], [b[0], -depth, b[1]], [a[0], -depth, b[1]]],
            ]
        };
        let piece = |layer: u16, c: [f32; 2], depth: f32| TerrainColliderPiece {
            layer,
            rect: Some([c[0] - 32.0, c[1] - 32.0, 64.0]),
            triangles: floor(c, depth),
        };
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed).add_plugins(crate::PhysicsPlugin);
        // Holes in the heightfield over the three floors (samples every 4 m from -128 m).
        let mut heights = vec![1.0; N * N];
        for c in 0..N {
            for r in 0..N {
                let (x, z) = (-128.0 + 4.0 * c as f32, -128.0 + 4.0 * r as f32);
                let near = |v: f32, at: f32| (v - at).abs() <= 16.0;
                if [(-100.0, -100.0), (100.0, 100.0), (100.0, -100.0)]
                    .iter()
                    .any(|&(px, pz)| near(x, px) && near(z, pz))
                {
                    heights[r + c * N] = f32::NAN;
                }
            }
        }
        let terrain = app
            .world_mut()
            .spawn((
                regia_scene::Transform::IDENTITY,
                crate::RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::HeightField {
                        nx: N,
                        nz: N,
                        heights,
                        scale: Vec3::new(256.0, 1.0, 256.0),
                    },
                    ..Collider::default()
                },
                TerrainLayerColliders {
                    pieces: vec![
                        piece(0, [-100.0, -100.0], 5.0),
                        piece(1, [100.0, -100.0], 7.0),
                        piece(0, [100.0, 100.0], 5.0),
                    ],
                },
            ))
            .id();
        let down = |app: &App, x: f32, z: f32| {
            let world = app.world().resource::<PhysicsWorld>();
            WorldQueries { world, layers: None }.raycast(
                Vec3::new(x, 10.0, z),
                Vec3::NEG_Y,
                50.0,
                QueryFilter::new(),
            )
        };
        let body_colliders = |app: &App| {
            let handle = app
                .world()
                .get::<RigidBodyHandle>(terrain)
                .expect("body")
                .to_handle();
            let world = app.world().resource::<PhysicsWorld>();
            world.bodies.get(handle).expect("rapier body").colliders().len()
        };
        for _ in 0..4 {
            app.update();
        }
        for (x, z, depth) in [(-100.0, -100.0, 5.0), (100.0, 100.0, 5.0), (100.0, -100.0, 7.0)] {
            let hit = down(&app, x, z).unwrap_or_else(|| panic!("a layer floor under ({x}, {z})"));
            assert_eq!(hit.entity, terrain, "a piece hit reports the terrain");
            assert!(
                (hit.distance - (10.0 + depth)).abs() < 1e-3,
                "the floor {depth} m down at ({x}, {z}): {}",
                hit.distance
            );
        }
        assert_eq!(
            body_colliders(&app),
            3,
            "the heightfield plus one trimesh per layer (2 layers)"
        );
        assert_eq!(
            *app.world().resource::<TerrainTileStats>(),
            TerrainTileStats::default(),
            "untiled pieces are not tiles"
        );
        // A changed component rebuilds: layer 1 moves 2 m deeper.
        app.world_mut()
            .get_mut::<TerrainLayerColliders>(terrain)
            .expect("pieces")
            .pieces[1] = piece(1, [100.0, -100.0], 9.0);
        for _ in 0..2 {
            app.update();
        }
        let hit = down(&app, 100.0, -100.0).expect("layer 1 rebuilt");
        assert!(
            (hit.distance - 19.0).abs() < 1e-3,
            "rebuilt 9 m down: {}",
            hit.distance
        );
        assert_eq!(body_colliders(&app), 3, "the old pieces went");
        // Removing the component removes every piece.
        app.world_mut()
            .entity_mut(terrain)
            .remove::<TerrainLayerColliders>();
        for _ in 0..2 {
            app.update();
        }
        assert!(down(&app, -100.0, -100.0).is_none(), "no component, no pieces");
        assert_eq!(body_colliders(&app), 1, "only the heightfield is left");
        assert!(app.world().resource::<TerrainTiles>().untiled.is_empty());
    }
}
