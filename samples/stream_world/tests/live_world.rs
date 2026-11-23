// SPDX-License-Identifier: MIT-0
//! Live world-streaming acceptance (v0.5.0 burn-down rows B09, B10, C01; spec 78 § 18): what the
//! headless flight in `acceptance.rs` does not prove.
//!
//! - `teleports_cancel_far_loads_and_every_transform_agrees_through_each_rebase`: the cooked
//!   8.4 km² world (2048 m × 4096 m, 512 cells), teleported with no warning while loads are in
//!   flight. Far loads are cancelled and never spawn; no entity appears twice; every cell holds
//!   exactly its cooked entities; and through every floating-origin rebase the static members sit
//!   where the level authored them (1 mm), each dynamic body's `Transform` agrees with its physics
//!   body, and a nav mesh keeps its world position.
//! - `lightmap_receivers_come_back_with_their_cell`: entity-owned lightmap data (spec 78 § 21) is
//!   the same after a cell unloads and loads again.
//! - `live_flight_holds_the_memory_caps_with_real_proxy_residency`: the flight with the real
//!   renderer (lavapipe on the gate, a card by `REGIA_GPU_UUID`): HLOD proxies become resident only
//!   through the renderer, never by the test; the device's measured GPU memory stays inside the
//!   declared tier budget (`regia_render::MemoryBudget`); the stream's VRAM estimate stays inside
//!   the level's platform budget; the queues stay bounded; the cut has no hole after the teleport.
//!   Frame times are measured and printed (`STREAM_LIVE_JSON`), never asserted.
//!
//! Counters and fixed steps only: no wall-clock assertion, no sleep, no golden image.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::{AssetServer, AssetServerSettings};
use regia_asset_pipeline::stream_cook::{CacheAssets, cook_streamed_level, register_cook_types};
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::reflect::{AppTypeRegistry, TypeRegistry};
use regia_physics::PhysicsPlugin;
use regia_physics::components::{ColliderHandle, RigidBodyHandle};
use regia_scene::ScenePlugin;
use regia_scene::Transform;
use regia_scene::components::{EntityGuid, GiMobility, LightmapSurface};
use regia_scene::format::SceneFile;
use regia_scene::level::{LevelId, LoadMode};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{
    CellMember, HlodCut, HlodProxyResidency, Readiness, StreamBudgets, StreamGates, StreamingSource,
};
use regia_world::format::WorldIndex;
use regia_world::{CellState, StreamStats, StreamTiming, WorldOrigin, WorldStreaming, WorldStreamingPlugin};
use stream_world::{CELL, Flight, FlightStep, level_text};

const SEED: u64 = 0x5eed;
/// Static members must sit within this of their authored position, meters (spec 78 § 18 test 9).
const STATIC_TOLERANCE_M: f64 = 1.0e-3;
/// A dynamic body's `Transform` is interpolated between fixed steps (`interpolate_transforms`), so
/// it trails its physics pose by under one step of motion; a rebase applied to one and not the
/// other would be off by a whole origin cell (1024 m).
const BODY_TOLERANCE_M: f32 = 0.25;
/// A dynamic body may not move further than this in world space in one step: the bodies fall
/// 3.5 m onto flat ground.
const BODY_STEP_M: f64 = 1.0;
/// `StreamTuning::plan_interval_steps` (30) plus the apply run that despawns an announced cell.
const UNLOAD_STEPS: u32 = 31;

fn level() -> LevelId {
    LevelId(Guid::from_u128(0x5712_ea32))
}

/// A world cooked into a project directory.
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

/// The acceptance flight's budgets (`acceptance.rs` `tight_budgets`): every wanted cell fits with
/// little to spare. The VRAM budget is the host platform's column of spec 78 § 20, so
/// `vram_over_budget` means something.
fn tight_budgets(index: &mut WorldIndex) {
    let largest = index.cells.iter().map(|c| c.own_bytes).max().unwrap_or(0);
    index.settings.budgets = Some(StreamBudgets {
        max_loads_per_step: 8,
        max_inflight: 8,
        max_pending_bytes: largest * 16,
        apply_budget_ms: 2.0,
        apply_budget_entities: 400,
        memory_budget_bytes: largest * 90,
        vram_budget_bytes: StreamBudgets::for_platform().vram_budget_bytes,
        hlod_budget_bytes: 256 << 20,
    });
}

fn cook(name: &str, text: &str) -> Cooked {
    let dir = regia_test_utils::unique_temp_dir(name);
    let platform = regia_asset_pipeline::host_platform();
    let cache = regia_asset::cooked_cache_dir(&dir, platform);
    std::fs::create_dir_all(&cache).expect("cache dir");
    let scene = SceneFile::from_ron_str(text).expect("level parses");
    let app = headless_app();
    let registry = registry_of(&app);
    let assets = CacheAssets::new(&cache);
    let output = cook_streamed_level(
        level(),
        &scene,
        &registry,
        &assets,
        Some(&cache),
        "assets/world.level.ron",
    )
    .expect("cook")
    .expect("streamed");
    let unexpected: Vec<_> = output
        .report
        .warnings
        .iter()
        .filter(|w| w.code != "stream_large_entity")
        .collect();
    assert!(unexpected.is_empty(), "{unexpected:?}");
    let read = |id| std::fs::read(cache.join(regia_asset::cooked_file_name(id))).expect("cooked file");
    let mut index =
        WorldIndex::from_bytes(&read(regia_world::format::world_index_id(level())), level()).expect("index");
    tight_budgets(&mut index);
    let always = regia_world::format::always_loaded_from_bytes(
        &read(regia_world::format::always_loaded_id(level())),
        level(),
    )
    .expect("always");
    Cooked { dir, index, always }
}

fn open(app: &mut App, cooked: &Cooked) {
    let server = AssetServer::open(AssetServerSettings {
        project_root: cooked.dir.clone(),
        platform: regia_asset_pipeline::host_platform().to_string(),
        content_dir: None,
    })
    .expect("server");
    let reader = server.payload_reader();
    app.insert_resource(server);
    regia_world::open_streamed_from(
        app.world_mut(),
        level(),
        LoadMode::Replace,
        cooked.index.clone(),
        &cooked.always,
        Arc::new(regia_world::PayloadCellSource::new(reader)),
    )
    .expect("open");
}

fn stats(app: &App) -> StreamStats {
    app.world().resource::<StreamStats>().clone()
}

fn place(app: &mut App, source: Entity, world: [f64; 3]) {
    let local = app
        .world()
        .resource::<WorldOrigin>()
        .from_world(world)
        .expect("local");
    app.world_mut()
        .get_mut::<Transform>(source)
        .expect("source")
        .translation = Vec3::from_array(local);
}

/// The authored world position of every entity of a generated level, by guid: one entity per line
/// in [`level_text`], `translation: (x, y, z)` in its `Transform`.
fn authored_positions(text: &str) -> HashMap<u128, [f64; 3]> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let Some(rest) = line.split_once("guid: \"").map(|(_, r)| r) else {
            continue;
        };
        let Some((guid, rest)) = rest.split_once('"') else {
            continue;
        };
        let Some((_, rest)) = rest.split_once("translation: (") else {
            continue;
        };
        let Some((xyz, _)) = rest.split_once(')') else {
            continue;
        };
        let v: Vec<f64> = xyz.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        let (Ok(id), [x, y, z]) = (guid.parse::<Guid>(), v.as_slice()) else {
            continue;
        };
        out.insert(id.as_u128(), [*x, *y, *z]);
    }
    out
}

/// The guid `level_text` gives the ground: a heightfield collider wider than a cell, so it is
/// always loaded (spec 78 § 5.3 rule 3).
const GROUND_GUID: &str = "00000001-0000-4000-8000-000000000000";

/// The generated level's guid kinds (`stream_world::level_text`): 2 = static prop, 3 = dynamic
/// body, 4 = light.
fn guid_kind(guid: u128) -> u32 {
    (guid >> 96) as u32
}

/// Each dynamic body's last world position, by guid.
type Bodies = HashMap<u128, [f64; 3]>;

/// One checked step of the teleport test.
type StepFn<'a> = dyn FnMut(&mut App, &mut Bodies, &str) -> Seen + 'a;

/// What one step's consistency check saw.
#[derive(Default, Debug)]
struct Seen {
    members: usize,
    statics_checked: usize,
    bodies_checked: usize,
    colliders_checked: usize,
    /// The always-loaded ground has its physics collider.
    ground_collider: bool,
    worst_static_m: f64,
    worst_body_m: f32,
}

/// Checks every streamed member after a step: unique guids, members only in spawned cells, static
/// members where the level put them, bodies where physics has them and no world-space jump.
fn check_members(
    app: &mut App,
    truth: &HashMap<u128, [f64; 3]>,
    last_body_world: &mut Bodies,
    at: &str,
) -> Seen {
    let origin = *app.world().resource::<WorldOrigin>();
    let states: BTreeMap<CellCoord, CellState> =
        app.world().resource::<WorldStreaming>().cells(level()).collect();
    let mut seen = Seen::default();
    let mut guids: BTreeSet<u128> = BTreeSet::new();
    let mut bodies_now: HashMap<u128, [f64; 3]> = HashMap::new();
    let world = app.world_mut();
    let mut members = world.query::<(&EntityGuid, &Transform, &CellMember, Option<&RigidBodyHandle>)>();
    let mut handles: Vec<(u128, Vec3, RigidBodyHandle)> = Vec::new();
    for (guid, transform, member, body) in members.iter(world) {
        let id = guid.0.as_u128();
        assert!(guids.insert(id), "{at}: guid {} is in the world twice", guid.0);
        seen.members += 1;
        let state = states.get(&member.0);
        assert!(
            matches!(
                state,
                Some(
                    CellState::Applying
                        | CellState::Spawned
                        | CellState::SimReady
                        | CellState::Visible
                        | CellState::Unloading
                )
            ),
            "{at}: a member of cell {:?} lives on while the cell is {state:?}",
            member.0
        );
        let world_pos = origin.to_world(transform.translation.to_array());
        match guid_kind(id) {
            3 => {
                if let Some(handle) = body {
                    handles.push((id, transform.translation, *handle));
                }
                bodies_now.insert(id, world_pos);
                if let Some(last) = last_body_world.get(&id) {
                    let moved = (0..3)
                        .map(|a| (world_pos[a] - last[a]).powi(2))
                        .sum::<f64>()
                        .sqrt();
                    assert!(
                        moved < BODY_STEP_M,
                        "{at}: body {} jumped {moved} m in world space in one step",
                        guid.0
                    );
                }
            }
            _ => {
                let Some(authored) = truth.get(&id) else {
                    panic!("{at}: member {} is not in the authored level", guid.0);
                };
                let error = (0..3)
                    .map(|a| (world_pos[a] - authored[a]).abs())
                    .fold(0.0f64, f64::max);
                assert!(
                    error <= STATIC_TOLERANCE_M,
                    "{at}: static member {} is {error} m from where the level put it ({world_pos:?} vs \
                     {authored:?})",
                    guid.0
                );
                seen.statics_checked += 1;
                seen.worst_static_m = seen.worst_static_m.max(error);
            }
        }
    }
    let physics = world.resource::<regia_physics::PhysicsWorld>();
    for (id, local, handle) in handles {
        let Some(body) = physics.bodies.get(handle.to_handle()) else {
            continue;
        };
        let p = body.translation();
        // The bodies drop 3.5 m onto the flat ground at y 0 and rest on it (radius 0.5).
        if p.y < 0.0 {
            let fixed: Vec<String> = physics
                .colliders
                .iter()
                .filter(|(_, c)| {
                    c.parent()
                        .and_then(|b| physics.bodies.get(b))
                        .is_none_or(|b| !b.is_dynamic())
                })
                .map(|(_, c)| format!("{:?} {:?}", c.shape().shape_type(), c.compute_aabb()))
                .collect();
            panic!(
                "{at}: body {id:032x} fell through the ground: physics ({}, {}, {}), world {:?}, origin cell \
                 {:?}; {} colliders, fixed ones {fixed:?}",
                p.x,
                p.y,
                p.z,
                origin.to_world([p.x, p.y, p.z]),
                origin.0.cell(),
                physics.colliders.len()
            );
        }
        let gap = Vec3::new(p.x, p.y, p.z).distance(local);
        assert!(
            gap < BODY_TOLERANCE_M,
            "{at}: body {id:032x}: Transform {local} vs physics ({}, {}, {}), {gap} m apart",
            p.x,
            p.y,
            p.z
        );
        seen.bodies_checked += 1;
        seen.worst_body_m = seen.worst_body_m.max(gap);
    }
    *last_body_world = bodies_now;
    // Static colliders, streamed or always loaded (the ground's static body): the physics collider
    // sits on the entity's `Transform`, and that is where the level put it.
    let mut colliders = world.query::<(&EntityGuid, &Transform, &ColliderHandle)>();
    let mut fixed: Vec<(u128, Vec3, ColliderHandle)> = Vec::new();
    for (guid, transform, handle) in colliders.iter(world) {
        let id = guid.0.as_u128();
        if guid_kind(id) == 3 {
            continue;
        }
        if let Some(authored) = truth.get(&id) {
            let world_pos = origin.to_world(transform.translation.to_array());
            let error = (0..3)
                .map(|a| (world_pos[a] - authored[a]).abs())
                .fold(0.0f64, f64::max);
            assert!(
                error <= STATIC_TOLERANCE_M,
                "{at}: static collider entity {} is {error} m from where the level put it",
                guid.0
            );
        }
        fixed.push((id, transform.translation, *handle));
    }
    let ground = GROUND_GUID.parse::<Guid>().expect("guid").as_u128();
    seen.ground_collider = fixed.iter().any(|(id, ..)| *id == ground);
    let physics = world.resource::<regia_physics::PhysicsWorld>();
    for (id, local, handle) in fixed {
        let Some(collider) = physics.colliders.get(handle.to_handle()) else {
            panic!("{at}: static collider {id:032x} has a handle but no physics collider");
        };
        let p = collider.translation();
        let gap = Vec3::new(p.x, p.y, p.z).distance(local);
        assert!(
            gap < 1.0e-3,
            "{at}: static collider {id:032x}: Transform {local} vs physics ({}, {}, {}), {gap} m apart",
            p.x,
            p.y,
            p.z
        );
        seen.colliders_checked += 1;
    }
    seen
}

/// Every spawned cell holds exactly the entities its cook wrote, and nothing is spawned for a cell
/// that is not.
fn assert_cells_whole(app: &mut App, at: &str) {
    let mut per_cell: BTreeMap<CellCoord, u32> = BTreeMap::new();
    let world = app.world_mut();
    let mut members = world.query::<&CellMember>();
    for member in members.iter(world) {
        *per_cell.entry(member.0).or_default() += 1;
    }
    let streaming = app.world().resource::<WorldStreaming>();
    for (cell, state) in streaming.cells(level()) {
        let have = per_cell.remove(&cell).unwrap_or(0);
        let Some(record) = streaming.record(level(), cell) else {
            continue;
        };
        match state {
            CellState::Spawned | CellState::SimReady | CellState::Visible => assert_eq!(
                have, record.entity_count,
                "{at}: cell {cell:?} ({state:?}) holds {have} of its {} entities",
                record.entity_count
            ),
            CellState::Unloaded | CellState::Queued | CellState::Loading | CellState::Failed(_) => {
                assert_eq!(
                    have, 0,
                    "{at}: cell {cell:?} is {state:?} but has {have} entities"
                );
            }
            // Part-spawned or part-despawned, and any state added later.
            _ => {}
        }
    }
    assert!(
        per_cell.is_empty(),
        "{at}: members of cells the runtime does not track: {per_cell:?}"
    );
}

/// The flight's queue and memory bounds, every step.
fn assert_bounded(app: &App, budgets: &StreamBudgets, at: &str) {
    let now = stats(app);
    assert!(
        now.resident_bytes_estimate <= budgets.memory_budget_bytes,
        "{at}: {now:?}"
    );
    assert!(now.inflight <= budgets.max_inflight, "{at}: {now:?}");
    assert!(now.pending_bytes <= budgets.max_pending_bytes, "{at}: {now:?}");
    assert!(
        now.last_apply_entities <= budgets.apply_budget_entities,
        "{at}: {now:?}"
    );
    assert_eq!(now.failures, 0, "{at}: {now:?}");
    assert_eq!(now.lockstep_stalls, 0, "{at}: {now:?}");
}

/// A nav mesh around `center` (world), inserted in local space like any game's.
fn nav_mesh_at(app: &mut App, center: [f64; 3]) -> (regia_core::glam::DVec3, regia_core::glam::DVec3) {
    let local = Vec3::from_array(
        app.world()
            .resource::<WorldOrigin>()
            .from_world(center)
            .expect("local"),
    );
    let corner = |x: f32, z: f32| local + Vec3::new(x, 0.0, z);
    let mut params = regia_nav::NavMeshBuildParams {
        cell_size: 0.5,
        ..Default::default()
    };
    params.agent.radius = 0.0;
    let mesh = regia_nav::NavMeshBuilder::new(params)
        .build(&[
            [corner(-10.0, -10.0), corner(10.0, -10.0), corner(10.0, 10.0)],
            [corner(-10.0, -10.0), corner(10.0, 10.0), corner(-10.0, 10.0)],
        ])
        .expect("nav mesh");
    let origin = *app.world().resource::<WorldOrigin>();
    let truth = (
        regia_core::glam::DVec3::from_array(origin.to_world(mesh.min.to_array())),
        regia_core::glam::DVec3::from_array(origin.to_world(mesh.max.to_array())),
    );
    app.insert_resource(mesh);
    truth
}

fn assert_nav_kept(app: &App, truth: (regia_core::glam::DVec3, regia_core::glam::DVec3), at: &str) {
    let origin = *app.world().resource::<WorldOrigin>();
    let mesh = app.world().resource::<regia_nav::NavMesh>();
    for (local, truth) in [(mesh.min, truth.0), (mesh.max, truth.1)] {
        let world = origin.to_world(local.to_array());
        let error = (0..3).map(|a| (world[a] - truth[a]).abs()).fold(0.0f64, f64::max);
        assert!(
            error <= STATIC_TOLERANCE_M,
            "{at}: the nav mesh moved {error} m in world space ({world:?} vs {truth:?})"
        );
    }
}

/// Spec 78 § 18 tests 5 and 9 on the cooked 8 km² world: two teleports with no warning, the second
/// one step after the first, so the first target's loads are queued or in flight when the source
/// leaves. Then a prepared teleport back, which loads the first target's cells for real.
#[test]
fn teleports_cancel_far_loads_and_every_transform_agrees_through_each_rebase() {
    let text = level_text(SEED);
    let truth = authored_positions(&text);
    assert!(truth.len() > 100_000, "{} authored positions", truth.len());
    let cooked = cook("stream_live_teleports", &text);
    let (cx, cz) = (
        cooked.index.bounds[1].x - cooked.index.bounds[0].x + 1,
        cooked.index.bounds[1].z - cooked.index.bounds[0].z + 1,
    );
    let area_km2 = (cx as f64 * CELL) * (cz as f64 * CELL) / 1.0e6;
    assert!(area_km2 > 8.0, "the world is {area_km2} km²");
    let budgets = cooked.index.settings.effective_budgets();

    let mut app = headless_app();
    app.add_plugins(regia_nav::NavPlugin);
    app.world_mut()
        .resource_mut::<WorldStreaming>()
        .set_timing(StreamTiming::Lockstep { latency_steps: 2 });
    open(&mut app, &cooked);
    let start = Flight::START;
    let source = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(start[0] as f32, start[1] as f32, start[2] as f32)),
            StreamingSource {
                kind: regia_scene::streaming::SourceKind::Player,
                drives_origin: true,
                view_bias: 0.0,
                ..StreamingSource::default()
            },
        ))
        .id();
    let mut bodies = HashMap::new();
    // Once the ground has its collider (physics builds it in the first fixed steps), it keeps it.
    let mut ground_since: Option<String> = None;
    let mut step = |app: &mut App, bodies: &mut Bodies, at: &str| -> Seen {
        app.update();
        assert_bounded(app, &budgets, at);
        let seen = check_members(app, &truth, bodies, at);
        match (&ground_since, seen.ground_collider) {
            (None, true) => ground_since = Some(at.to_owned()),
            (Some(since), false) => {
                panic!("{at}: the ground lost its physics collider (had it since {since})")
            }
            _ => {}
        }
        seen
    };
    let ready = |app: &App, at: [f64; 3]| {
        let gates = app.world().resource::<StreamGates>();
        app.world()
            .resource::<WorldStreaming>()
            .ready_near(gates, at, 2.0 * CELL, Readiness::SimReady)
    };
    let settle = |app: &mut App, bodies: &mut Bodies, at: [f64; 3], name: &str, step: &mut StepFn<'_>| {
        let mut n = 0u32;
        while !(ready(app, at) && stats(app).inflight == 0) {
            n += 1;
            assert!(n < 3000, "{name}: never ready: {:?}", stats(app));
            step(app, bodies, &format!("{name} wait {n}"));
        }
        // The far cells' unload: one plan interval and the despawn run.
        for i in 0..UNLOAD_STEPS {
            step(app, bodies, &format!("{name} unload {i}"));
        }
        n
    };

    // Open at the start; the nav mesh goes at the far target, 3.3 km away.
    let open_wait = settle(&mut app, &mut bodies, start, "open", &mut step);
    assert!(
        step(&mut app, &mut bodies, "after open").ground_collider,
        "the always-loaded ground has a physics collider once the level is open"
    );
    let nav_truth = nav_mesh_at(&mut app, Flight::TARGET);
    assert_cells_whole(&mut app, "after open");
    let at_open = stats(&app);

    // Teleport 1 (no warning) to the target, one step, then teleport 2 (no warning) to the middle.
    // Each leg is longer than twice the unload radius (a teleport), and the middle and the return
    // to the target end further than the origin threshold (1536 m) from the origin before them, so
    // each of those rebases.
    let middle = [400.0, 1.7, 1200.0];
    place(&mut app, source, Flight::TARGET);
    step(&mut app, &mut bodies, "teleport 1");
    let mid_flight = stats(&app);
    let queued_or_loading = app
        .world()
        .resource::<WorldStreaming>()
        .cells(level())
        .filter(|(_, s)| matches!(s, CellState::Queued | CellState::Loading | CellState::Applying))
        .count();
    assert!(
        queued_or_loading > 0,
        "the target's cells are queued or in flight one step after the teleport: {mid_flight:?}"
    );
    place(&mut app, source, middle);
    let middle_wait = settle(&mut app, &mut bodies, middle, "middle", &mut step);
    let at_middle = stats(&app);
    assert!(
        at_middle.loads_cancelled > at_open.loads_cancelled,
        "the second teleport cancels the first one's loads: {at_open:?} -> {at_middle:?}"
    );
    assert!(at_middle.teleports >= at_open.teleports + 2, "{at_middle:?}");
    assert_cells_whole(&mut app, "after the middle settles");
    // Nothing of the first target is left: every resident cell is within the unload radius.
    let grid = cooked.index.settings.grid().expect("grid");
    let unload = cooked.index.settings.unload_radius_m;
    let far: Vec<CellCoord> = app
        .world()
        .resource::<WorldStreaming>()
        .cells(level())
        .filter(|(_, s)| *s != CellState::Unloaded)
        .map(|(c, _)| c)
        .filter(|c| {
            let [x, z] = grid.cell_center(*c);
            (x - middle[0]).hypot(z - middle[2]) > unload + CELL
        })
        .collect();
    assert!(far.is_empty(), "cells left far from the source: {far:?}");

    // Teleport 3, prepared: back to the target, which now loads for real.
    let pin = app
        .world_mut()
        .resource_mut::<WorldStreaming>()
        .prepare_teleport(Flight::TARGET)
        .expect("pin");
    let mut prepared = 0u32;
    while !ready(&app, Flight::TARGET) {
        prepared += 1;
        assert!(prepared < 3000, "the target never got ready: {:?}", stats(&app));
        step(&mut app, &mut bodies, &format!("prepare {prepared}"));
    }
    app.world_mut().resource_mut::<WorldStreaming>().unpin(pin);
    place(&mut app, source, Flight::TARGET);
    let target_wait = settle(&mut app, &mut bodies, Flight::TARGET, "target", &mut step);
    assert_cells_whole(&mut app, "after the target settles");
    let end = stats(&app);
    assert!(
        at_middle.rebases > at_open.rebases && end.rebases > at_middle.rebases,
        "the middle and the return to the target each rebase: {at_open:?} -> {at_middle:?} -> {end:?}"
    );
    assert_nav_kept(&app, nav_truth, "at the target");
    // The bodies near the target dropped and lie still, where physics has them.
    let mut last = 0;
    let mut seen = Seen::default();
    for i in 0..120 {
        seen = step(&mut app, &mut bodies, &format!("rest {i}"));
        last = seen.bodies_checked;
    }
    assert!(last > 0, "bodies near the target were checked against physics");
    assert!(seen.statics_checked > 1000, "{seen:?}");
    println!(
        "teleports: open wait {open_wait}, middle wait {middle_wait}, prepare {prepared}, target wait \
         {target_wait} steps; cancelled {}, stale {}, rebases {}, teleports {}; last check {seen:?}",
        end.loads_cancelled, end.stale_discards, end.rebases, end.teleports
    );
}

/// A small streamed level whose props carry entity-owned lightmap data: a `LightmapReceiver` entry
/// per prop, a `LightmapSurface` and `GiMobility::Static`.
fn lightmap_level() -> String {
    const TRANSFORM: &str = "regia_scene::transform::Transform";
    const PRIMITIVE: &str = "regia_render::primitives::Primitive";
    const RECEIVER: &str = "regia_render::lightmap::LightmapReceiver";
    const SURFACE: &str = "regia_scene::components::LightmapSurface";
    const MOBILITY: &str = "regia_scene::components::GiMobility";
    const STREAMING: &str = "regia_scene::streaming::LevelStreaming";
    let mut out = String::from("(format_version: 1, entities: [\n");
    for cx in 0..12u32 {
        for prop in 0..3u32 {
            let index = cx * 3 + prop + 1;
            let x = f64::from(cx) * CELL + 16.0 + f64::from(prop) * 24.0;
            let density = 0.5 + f64::from(index) * 0.25;
            let _ = writeln!(
                out,
                "(guid: \"{}\", components: {{ \"{TRANSFORM}\": (translation: ({x:.1}, 1.0, 16.0)), \
                 \"{PRIMITIVE}\": (format_version: 1, shape: Cuboid(size: (1.0, 1.0, 1.0)), color: (0.4, \
                 0.4, 0.5, 1.0), roughness: 0.7, metallic: 0.0), \"{RECEIVER}\": (entry: \"prop_{index}\"), \
                 \"{SURFACE}\": (density_scale: {density:.2}, receive: {}), \"{MOBILITY}\": Static }}),",
                lightmap_guid(index),
                prop != 2
            );
        }
    }
    let _ = write!(
        out,
        "], resources: {{ \"{STREAMING}\": (enabled: true, cell_size_m: {CELL:.1}, load_radius_m: 256.0, \
         unload_radius_m: 320.0, hlod: None, origin: (enabled: false, cell_size_m: 1024.0, threshold_m: \
         1536.0)) }})"
    );
    out
}

fn lightmap_guid(index: u32) -> String {
    format!("5712ea32-0000-4000-8000-{index:012x}")
}

/// Every streamed prop's entity-owned lightmap data, by guid.
fn lightmap_data(app: &mut App) -> BTreeMap<u128, (CellCoord, String, LightmapSurface, GiMobility)> {
    let world = app.world_mut();
    let mut q = world.query::<(
        &EntityGuid,
        &CellMember,
        Option<&regia_render::lightmap::LightmapReceiver>,
        Option<&LightmapSurface>,
        Option<&GiMobility>,
    )>();
    q.iter(world)
        .map(|(guid, member, receiver, surface, mobility)| {
            (
                guid.0.as_u128(),
                (
                    member.0,
                    receiver.map(|r| r.entry.clone()).unwrap_or_default(),
                    surface.copied().unwrap_or(LightmapSurface {
                        density_scale: 0.0,
                        receive: false,
                    }),
                    mobility.copied().unwrap_or(GiMobility::Dynamic),
                ),
            )
        })
        .collect()
}

/// Spec 78 § 21 keeps lightmaps with the entities that own them: a cell's receivers, their entry
/// names, surfaces and mobility are the same after it unloads and loads again.
#[test]
fn lightmap_receivers_come_back_with_their_cell() {
    let cooked = cook("stream_live_lightmaps", &lightmap_level());
    let mut app = headless_app();
    app.world_mut()
        .resource_mut::<WorldStreaming>()
        .set_timing(StreamTiming::Lockstep { latency_steps: 2 });
    open(&mut app, &cooked);
    let home = [64.0, 1.7, 64.0];
    let source = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(64.0, 1.7, 64.0)),
            StreamingSource::default(),
        ))
        .id();
    let home_cell = CellCoord::new(0, 0);
    let settle = |app: &mut App, want: fn(Option<CellState>) -> bool, name: &str| {
        let mut n = 0;
        loop {
            app.update();
            n += 1;
            assert!(n < 2000, "{name}: {:?}", stats(app));
            let state = app.world().resource::<WorldStreaming>().state(level(), home_cell);
            if want(state) && stats(app).inflight == 0 {
                break;
            }
        }
        for _ in 0..UNLOAD_STEPS {
            app.update();
        }
    };
    settle(
        &mut app,
        |s| matches!(s, Some(CellState::SimReady | CellState::Visible)),
        "first load",
    );
    let first = lightmap_data(&mut app);
    let home_props: BTreeMap<_, _> = first.iter().filter(|(_, v)| v.0 == home_cell).collect();
    assert_eq!(home_props.len(), 3, "{first:?}");
    for (guid, (_, entry, surface, mobility)) in &home_props {
        let index = (**guid & 0xffff_ffff_ffff) as u32;
        assert_eq!(
            entry,
            &format!("prop_{index}"),
            "the cook keeps the receiver entry"
        );
        assert!((f64::from(surface.density_scale) - (0.5 + f64::from(index) * 0.25)).abs() < 1.0e-6);
        assert_eq!(*mobility, GiMobility::Static);
    }

    // Away, past the unload radius: the home cell's props go.
    place(&mut app, source, [1400.0, 1.7, 64.0]);
    settle(
        &mut app,
        |s| matches!(s, None | Some(CellState::Unloaded)),
        "unload",
    );
    let away = lightmap_data(&mut app);
    assert!(
        away.keys().all(|g| !home_props.contains_key(g)),
        "the home cell's props are gone"
    );

    // Back: the same receivers, entries and surfaces.
    place(&mut app, source, home);
    settle(
        &mut app,
        |s| matches!(s, Some(CellState::SimReady | CellState::Visible)),
        "reload",
    );
    let again = lightmap_data(&mut app);
    let again_home: BTreeMap<_, _> = again.iter().filter(|(_, v)| v.0 == home_cell).collect();
    assert_eq!(again_home, home_props, "the reloaded cell's lightmap data");
    assert_eq!(stats(&app).failures, 0);
}

/// One frame's measurements of the live flight.
#[derive(Default)]
struct LiveLog {
    frame_ms: Vec<f64>,
    gpu_peak_bytes: u64,
    rss_peak_bytes: u64,
    vram_estimate_peak: u64,
    resident_proxies_peak: usize,
    holes_after_teleport: u64,
    memory_samples: u32,
    /// The last frame's cut: `(node, proxy)`.
    last_cut: BTreeSet<(u32, bool)>,
    /// Cut entries that came or went between two frames: HLOD transitions.
    hlod_transitions: u64,
    /// The frames (by index) whose cut had a hole.
    hole_frames: Vec<usize>,
}

/// The flight with the real renderer (spec 78 § 18 acceptance A and B, burn-down row C01). The path
/// is `Flight`'s, sampled every `STREAM_LIVE_STRIDE`-th step (default 8 on lavapipe, 1 on a card)
/// at `STREAM_LIVE_SIZE` (default 320x180). The declared GPU cap is the tier named by
/// `STREAM_LIVE_TIER` (default `minimum`, DO27's minimum card).
#[test]
fn live_flight_holds_the_memory_caps_with_real_proxy_residency() {
    use regia_render::{
        BudgetTier, Camera, DirectionalLight, MemoryBudget, RenderDevice, RenderTarget, TextureViewSettings,
    };
    use std::time::Instant;

    let Ok(device) = RenderDevice::headless() else {
        eprintln!("live_flight: no GPU adapter; skipped");
        return;
    };
    let env_u32 = |name: &str, default: u32| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let stride = env_u32("STREAM_LIVE_STRIDE", 8).max(1);
    let size: Vec<u32> = std::env::var("STREAM_LIVE_SIZE")
        .unwrap_or_else(|_| "320x180".to_owned())
        .split('x')
        .filter_map(|v| v.parse().ok())
        .collect();
    let (width, height) = (
        size.first().copied().unwrap_or(320),
        size.get(1).copied().unwrap_or(180),
    );
    let tier_name = std::env::var("STREAM_LIVE_TIER").unwrap_or_else(|_| "minimum".to_owned());
    let tier = BudgetTier::from_name(&tier_name).expect("STREAM_LIVE_TIER: minimum, deck or desktop_high");
    let cap = MemoryBudget::for_tier(tier);
    let adapter = device.adapter.get_info();

    let text = level_text(SEED);
    let cooked = cook("stream_live_flight", &text);
    let budgets = cooked.index.settings.effective_budgets();
    let total_proxies = cooked
        .index
        .hlod
        .as_ref()
        .map_or(0, |h| h.node_proxies.iter().filter(|p| p.is_some()).count());
    assert!(total_proxies > 0, "the cook wrote proxies");

    let mut app = App::new();
    app.insert_resource(device);
    app.add_plugins((
        regia_app::MinimalPlugins,
        regia_render::RenderPlugin { headless: true },
        ScenePlugin,
        PhysicsPlugin,
        WorldStreamingPlugin,
    ));
    regia_app::enable_fixed_step(&mut app, stream_world::STEP_HZ);
    app.world_mut()
        .resource_mut::<WorldStreaming>()
        .set_timing(StreamTiming::Lockstep { latency_steps: 2 });
    open(&mut app, &cooked);
    app.world_mut().spawn(DirectionalLight {
        direction: [0.4, -1.0, 0.2],
        color: [1.0, 1.0, 1.0],
        illuminance: 60_000.0,
        cast_shadows: false,
        static_shadow: false,
    });
    let eye = |p: [f64; 3]| [p[0], p[1] + 18.0, p[2]];
    let start = eye(Flight::START);
    let camera = app
        .world_mut()
        .spawn((
            Transform {
                translation: Vec3::new(start[0] as f32, start[1] as f32, start[2] as f32),
                rotation: regia_core::math::Quat::from_rotation_x(-0.35),
                ..Transform::default()
            },
            Camera {
                target: RenderTarget::Texture { asset_id: 0x57E6 },
                exposure_ev: Some(14.0),
                far: 6000.0,
                ..Camera::default()
            },
            TextureViewSettings {
                size: [width, height],
                ..TextureViewSettings::default()
            },
            StreamingSource {
                drives_origin: true,
                ..StreamingSource::default()
            },
        ))
        .id();

    let mut log = LiveLog::default();
    let frame = |app: &mut App, log: &mut LiveLog, at: &str| {
        let began = Instant::now();
        app.update();
        log.frame_ms.push(began.elapsed().as_secs_f64() * 1000.0);
        assert_bounded(app, &budgets, at);
        let now = stats(app);
        assert!(
            !now.vram_over_budget,
            "{at}: the streamed cells' VRAM estimate {} is over the level's budget {}",
            now.resident_vram_estimate, budgets.vram_budget_bytes
        );
        log.vram_estimate_peak = log.vram_estimate_peak.max(now.resident_vram_estimate);
        let resident = app.world().resource::<HlodProxyResidency>().resident.len();
        log.resident_proxies_peak = log.resident_proxies_peak.max(resident);
        let cut = app.world().resource::<HlodCut>();
        let now: BTreeSet<(u32, bool)> = cut.entries.iter().map(|e| (e.node, e.proxy)).collect();
        if !log.last_cut.is_empty() {
            log.hlod_transitions += now.symmetric_difference(&log.last_cut).count() as u64;
        }
        if cut.holes > 0 {
            log.hole_frames.push(log.frame_ms.len() - 1);
        }
        log.last_cut = now;
        if log.frame_ms.len().is_multiple_of(16) {
            sample_memory(app, log, cap, at);
        }
    };
    let ready = |app: &App, at: [f64; 3], readiness: Readiness| {
        let gates = app.world().resource::<StreamGates>();
        app.world()
            .resource::<WorldStreaming>()
            .ready_near(gates, at, 2.0 * CELL, readiness)
    };

    // The open wait, until the start is drawable.
    let mut n = 0u32;
    while !(ready(&app, Flight::START, Readiness::Visible) && stats(&app).pending_waits == 0) {
        n += 1;
        assert!(n < 4000, "open: never visible: {:?}", stats(&app));
        frame(&mut app, &mut log, "open");
    }
    let open_frames = log.frame_ms.len();
    let mut teleport_frames = 0u32;
    let mut after_teleport = false;
    let mut flight_step = 0u32;
    let mut holes_at_teleport = 0;
    let mut drive_frames: (usize, usize) = (0, 0);
    loop {
        let target = match Flight.step(flight_step) {
            FlightStep::End => break,
            FlightStep::At(p) => p,
            FlightStep::Teleport(target) => {
                drive_frames = (open_frames, log.frame_ms.len());
                let pin = app
                    .world_mut()
                    .resource_mut::<WorldStreaming>()
                    .prepare_teleport(target)
                    .expect("pin");
                while !ready(&app, target, Readiness::Visible) {
                    teleport_frames += 1;
                    assert!(
                        teleport_frames < 6000,
                        "teleport: never visible: {:?}",
                        stats(&app)
                    );
                    frame(&mut app, &mut log, "teleport wait");
                }
                app.world_mut().resource_mut::<WorldStreaming>().unpin(pin);
                after_teleport = true;
                holes_at_teleport = stats(&app).holes;
                target
            }
        };
        place(&mut app, camera, eye(target));
        frame(&mut app, &mut log, &format!("flight step {flight_step}"));
        if after_teleport {
            let cut = app.world().resource::<HlodCut>();
            assert_eq!(
                cut.holes, 0,
                "flight step {flight_step}: a hole after the teleport"
            );
        }
        // The teleport step itself is never skipped by the stride.
        let next = flight_step + stride;
        let teleport_at = Flight::WALK_STEPS + Flight::DRIVE_STEPS;
        flight_step = if flight_step < teleport_at && next > teleport_at {
            teleport_at
        } else {
            next
        };
    }
    log.holes_after_teleport = stats(&app).holes - holes_at_teleport;
    sample_memory(&mut app, &mut log, cap, "end");
    let end = stats(&app);
    let cut = app.world().resource::<HlodCut>();
    let proxies_drawn = cut.entries.iter().filter(|e| e.proxy).count();
    assert!(proxies_drawn > 0, "far cells draw through proxies: {end:?}");
    assert_eq!(log.holes_after_teleport, 0, "{end:?}");
    assert!(
        log.resident_proxies_peak < total_proxies,
        "the renderer made {} of {total_proxies} proxies resident: the far ones stay out",
        log.resident_proxies_peak
    );
    assert!(end.rebases >= 1, "{end:?}");
    assert!(end.teleports >= 1, "{end:?}");
    assert!(log.memory_samples > 2);
    // The cut changes as the flight crosses cells: proxies give way to full cells and back.
    assert!(
        log.hlod_transitions > 0,
        "the flight crossed cells without one HLOD transition"
    );

    // Measured, never asserted: the frame times of each part of the flight.
    let pct = |frames: &[f64], p: f64| {
        let mut v = frames.to_vec();
        v.sort_by(f64::total_cmp);
        v.get(((v.len() as f64 - 1.0) * p).round() as usize)
            .copied()
            .unwrap_or(0.0)
    };
    let part = |frames: &[f64]| {
        format!(
            "{{\"frames\":{},\"p50_ms\":{:.2},\"p95_ms\":{:.2},\"max_ms\":{:.2},\"over_33ms\":{}}}",
            frames.len(),
            pct(frames, 0.5),
            pct(frames, 0.95),
            frames.iter().copied().fold(0.0, f64::max),
            frames.iter().filter(|ms| **ms > 33.0).count()
        )
    };
    let flight = &log.frame_ms[drive_frames.0..drive_frames.1];
    let teleport_end = drive_frames.1 + teleport_frames as usize;
    let teleport = &log.frame_ms[drive_frames.1..teleport_end.min(log.frame_ms.len())];
    let last_leg = &log.frame_ms[teleport_end.min(log.frame_ms.len())..];
    let report = format!(
        "{{\"adapter\":\"{}\",\"backend\":\"{:?}\",\"width\":{width},\"height\":{height},\"stride\":{stride},\
         \"tier\":\"{}\",\"tier_vram_bytes\":{},\"tier_ram_bytes\":{},\"gpu_peak_bytes\":{},\"rss_peak_bytes\":{},\
         \"level_vram_budget_bytes\":{},\"vram_estimate_peak_bytes\":{},\"proxies\":{total_proxies},\
         \"resident_proxies_peak\":{},\"holes_after_teleport\":{},\"hole_frames_open\":{},\"hole_frames_walk_drive\":{},\
         \"hole_frames_after\":{},\"hlod_transitions\":{},\
         \"rebases\":{},\"loads_issued\":{},\
         \"loads_cancelled\":{},\"open\":{},\"walk_drive\":{},\"teleport_wait\":{},\"last_leg\":{}}}",
        adapter.name.replace('"', "'"),
        adapter.backend,
        tier.name(),
        cap.vram,
        cap.ram,
        log.gpu_peak_bytes,
        log.rss_peak_bytes,
        budgets.vram_budget_bytes,
        log.vram_estimate_peak,
        log.resident_proxies_peak,
        log.holes_after_teleport,
        log.hole_frames.iter().filter(|i| **i < open_frames).count(),
        log.hole_frames
            .iter()
            .filter(|i| (open_frames..drive_frames.1).contains(*i))
            .count(),
        log.hole_frames.iter().filter(|i| **i >= drive_frames.1).count(),
        log.hlod_transitions,
        end.rebases,
        end.loads_issued,
        end.loads_cancelled,
        part(&log.frame_ms[..open_frames]),
        part(flight),
        part(teleport),
        part(last_leg),
    );
    println!("{report}");
    if let Some(path) = std::env::var_os("STREAM_LIVE_JSON") {
        let tmp = std::path::PathBuf::from(&path).with_extension("tmp");
        std::fs::write(&tmp, &report).expect("write the report");
        std::fs::rename(&tmp, &path).expect("move the report into place");
    }
}

/// Reads the device's measured GPU memory and the process RSS and checks the GPU number against
/// the declared tier cap.
fn sample_memory(app: &mut App, log: &mut LiveLog, cap: regia_render::MemoryBudget, at: &str) {
    let report = regia_render::memory_report(app.world().resource::<regia_render::RenderDevice>());
    // The whole device's figure (ADR-5860): the allocator's live total on Vulkan and D3D12, the
    // Metal device's allocated size on Metal, where no per-allocation report exists.
    let Some(gpu_bytes) = report.device_allocated_bytes else {
        panic!(
            "{at}: the {} backend reports no allocated size, so GPU memory is not measured",
            report.backend
        );
    };
    assert!(
        gpu_bytes <= cap.vram,
        "{at}: GPU memory {gpu_bytes} is over the declared cap {}",
        cap.vram
    );
    log.gpu_peak_bytes = log.gpu_peak_bytes.max(gpu_bytes);
    log.rss_peak_bytes = log.rss_peak_bytes.max(report.rss_bytes.unwrap_or(0));
    log.memory_samples += 1;
}
