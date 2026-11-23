// SPDX-License-Identifier: MPL-2.0
//! A generated world over an in-memory source: `n` by `n` cells of `size` meters, `per_cell` entities
//! each, plus an app in fixed-step lockstep around it.
#![allow(dead_code, reason = "each test binary uses a part of this")]

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::ScenePlugin;
use regia_scene::Transform;
use regia_scene::cooked::{COOKED_SCENE_VERSION, CookedScene, cook_scene};
use regia_scene::format::{SceneEntity, SceneFile};
use regia_scene::level::{LevelId, LoadMode};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{LevelStreaming, StreamBudgets, StreamTuning};
use regia_world::format::{
    CELL_BLOB_FORMAT_VERSION, CellBlob, CellRecord, WORLD_INDEX_FORMAT_VERSION, WorldIndex, cell_blob_id,
};
use regia_world::{MemoryCellSource, StreamTiming, WorldStreaming, WorldStreamingPlugin, open_streamed_from};

pub const SIZE: f64 = 100.0;

pub fn level() -> LevelId {
    LevelId(Guid::from_u128(0xfeed))
}

pub fn cell_id(cell: CellCoord) -> regia_asset::AssetId {
    cell_blob_id(level(), cell)
}

pub fn settings(load: f64, unload: f64) -> LevelStreaming {
    LevelStreaming {
        enabled: true,
        cell_size_m: SIZE,
        load_radius_m: load,
        unload_radius_m: unload,
        hlod: None,
        budgets: Some(StreamBudgets {
            max_loads_per_step: 0,
            max_inflight: 4,
            max_pending_bytes: 1 << 30,
            apply_budget_entities: 100,
            ..StreamBudgets::default()
        }),
        tuning: StreamTuning {
            retry_after_steps: 10,
            ..StreamTuning::default()
        },
        ..LevelStreaming::default()
    }
}

pub fn app() -> App {
    let mut app = App::new();
    app.add_plugins(ScenePlugin).add_plugins(WorldStreamingPlugin);
    regia_app::enable_fixed_step(&mut app, 60.0);
    app
}

/// One root entity with `size - 1` children at `(x, z)` relative to the cell center. Roots sort by
/// their first guid.
fn hierarchy(next: &mut u128, size: usize, translation: Vec3) -> Vec<SceneEntity> {
    let root = Guid::from_u128(*next);
    *next += 1;
    let mut out = Vec::with_capacity(size);
    let transform = |v: Vec3| format!("(translation: ({}, {}, {}))", v.x, v.y, v.z);
    let text = format!(
        r#"(guid: "{root}", name: "e", components: {{ "regia_scene::transform::Transform": {} }})"#,
        transform(translation)
    );
    out.push(ron::from_str::<SceneEntity>(&text).expect("root"));
    for _ in 1..size {
        let guid = Guid::from_u128(*next);
        *next += 1;
        let text = format!(
            r#"(guid: "{guid}", name: "c", parent: Some("{root}"), components: {{ "regia_scene::transform::Transform": {} }})"#,
            transform(Vec3::ZERO)
        );
        out.push(ron::from_str::<SceneEntity>(&text).expect("child"));
    }
    out
}

/// The cooked blob of a cell with `per_cell` hierarchies of `hierarchy_size` entities.
pub fn cook_cell(app: &App, cell: CellCoord, per_cell: usize, hierarchy_size: usize) -> CookedScene {
    let mut next = ((cell.x as u128 + 1000) << 64) | ((cell.z as u128 + 1000) << 32) | 1;
    let mut file = SceneFile::new();
    for i in 0..per_cell {
        file.entities.extend(hierarchy(
            &mut next,
            hierarchy_size,
            Vec3::new(i as f32 * 0.5, 0.0, 0.0),
        ));
    }
    let (cooked, warnings) = cook_scene(app.world(), &file).expect("cook");
    assert!(warnings.is_empty(), "{warnings:?}");
    cooked
}

pub struct World {
    pub index: WorldIndex,
    pub source: MemoryCellSource,
}

/// Cooks an `nx` by `nz` world into memory. The origin space is one big cell at 0 so anchors are
/// simple: root translations stay relative to the origin.
pub fn build(
    app: &App,
    nx: i64,
    nz: i64,
    per_cell: usize,
    hierarchy_size: usize,
    s: LevelStreaming,
) -> World {
    build_with(app, nx, nz, s, |_| (per_cell, hierarchy_size))
}

pub fn build_with(
    app: &App,
    nx: i64,
    nz: i64,
    s: LevelStreaming,
    shape: impl Fn(CellCoord) -> (usize, usize),
) -> World {
    let source = MemoryCellSource::new();
    let mut cells = Vec::new();
    for x in 0..nx {
        for z in 0..nz {
            let coord = CellCoord::new(x, z);
            let (per_cell, hierarchy_size) = shape(coord);
            if per_cell == 0 {
                continue;
            }
            let scene = cook_cell(app, coord, per_cell, hierarchy_size);
            let blob = CellBlob {
                format_version: CELL_BLOB_FORMAT_VERSION,
                coord,
                scene,
                anchor: [0, 0, 0],
                nav: None,
            };
            let id = cell_id(coord);
            let bytes = blob.to_bytes(id, Vec::new()).expect("encode");
            let record = CellRecord {
                coord,
                blob: id,
                blob_hash: *blake3::hash(&bytes).as_bytes(),
                entity_count: (per_cell * hierarchy_size) as u32,
                own_bytes: bytes.len() as u64,
                dep_bytes: 0,
                vram_bytes: 0,
                min_y: 0.0,
                max_y: 1.0,
                priority: 0,
                deps: vec![],
                nav_tiles: 0,
                has_terrain_collision: false,
            };
            source.insert(id, bytes);
            cells.push(record);
        }
    }
    let index = WorldIndex {
        format_version: WORLD_INDEX_FORMAT_VERSION,
        level: level().0,
        content_hash: WorldIndex::hash_cells(&cells),
        settings: s,
        cells,
        bounds: [CellCoord::new(0, 0), CellCoord::new(nx - 1, nz - 1)],
        hlod: None,
        replicated: vec![],
        nav: None,
    };
    World { index, source }
}

pub fn empty_scene() -> CookedScene {
    CookedScene {
        format_version: COOKED_SCENE_VERSION,
        strings: vec![],
        entities: vec![],
        resources: vec![],
    }
}

/// Opens the world in lockstep with `latency` steps.
pub fn open(app: &mut App, world: &World, latency: u32) {
    let source = Arc::new(world.source.clone());
    let index = world.index.clone();
    {
        let w = app.world_mut();
        let _ = w.resource::<AppTypeRegistry>();
        w.resource_mut::<WorldStreaming>()
            .set_timing(StreamTiming::Lockstep {
                latency_steps: latency,
            });
        open_streamed_from(w, level(), LoadMode::Replace, index, &empty_scene(), source).expect("open");
    }
}

/// A source entity at `(x, z)`.
pub fn spawn_source(app: &mut App, x: f32, z: f32) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(x, 0.0, z)),
            regia_scene::streaming::StreamingSource {
                kind: regia_scene::streaming::SourceKind::Player,
                view_bias: 0.0,
                ..Default::default()
            },
        ))
        .id()
}

pub fn move_to(app: &mut App, entity: Entity, x: f32, z: f32) {
    if let Some(mut t) = app.world_mut().get_mut::<Transform>(entity) {
        t.translation = Vec3::new(x, 0.0, z);
    }
}

pub fn state(app: &App, cell: CellCoord) -> Option<regia_world::CellState> {
    app.world().resource::<WorldStreaming>().state(level(), cell)
}

pub fn stats(app: &App) -> regia_world::StreamStats {
    app.world().resource::<WorldStreaming>().stats().clone()
}

pub fn steps(app: &mut App, n: usize) {
    for _ in 0..n {
        app.update();
    }
}

pub fn count_members(app: &mut App) -> usize {
    app.world_mut()
        .query::<&regia_scene::streaming::CellMember>()
        .iter(app.world())
        .count()
}

/// A second level over the same grid whose cells hold no entities (their guids would collide with
/// the first level's): `id` names it, so its cell blobs and index are its own.
pub fn build_empty_level(id: LevelId, nx: i64, nz: i64, s: LevelStreaming) -> World {
    let source = MemoryCellSource::new();
    let mut cells = Vec::new();
    for x in 0..nx {
        for z in 0..nz {
            let coord = CellCoord::new(x, z);
            let blob = CellBlob {
                format_version: CELL_BLOB_FORMAT_VERSION,
                coord,
                scene: empty_scene(),
                anchor: [0, 0, 0],
                nav: None,
            };
            let blob_id = cell_blob_id(id, coord);
            let bytes = blob.to_bytes(blob_id, Vec::new()).expect("encode");
            cells.push(CellRecord {
                coord,
                blob: blob_id,
                blob_hash: *blake3::hash(&bytes).as_bytes(),
                entity_count: 0,
                own_bytes: bytes.len() as u64,
                dep_bytes: 0,
                vram_bytes: 0,
                min_y: 0.0,
                max_y: 1.0,
                priority: 0,
                deps: vec![],
                nav_tiles: 0,
                has_terrain_collision: false,
            });
            source.insert(blob_id, bytes);
        }
    }
    let index = WorldIndex {
        format_version: WORLD_INDEX_FORMAT_VERSION,
        level: id.0,
        content_hash: WorldIndex::hash_cells(&cells),
        settings: s,
        cells,
        bounds: [CellCoord::new(0, 0), CellCoord::new(nx - 1, nz - 1)],
        hlod: None,
        replicated: vec![],
        nav: None,
    };
    World { index, source }
}

/// Opens another level additively next to the open one.
pub fn open_additive(app: &mut App, id: LevelId, world: &World) {
    let source = Arc::new(world.source.clone());
    open_streamed_from(
        app.world_mut(),
        id,
        LoadMode::Additive,
        world.index.clone(),
        &empty_scene(),
        source,
    )
    .expect("open additively");
}
