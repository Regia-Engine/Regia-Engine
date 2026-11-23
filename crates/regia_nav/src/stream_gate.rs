// SPDX-License-Identifier: MPL-2.0
//! Navigation's hook into world streaming (`docs/specs/78-world-streaming.md` § 13.2): the `nav`
//! gate. When a streamed cell spawns with cooked nav tiles (`CellLoaded::nav`), they load into the
//! streamed [`NavTileCache`]; the cell is `sim_ready` for nav once its tiles are built. When it
//! unloads, its tiles leave the cache. A cell with no nav tiles passes the gate at once.
//!
//! The hook is opt-in: add [`NavStreamPlugin`] to an app whose streamed levels carry nav tiles. It
//! builds the streamed cache ([`NavTileCache::streamed`]) from the grid the level's world index names
//! ([`StreamNavLayout`]), and adds [`crate::NavRegionPlugin`], which builds the arriving tiles in the
//! fixed step.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::level::LevelId;
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{
    CellLoaded, CellUnloading, GateId, OriginOffset, Readiness, StreamGates, StreamNavLayout,
};

use crate::NavTileCache;
use crate::config::{NavAgentParams, NavMeshBuildParams};
use crate::tile_cache::{NavTileCacheParams, NavTileLayout};

/// The gate's name in [`StreamGates`] and in `stream.cells`' `missing_gates`.
pub const NAV_GATE: &str = "nav";

/// The id the hook gives a streaming cell in the tile cache: the cell's `x` and `z` packed into one
/// `u64` (each as its low 32 bits). The cache is one per world, so the level is not part of it.
pub fn nav_cell_id(cell: CellCoord) -> u64 {
    (u64::from(cell.x as u32) << 32) | u64::from(cell.z as u32)
}

/// Whether `cell` packs into a [`nav_cell_id`] without two cells sharing an id (both axes fit `i32`).
fn nav_cell_fits(cell: CellCoord) -> bool {
    i32::try_from(cell.x).is_ok() && i32::try_from(cell.z).is_ok()
}

/// Loads streamed cells' nav tiles and answers the `nav` gate (spec 78 § 13.2).
#[derive(Default)]
pub struct NavStreamPlugin;

impl Plugin for NavStreamPlugin {
    fn build(&self, app: &mut App) {
        crate::origin::ensure(app);
        if !app.has_plugin(std::any::type_name::<crate::NavRegionPlugin>()) {
            app.add_plugins(crate::NavRegionPlugin);
        }
        let gate = app
            .world_mut()
            .get_resource_or_init::<StreamGates>()
            .register(NAV_GATE, Readiness::SimReady);
        app.insert_resource(NavStreamState {
            gate,
            loaded: BTreeMap::new(),
            load_failures: 0,
        })
        .add_message::<CellLoaded>()
        .add_message::<CellUnloading>()
        // Every frame, so no message is missed (a fixed step does not run every frame).
        .add_systems(
            PostUpdate,
            (ensure_streamed_cache, ApplyDeferred, stream_nav_cells).chain(),
        );
    }
}

/// The hook's bookkeeping.
#[derive(Resource, Debug)]
pub struct NavStreamState {
    gate: GateId,
    /// Cells whose tiles are in the cache, with their cache id.
    loaded: BTreeMap<(LevelId, CellCoord), u64>,
    /// Nav blobs the cache refused (damaged or foreign) or that had no streamed cache to go to.
    load_failures: u64,
}

impl NavStreamState {
    /// The `nav` gate's id.
    pub fn gate(&self) -> GateId {
        self.gate
    }

    /// Cells whose nav tiles are loaded.
    pub fn loaded_cells(&self) -> usize {
        self.loaded.len()
    }

    /// Nav blobs that could not load. Such a cell still passes the gate: its agents find no mesh
    /// there, which beats holding the whole cell back.
    pub fn load_failures(&self) -> u64 {
        self.load_failures
    }
}

/// Builds the streamed tile cache a level's cooked nav tiles need, from its world index
/// ([`StreamNavLayout`]), unless the app already has one on the same grid. The layout is in world
/// meters and the cache in local ones.
fn ensure_streamed_cache(
    layout: Option<Res<StreamNavLayout>>,
    origin: Option<Res<OriginOffset>>,
    cache: Option<Res<NavTileCache>>,
    mut commands: Commands,
) {
    let Some(layout) = layout.filter(|l| l.is_changed()) else {
        return;
    };
    let cook = layout.cook;
    let offset = origin.map_or([0.0; 3], |o| o.0.map(|axis| axis as f32));
    let offset = Vec3::from_array(offset);
    let in_world = NavTileLayout {
        origin: Vec3::from_array(layout.origin),
        width: layout.width as usize,
        height: layout.height as usize,
    };
    let wanted = NavTileLayout {
        origin: in_world.origin - offset,
        ..in_world
    };
    if cache.is_some_and(|c| c.is_streamed() && c.layout() == wanted) {
        return;
    }
    let params = NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: cook.cell_size,
            cell_height: cook.cell_height,
            border_size: cook.border_size,
            agent: NavAgentParams {
                radius: cook.agent_radius,
                height: cook.agent_height,
                max_climb: cook.agent_max_climb,
                max_slope: cook.agent_max_slope,
            },
        },
        tile_size: cook.tile_size,
    };
    // Built in world meters and then shifted, so the cache knows its cooked heights' frame.
    match NavTileCache::streamed(params, in_world) {
        Ok(mut cache) => {
            cache.shift_origin(-offset);
            commands.insert_resource(cache);
        }
        Err(error) => tracing::error!(target: "regia_nav", "stream_nav_layout_refused: {error}"),
    }
}

/// `PostUpdate`: loads and unloads cells' nav tiles and marks the cells whose tiles are built.
pub(crate) fn stream_nav_cells(
    mut loaded: MessageReader<CellLoaded>,
    mut unloading: MessageReader<CellUnloading>,
    mut state: ResMut<NavStreamState>,
    gates: Option<ResMut<StreamGates>>,
    mut cache: Option<ResMut<NavTileCache>>,
) {
    let Some(mut gates) = gates else {
        return;
    };
    let state = &mut *state;
    for message in unloading.read() {
        if let Some(id) = state.loaded.remove(&(message.level, message.cell))
            && let Some(cache) = cache.as_deref_mut()
            && let Err(error) = cache.unload_cell(id)
        {
            tracing::warn!(target: "regia_nav", "nav cell {}: unload: {error}", message.cell);
        }
    }
    for message in loaded.read() {
        let Some(blob) = message.nav.as_deref() else {
            continue;
        };
        let _span = tracing::info_span!("nav_stream_load_cell").entered();
        if !nav_cell_fits(message.cell) {
            // Its id would alias another cell's, and an unload of either would drop both.
            state.load_failures += 1;
            tracing::warn!(target: "regia_nav", "nav cell {}: nav_cell_out_of_range", message.cell);
            continue;
        }
        let id = nav_cell_id(message.cell);
        match cache.as_deref_mut().map(|cache| cache.load_cell(id, blob)) {
            Some(Ok(_)) => {
                state.loaded.insert((message.level, message.cell), id);
            }
            Some(Err(error)) => {
                state.load_failures += 1;
                tracing::warn!(target: "regia_nav", "nav cell {}: nav_cell_load_failed: {error}", message.cell);
            }
            None => {
                state.load_failures += 1;
                tracing::warn!(
                    target: "regia_nav",
                    "nav cell {}: nav tiles but no NavTileCache to load them into",
                    message.cell
                );
            }
        }
    }
    let waiting: Vec<_> = gates.awaiting(state.gate).collect();
    for (level, cell) in waiting {
        let ready = match state.loaded.get(&(level, cell)) {
            Some(id) => cache.as_deref().is_some_and(|cache| cache.cell_ready(*id)),
            // No tiles (or tiles that could not load): nothing to wait for.
            None => true,
        };
        if ready {
            gates.set_ready(state.gate, level, cell);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NavMeshBuildParams, NavTileCacheParams};
    use regia_core::id::Guid;
    use regia_core::math::Vec3;

    fn params() -> NavTileCacheParams {
        NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: 0.25,
                ..Default::default()
            },
            tile_size: 32,
        }
    }

    fn floor() -> Vec<[Vec3; 3]> {
        let c = 16.0;
        vec![
            [
                Vec3::new(-c, 0.0, -c),
                Vec3::new(c, 0.0, -c),
                Vec3::new(c, 0.0, c),
            ],
            [
                Vec3::new(-c, 0.0, -c),
                Vec3::new(c, 0.0, c),
                Vec3::new(-c, 0.0, c),
            ],
        ]
    }

    fn loaded(level: LevelId, cell: CellCoord, nav: Option<Vec<u8>>) -> CellLoaded {
        CellLoaded {
            level,
            cell,
            entities: Vec::new(),
            nav: nav.map(std::sync::Arc::from),
        }
    }

    /// Spec 78 § 13.2: the cache comes from the layout the world index names. Opened with the origin
    /// already moved, on every axis, the cooked tiles land in local space.
    #[test]
    fn the_cache_is_built_from_the_index_and_keeps_tiles_local_with_the_origin_moved() {
        let whole = NavTileCache::build(params(), &floor()).expect("builds");
        let blob = whole.cook_cell(&[(0, 0)]).expect("cooks");
        let cooked = whole.layout();
        let p = params();
        let layout = StreamNavLayout {
            cook: regia_scene::streaming::StreamNavCook {
                cell_size: p.build.cell_size,
                cell_height: p.build.cell_height,
                border_size: p.build.border_size,
                tile_size: p.tile_size,
                agent_radius: p.build.agent.radius,
                agent_height: p.build.agent.height,
                agent_max_climb: p.build.agent.max_climb,
                agent_max_slope: p.build.agent.max_slope,
            },
            origin: cooked.origin.to_array(),
            width: cooked.width as u32,
            height: cooked.height as u32,
        };
        let offset = Vec3::new(1024.0, 1024.0, -2048.0);
        let mut app = App::new();
        app.insert_resource(FixedTime::default());
        app.add_plugins(NavStreamPlugin);
        app.insert_resource(OriginOffset([1024.0, 1024.0, -2048.0]));
        app.insert_resource(layout);
        let (level, cell) = (LevelId(Guid::from_u128(5)), CellCoord::new(0, 0));
        app.world_mut()
            .resource_mut::<StreamGates>()
            .note_spawned(level, cell);
        app.world_mut().write_message(loaded(level, cell, Some(blob)));
        for _ in 0..8 {
            app.update();
            app.world_mut().run_schedule(FixedUpdate);
        }
        assert_eq!(app.world().resource::<NavStreamState>().load_failures(), 0);
        let cache = app.world().resource::<NavTileCache>();
        assert_eq!(cache.layout().origin, cooked.origin - offset);
        let tile_center = cooked.origin + Vec3::new(4.0, 0.0, 4.0);
        let (_, on_mesh) = cache
            .query()
            .find_nearest(tile_center - offset, Vec3::splat(2.0))
            .expect("on the tile, in local space");
        assert!((on_mesh.y + 1024.0).abs() < 0.5, "local height {}", on_mesh.y);
    }

    /// A cell whose coordinates do not fit `i32` would share a cache id with another: it is
    /// refused and counted, and the cell with the aliased id keeps its tiles.
    #[test]
    fn a_cell_that_would_alias_another_id_is_refused() {
        let whole = NavTileCache::build(params(), &floor()).expect("builds");
        let blob = whole.cook_cell(&[(0, 0)]).expect("cooks");
        let streamed = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
        let mut app = App::new();
        app.insert_resource(FixedTime::default());
        app.add_plugins(NavStreamPlugin);
        app.insert_resource(streamed);
        let level = LevelId(Guid::from_u128(5));
        let (a, far) = (CellCoord::new(0, 0), CellCoord::new(1 << 32, 0));
        assert_eq!(nav_cell_id(a), nav_cell_id(far), "the packing aliases them");
        app.world_mut()
            .write_message(loaded(level, a, Some(blob.clone())));
        app.world_mut().write_message(loaded(level, far, Some(blob)));
        app.update();
        let state = app.world().resource::<NavStreamState>();
        assert_eq!(state.load_failures(), 1, "the far cell is refused");
        assert_eq!(state.loaded.len(), 1);
        assert!(state.loaded.contains_key(&(level, a)));
    }

    /// Spec 78 § 13.2: a cell with nav tiles passes the `nav` gate once they are built, a cell
    /// without passes at once, a damaged blob is counted and does not hold its cell, and an unload
    /// takes the tiles out of the cache.
    #[test]
    fn the_nav_gate_follows_the_tiles_of_each_cell() {
        let whole = NavTileCache::build(params(), &floor()).expect("builds");
        let blob = whole.cook_cell(&[(0, 0)]).expect("cooks");
        let streamed = NavTileCache::streamed(params(), whole.layout()).expect("streamed");

        let mut app = App::new();
        app.insert_resource(FixedTime::default());
        app.add_plugins(NavStreamPlugin);
        app.insert_resource(streamed);
        let level = LevelId(Guid::from_u128(5));
        let (a, b, c) = (CellCoord::new(0, 0), CellCoord::new(1, 0), CellCoord::new(2, 0));
        {
            let mut gates = app.world_mut().resource_mut::<StreamGates>();
            for cell in [a, b, c] {
                gates.note_spawned(level, cell);
            }
        }
        let damaged = {
            let mut bytes = blob.clone();
            let last = bytes.len() - 1;
            bytes[last] ^= 0xff;
            bytes
        };
        app.world_mut().write_message(loaded(level, a, Some(blob)));
        app.world_mut().write_message(loaded(level, b, None));
        app.world_mut().write_message(loaded(level, c, Some(damaged)));
        app.update();
        let gate = app.world().resource::<NavStreamState>().gate();
        let waiting = |app: &App| -> Vec<CellCoord> {
            app.world()
                .resource::<StreamGates>()
                .awaiting(gate)
                .map(|(_, cell)| cell)
                .collect()
        };
        assert_eq!(waiting(&app), vec![a], "only the cell with tiles waits for them");
        let state = app.world().resource::<NavStreamState>();
        assert_eq!((state.loaded_cells(), state.load_failures()), (1, 1));
        assert!(
            app.world()
                .resource::<StreamGates>()
                .missing(level, a)
                .contains(&NAV_GATE)
        );

        // The fixed step builds the arriving tiles; the next frame's hook marks the cell.
        app.world_mut().run_schedule(FixedUpdate);
        app.update();
        assert!(waiting(&app).is_empty());
        assert!(app.world().resource::<NavTileCache>().cell_ready(nav_cell_id(a)));

        app.world_mut().write_message(CellUnloading { level, cell: a });
        app.update();
        assert_eq!(app.world().resource::<NavStreamState>().loaded_cells(), 0);
        assert!(app.world().resource::<NavTileCache>().loaded_cells().is_empty());
    }

    #[test]
    fn cell_ids_keep_negative_coordinates_apart() {
        let ids = [
            nav_cell_id(CellCoord::new(0, 0)),
            nav_cell_id(CellCoord::new(-1, 0)),
            nav_cell_id(CellCoord::new(0, -1)),
            nav_cell_id(CellCoord::new(-1, -1)),
            nav_cell_id(CellCoord::new(1, 1)),
        ];
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }
}
