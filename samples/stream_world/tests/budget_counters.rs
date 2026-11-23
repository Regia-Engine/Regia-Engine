// SPDX-License-Identifier: MIT-0
//! CPU counters for the streaming sample (spec 78 § 8.4, § 8.2).
//!
//! A scripted flight of `FLIGHT_STEPS` fixed steps, then one move past the sample's unload
//! hysteresis. Counts only: no `elapsed()` assert, no sleep, no GPU, no golden image.
//!
//! The cell budget is the one `tight_budgets` in `acceptance.rs` already configures
//! (`memory_budget_bytes = largest cell * 90`, `max_loads_per_step` 8, `apply_budget_entities`
//! 400). The hysteresis is the pair `level_text` writes: `load_radius_m` 512, `unload_radius_m`
//! 640. The runtime does not publish a single "unloaded within K steps" number.
//! `StreamTuning::plan_interval_steps` defaults to 30 (spec 78 § 8.2), the longest gap between
//! plans when a source has not moved, and `apply_step` despawns an announced cell on the following
//! run. `UNLOAD_STEPS` is that interval plus one apply run.

use std::fmt::Write as _;

use std::collections::BTreeSet;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::{AssetServer, AssetServerSettings};
use regia_asset_pipeline::stream_cook::{CacheAssets, cook_streamed_level, register_cook_types};
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::reflect::{AppTypeRegistry, TypeRegistry};
use regia_physics::PhysicsPlugin;
use regia_scene::ScenePlugin;
use regia_scene::Transform;
use regia_scene::format::SceneFile;
use regia_scene::level::{LevelId, LoadMode};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{StreamBudgets, StreamTuning, StreamingSource};
use regia_world::format::WorldIndex;
use regia_world::{
    CellState, StreamStats, StreamTiming, TraceKind, WorldOrigin, WorldStreaming, WorldStreamingPlugin,
};
use stream_world::CELL;

/// Fixed steps of the scripted flight. Not a timer.
const FLIGHT_STEPS: u32 = 16;
/// Meters along +X per step. Above `cell_size / 8` (16 m), so each step is a plan trigger
/// (spec 78 § 8.2), and short enough that the home cell stays inside the load radius.
const STEP_METERS: f64 = 24.0;
/// `StreamTuning::plan_interval_steps` default (30) plus the apply run that despawns an announced cell.
const UNLOAD_STEPS: u32 = 31;
/// Cells on a side of the cooked patch. Larger than the memory budget's 90-cell room, and far
/// smaller than the acceptance flight's 512-cell world.
const PATCH: u32 = 12;
/// Props in each cell, kept inside the cell so none lands on the next cell's edge.
const PROPS: u32 = 2;
/// The cell the flight starts in, and the one the later move must unload.
const HOME: CellCoord = CellCoord::new(5, 5);
/// Open-wait cap, in steps. The acceptance flight uses the same kind of counter.
const OPEN_STEPS: u32 = 400;

const TRANSFORM: &str = "regia_scene::transform::Transform";
const PRIMITIVE: &str = "regia_render::primitives::Primitive";
const STREAMING: &str = "regia_scene::streaming::LevelStreaming";

fn level_id() -> LevelId {
    LevelId(Guid::from_u128(0x57b0_d6e7))
}

fn guid(index: u32) -> String {
    format!("57b0d6e7-0000-4000-8000-{index:012x}")
}

/// The sample's radii and origin (`level_text`), with proxies off so the counter test needs no GPU.
fn level_text() -> String {
    let mut out = String::from("(format_version: 1, entities: [\n");
    let mut index = 0u32;
    for cx in 0..PATCH {
        for cz in 0..PATCH {
            let ox = f64::from(cx) * CELL;
            let oz = f64::from(cz) * CELL;
            for prop in 0..PROPS {
                index += 1;
                let x = ox + 16.0 + f64::from(prop) * 24.0;
                let z = oz + 16.0;
                let _ = writeln!(
                    out,
                    "(guid: \"{}\", components: {{ \"{TRANSFORM}\": (translation: ({x:.1}, 1.0, {z:.1})), \
                     \"{PRIMITIVE}\": (format_version: 1, shape: Cuboid(size: (1.0, 1.0, 1.0)), \
                     color: (0.4, 0.4, 0.5, 1.0), roughness: 0.7, metallic: 0.0) }}),",
                    guid(index)
                );
            }
        }
    }
    let _ = write!(
        out,
        "], resources: {{ \"{STREAMING}\": (enabled: true, cell_size_m: {CELL:.1}, load_radius_m: 512.0, \
         unload_radius_m: 640.0, hlod: None, origin: (enabled: true, cell_size_m: 1024.0, \
         threshold_m: 1536.0)) }})"
    );
    out
}

/// Same numbers as `tight_budgets` in `acceptance.rs`.
fn tight_budgets(index: &mut WorldIndex) {
    let largest = index.cells.iter().map(|cell| cell.own_bytes).max().unwrap_or(0);
    index.settings.budgets = Some(StreamBudgets {
        max_loads_per_step: 8,
        max_inflight: 8,
        max_pending_bytes: largest * 16,
        apply_budget_ms: 2.0,
        apply_budget_entities: 400,
        memory_budget_bytes: largest * 90,
        vram_budget_bytes: 0,
        hlod_budget_bytes: 256 << 20,
    });
}

/// How many cells the configured memory budget can hold, using the smallest cooked cell.
fn cell_cap(index: &WorldIndex) -> u64 {
    let min_bytes = index
        .cells
        .iter()
        .map(|cell| cell.own_bytes)
        .filter(|bytes| *bytes > 0)
        .min()
        .unwrap_or(1);
    index.settings.effective_budgets().memory_budget_bytes / min_bytes
}

struct Cooked {
    dir: std::path::PathBuf,
    index: WorldIndex,
    always: regia_scene::cooked::CookedScene,
}

fn headless_app() -> App {
    let mut app = App::new();
    app.add_plugins(ScenePlugin)
        .add_plugins(PhysicsPlugin)
        .add_plugins(WorldStreamingPlugin);
    regia_render::register_render_types(&mut app);
    regia_app::enable_fixed_step(&mut app, stream_world::STEP_HZ);
    app
}

fn registry_of(app: &App) -> TypeRegistry {
    let handle = app.world().resource::<AppTypeRegistry>().0.clone();
    let mut registry = TypeRegistry::default();
    std::mem::swap(&mut registry, &mut *handle.write());
    register_cook_types(&mut registry);
    registry
}

fn cook() -> Cooked {
    let dir = regia_test_utils::unique_temp_dir("stream_budget_counters");
    let platform = regia_asset_pipeline::host_platform();
    let cache = regia_asset::cooked_cache_dir(&dir, platform);
    std::fs::create_dir_all(&cache).expect("cache dir");
    let scene = SceneFile::from_ron_str(&level_text()).expect("level parses");
    let app = headless_app();
    let registry = registry_of(&app);
    let assets = CacheAssets::new(&cache);
    let output = cook_streamed_level(
        level_id(),
        &scene,
        &registry,
        &assets,
        Some(&cache),
        "assets/world.level.ron",
    )
    .expect("cook")
    .expect("streamed");
    assert!(output.report.cells >= 16, "{:?}", output.report);
    let read = |id| std::fs::read(cache.join(regia_asset::cooked_file_name(id))).expect("cooked file");
    let mut index =
        WorldIndex::from_bytes(&read(regia_world::format::world_index_id(level_id())), level_id())
            .expect("index");
    tight_budgets(&mut index);
    let always = regia_world::format::always_loaded_from_bytes(
        &read(regia_world::format::always_loaded_id(level_id())),
        level_id(),
    )
    .expect("always");
    Cooked { dir, index, always }
}

fn stats(app: &App) -> StreamStats {
    app.world().resource::<StreamStats>().clone()
}

fn loaded_count(app: &App) -> usize {
    app.world()
        .resource::<WorldStreaming>()
        .cells(level_id())
        .filter(|(_, state)| !matches!(state, CellState::Unloaded | CellState::Failed(_)))
        .count()
}

fn flight_position(step: u32) -> [f64; 3] {
    let origin = (HOME.x as f64 + 0.5) * CELL;
    let z = (HOME.z as f64 + 0.5) * CELL;
    [origin + f64::from(step) * STEP_METERS, 1.7, z]
}

fn place(app: &mut App, source: Entity, world_pos: [f64; 3]) {
    let local = app
        .world()
        .resource::<WorldOrigin>()
        .from_world(world_pos)
        .expect("local");
    app.world_mut()
        .get_mut::<Transform>(source)
        .expect("source")
        .translation = Vec3::from_array(local);
}

/// Load events of the step just run. The set's length against the list is the duplicate check;
/// `loads_issued` is the counter of load calls for the same step.
fn assert_one_load_per_cell(app: &App, issued_before: u64, at: &str) {
    let streaming = app.world().resource::<WorldStreaming>();
    let step = streaming.stats().step;
    let events: Vec<CellCoord> = streaming
        .trace()
        .filter(|event| event.step == step && event.kind == TraceKind::Loading)
        .filter_map(|event| event.cell)
        .collect();
    let distinct: BTreeSet<CellCoord> = events.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        events.len(),
        "{at}: a cell was loaded twice in step {step}: {events:?}"
    );
    let issued = stats(app).loads_issued - issued_before;
    assert_eq!(
        issued,
        events.len() as u64,
        "{at}: {issued} load calls, {} load events, step {step}",
        events.len()
    );
}

fn assert_within_budget(app: &App, cap: u64, at: &str) {
    let loaded = loaded_count(app);
    let now = stats(app);
    assert_eq!(loaded, now.resident_cells as usize, "{at}: {now:?}");
    assert!(
        loaded as u64 <= cap,
        "{at}: {loaded} loaded cells, the sample budget holds {cap}: {now:?}"
    );
    assert_eq!(now.failures, 0, "{at}: {now:?}");
    assert_eq!(now.lockstep_stalls, 0, "{at}");
}

fn step(app: &mut App, cap: u64, at: &str) {
    let issued = stats(app).loads_issued;
    app.update();
    assert_one_load_per_cell(app, issued, at);
    assert_within_budget(app, cap, at);
}

/// The home cell's ground distance from `pos`.
fn home_distance(index: &WorldIndex, pos: [f64; 3]) -> f64 {
    let grid = index.settings.grid().expect("grid");
    grid.distance_to_cell(pos[0], pos[2], HOME)
}

#[test]
fn a_scripted_flight_stays_inside_the_cell_budget() {
    assert_eq!(
        StreamTuning::default().plan_interval_steps + 1,
        UNLOAD_STEPS,
        "UNLOAD_STEPS is the documented plan interval plus one apply run"
    );
    let cooked = cook();
    let budgets = cooked.index.settings.effective_budgets();
    let largest = cooked
        .index
        .cells
        .iter()
        .map(|cell| cell.own_bytes)
        .max()
        .unwrap_or(0);
    assert!(largest > 0, "a cooked cell has no bytes");
    assert_eq!(budgets.memory_budget_bytes, largest * 90);
    assert_eq!(budgets.max_loads_per_step, 8);
    assert_eq!(budgets.apply_budget_entities, 400);
    assert_eq!(cooked.index.settings.load_radius_m, 512.0);
    assert_eq!(cooked.index.settings.unload_radius_m, 640.0);
    let cap = cell_cap(&cooked.index);
    assert!(
        cap >= 90,
        "largest * 90 bytes holds at least 90 of the largest cell, cap {cap}"
    );

    let mut app = headless_app();
    let server = AssetServer::open(AssetServerSettings {
        project_root: cooked.dir.clone(),
        platform: regia_asset_pipeline::host_platform().to_string(),
        content_dir: None,
    })
    .expect("server");
    let reader = server.payload_reader();
    app.insert_resource(server);
    app.world_mut()
        .resource_mut::<WorldStreaming>()
        .set_timing(StreamTiming::Lockstep { latency_steps: 2 });
    regia_world::open_streamed_from(
        app.world_mut(),
        level_id(),
        LoadMode::Replace,
        cooked.index.clone(),
        &cooked.always,
        Arc::new(regia_world::PayloadCellSource::new(reader)),
    )
    .expect("open");

    let start = flight_position(0);
    let source = app
        .world_mut()
        .spawn((
            Transform::default(),
            StreamingSource {
                drives_origin: true,
                ..StreamingSource::default()
            },
        ))
        .id();
    place(&mut app, source, start);

    let mut waited = 0u32;
    loop {
        step(&mut app, cap, "open");
        waited += 1;
        assert!(
            waited < OPEN_STEPS,
            "the open wait never ended: {:?}",
            stats(&app)
        );
        if stats(&app).pending_waits == 0 {
            break;
        }
    }
    let home_after_open = app.world().resource::<WorldStreaming>().state(level_id(), HOME);
    assert!(
        home_after_open.as_ref().is_some_and(CellState::is_spawned),
        "the home cell never spawned: {home_after_open:?}"
    );

    for at in 0..FLIGHT_STEPS {
        place(&mut app, source, flight_position(at));
        step(&mut app, cap, "flight");
    }
    let end = flight_position(FLIGHT_STEPS - 1);
    let end_distance = home_distance(&cooked.index, end);
    assert!(
        end_distance <= cooked.index.settings.load_radius_m,
        "the flight left the home cell: distance {end_distance}"
    );
    let loaded = loaded_count(&app);
    assert!(loaded > 0, "the flight loaded no cells");
    assert!(
        loaded as u64 <= cap,
        "after {FLIGHT_STEPS} steps, {loaded} cells are loaded; the sample budget holds {cap}"
    );

    // Past `unload_radius_m` (640) and short of a teleport (`2 * unload_radius`, spec 78 § 8.7).
    let hi = cooked.index.settings.grid().expect("grid").cell_max(HOME)[0];
    let away_x = hi + cooked.index.settings.unload_radius_m + CELL * 0.25;
    let away = [away_x, 1.7, end[2]];
    let away_distance = home_distance(&cooked.index, away);
    assert!(
        away_distance > cooked.index.settings.unload_radius_m,
        "the move stays inside the unload radius: {away_distance}"
    );
    let hop = (away[0] - end[0]).hypot(away[2] - end[2]);
    assert!(
        hop <= 2.0 * cooked.index.settings.unload_radius_m,
        "the move is a teleport ({hop} m), not the hysteresis release"
    );
    place(&mut app, source, away);
    for _ in 0..UNLOAD_STEPS {
        step(&mut app, cap, "unload");
        let state = app.world().resource::<WorldStreaming>().state(level_id(), HOME);
        if state == Some(CellState::Unloaded) {
            return;
        }
    }
    let state = app.world().resource::<WorldStreaming>().state(level_id(), HOME);
    panic!("home cell {HOME} was still {state:?} after {UNLOAD_STEPS} steps past unload_radius_m 640");
}
