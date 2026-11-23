// SPDX-License-Identifier: MIT-0
//! Spec 78 § 18 acceptance A, headless: the generated 8 km² world cooked for real, flown in lockstep.
//! No wall clock anywhere: counters and fixed steps only. The render half is `flight_on_lavapipe`.

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
use regia_scene::components::EntityGuid;
use regia_scene::format::SceneFile;
use regia_scene::level::{LevelId, LoadMode};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{
    CellMember, HlodProxyMesh, HlodProxyResidency, Readiness, StreamBudgets, StreamGates, StreamGuarded,
    StreamingSource,
};
use regia_world::format::WorldIndex;
use regia_world::{CellState, StreamStats, StreamTiming, WorldOrigin, WorldStreaming, WorldStreamingPlugin};
use stream_world::{CELL, Flight, FlightStep, PROPS_PER_CELL, level_text};

fn level() -> LevelId {
    LevelId(Guid::from_u128(0x5712_ea31))
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

/// The tight budgets of the flight: every wanted cell fits, with little to spare.
fn tight_budgets(index: &mut WorldIndex) {
    let largest = index.cells.iter().map(|c| c.own_bytes).max().unwrap_or(0);
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

fn cook(name: &str) -> Cooked {
    let dir = regia_test_utils::unique_temp_dir(name);
    let platform = regia_asset_pipeline::host_platform();
    let cache = regia_asset::cooked_cache_dir(&dir, platform);
    std::fs::create_dir_all(&cache).expect("cache dir");
    let scene = SceneFile::from_ron_str(&level_text(0x5eed)).expect("generated level parses");
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
    assert_eq!(output.report.cells, 512, "{:?}", output.report);
    // A prop rounded onto the far edge counts for the next cell.
    assert!(
        output.report.entities_min >= PROPS_PER_CELL - 10,
        "{:?}",
        output.report
    );
    assert!(output.report.proxies >= 512);
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

/// What one flight measured.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    /// `(trace_hash, world_hash, members, member state hash)` every 100th step.
    hashes: Vec<(u64, u64, usize, u64)>,
    rebases: u64,
    teleports_waited: u32,
}

/// An order-independent hash of every member's guid and pose: bodies moved by physics included.
fn world_hash(app: &mut App) -> (usize, u64) {
    let world = app.world_mut();
    let mut query = world.query::<(&EntityGuid, &Transform, &CellMember)>();
    let mut sum = 0u64;
    let mut count = 0;
    for (guid, transform, _) in query.iter(world) {
        let t = transform.translation;
        let mixed = (guid.0.as_u128() as u64)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(u64::from(t.x.to_bits()))
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(u64::from(t.y.to_bits()))
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(u64::from(t.z.to_bits()));
        sum = sum.wrapping_add(mixed);
        count += 1;
    }
    (count, sum)
}

fn every_proxy_is_resident(proxies: Query<&HlodProxyMesh>, mut residency: ResMut<HlodProxyResidency>) {
    for proxy in &proxies {
        residency.resident.insert(proxy.node);
    }
}

/// Flies the world and checks every budget at every step.
fn fly(cooked: &Cooked) -> Outcome {
    let mut app = headless_app();
    // No renderer here: stands in for the proxy loads, which the lavapipe test does for real.
    app.add_systems(
        PostUpdate,
        every_proxy_is_resident.before(regia_scene::streaming::StreamSet::Hlod),
    );
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
    let budgets = cooked.index.settings.effective_budgets();
    regia_world::open_streamed_from(
        app.world_mut(),
        level(),
        LoadMode::Replace,
        cooked.index.clone(),
        &cooked.always,
        Arc::new(regia_world::PayloadCellSource::new(reader)),
    )
    .expect("open");

    let start = Vec3::new(
        Flight::START[0] as f32,
        Flight::START[1] as f32,
        Flight::START[2] as f32,
    );
    let source = app
        .world_mut()
        .spawn((
            Transform::from_translation(start),
            StreamingSource {
                kind: regia_scene::streaming::SourceKind::Player,
                drives_origin: true,
                view_bias: 0.0,
                ..StreamingSource::default()
            },
            StreamGuarded,
        ))
        .id();

    // The open wait: the guard may hold the source here, and only here.
    let mut waited = 0;
    loop {
        app.update();
        waited += 1;
        assert!(waited < 2000, "the open wait never ended: {:?}", stats(&app));
        if stats(&app).pending_waits == 0 {
            break;
        }
    }
    let frozen_at_open = stats(&app).source_frozen_steps;
    let holes_at_open = stats(&app).holes;

    let grid = cooked.index.settings.grid().expect("grid");
    let mut path: BTreeSet<CellCoord> = BTreeSet::new();
    let mut seen_visible: BTreeSet<CellCoord> = BTreeSet::new();
    let mut hashes = Vec::new();
    let mut teleports_waited = 0;
    let mut step = 0u32;
    let check = |app: &App, seen: &mut BTreeSet<CellCoord>, at: u32| {
        let now = stats(app);
        assert!(
            now.resident_bytes_estimate <= budgets.memory_budget_bytes,
            "step {at}: {now:?}"
        );
        assert!(now.inflight <= budgets.max_inflight, "step {at}: {now:?}");
        assert!(
            now.pending_bytes <= budgets.max_pending_bytes,
            "step {at}: {now:?}"
        );
        assert!(
            now.last_apply_entities <= budgets.apply_budget_entities,
            "step {at}: {now:?}"
        );
        assert_eq!(now.lockstep_stalls, 0, "step {at}");
        assert_eq!(now.failures, 0, "step {at}: {now:?}");
        for (cell, state) in app.world().resource::<WorldStreaming>().cells(level()) {
            if state == CellState::Visible {
                seen.insert(cell);
            }
        }
    };
    loop {
        let target = match Flight.step(step) {
            FlightStep::End => break,
            FlightStep::At(position) => position,
            FlightStep::Teleport(target) => {
                let pin = app
                    .world_mut()
                    .resource_mut::<WorldStreaming>()
                    .prepare_teleport(target)
                    .expect("pin");
                loop {
                    app.update();
                    teleports_waited += 1;
                    check(&app, &mut seen_visible, step);
                    assert!(
                        teleports_waited < 3000,
                        "the target never got ready: {:?}",
                        stats(&app)
                    );
                    let gates = app.world().resource::<StreamGates>();
                    let ready = app.world().resource::<WorldStreaming>().ready_near(
                        gates,
                        target,
                        2.0 * CELL,
                        Readiness::SimReady,
                    );
                    if ready {
                        break;
                    }
                }
                app.world_mut().resource_mut::<WorldStreaming>().unpin(pin);
                target
            }
        };
        if let Ok(cell) = grid.cell_of(target[0], target[2]) {
            path.insert(cell);
        }
        let local = app
            .world()
            .resource::<WorldOrigin>()
            .from_world(target)
            .expect("local");
        app.world_mut()
            .get_mut::<Transform>(source)
            .expect("source")
            .translation = Vec3::from_array(local);
        app.update();
        check(&app, &mut seen_visible, step);
        if step.is_multiple_of(100) {
            let (members, pose_hash) = world_hash(&mut app);
            let now = stats(&app);
            hashes.push((now.trace_hash, now.world_hash, members, pose_hash));
        }
        step += 1;
    }

    let end = stats(&app);
    assert_eq!(end.holes - holes_at_open, 0, "holes after the open wait: {end:?}");
    assert_eq!(end.source_frozen_steps, frozen_at_open, "{end:?}");
    assert_eq!(end.apply_over_budget, 0, "{end:?}");
    println!("flight: {end:?}, {} path cells", path.len());
    let missed: Vec<_> = path.difference(&seen_visible).collect();
    assert!(missed.is_empty(), "cells on the path never visible: {missed:?}");
    assert!(path.len() > 10, "{path:?}");
    assert!(end.rebases >= 1, "the flight crosses a rebase: {end:?}");
    assert!(end.teleports >= 1, "{end:?}");
    Outcome {
        hashes,
        rebases: end.rebases,
        teleports_waited,
    }
}

fn stats(app: &App) -> StreamStats {
    app.world().resource::<StreamStats>().clone()
}

#[test]
fn the_flight_holds_every_budget_and_two_runs_are_identical() {
    let cooked = cook("stream_world_flight");
    assert!(
        cooked.index.bounds[1].x - cooked.index.bounds[0].x + 1 == 16
            && cooked.index.bounds[1].z - cooked.index.bounds[0].z + 1 == 32,
        "2048 m x 4096 m in 128 m cells: {:?}",
        cooked.index.bounds
    );
    let first = fly(&cooked);
    let second = fly(&cooked);
    assert_eq!(first, second);
    assert!(first.hashes.len() > 20);
}

/// One stop of the render flight.
fn wait_visible(app: &mut App, around: [f64; 3], radius: f64) -> u32 {
    let mut steps = 0;
    loop {
        app.update();
        steps += 1;
        assert!(steps < 4000, "never visible: {:?}", stats(app));
        let gates = app.world().resource::<StreamGates>();
        let ready =
            app.world()
                .resource::<WorldStreaming>()
                .ready_near(gates, around, radius, Readiness::Visible);
        if ready && stats(app).pending_waits == 0 {
            return steps;
        }
        std::thread::yield_now();
    }
}

/// The render half of acceptance A at 320 x 180: the real `render` gate, real proxy loads and a real
/// frame at each stop, on whatever `RenderDevice::headless` finds (lavapipe on the gate). Images go to
/// the project directory, or `STREAM_WORLD_SHOTS`; they are for review, never compared.
#[test]
fn flight_on_lavapipe() {
    use regia_render::{Camera, DirectionalLight, RenderDevice, RenderTarget, TextureViewSettings};

    let Ok(device) = RenderDevice::headless() else {
        eprintln!("flight_on_lavapipe: no GPU adapter; skipped");
        return;
    };
    const TARGET: u64 = 0x57E4;
    let cooked = cook("stream_world_render");
    let ground_terrain = rolling_terrain(&device);
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
        level(),
        LoadMode::Replace,
        cooked.index.clone(),
        &cooked.always,
        Arc::new(regia_world::PayloadCellSource::new(reader)),
    )
    .expect("open");
    // The world's terrain, drawn by the renderer's shared pass. It stays in its own space across the
    // rebase of this flight (spec 78 § 12.2, ADR-2022).
    let (terrain_draw, terrain) = ground_terrain;
    app.world_mut().spawn(terrain);
    app.world_mut()
        .resource_mut::<regia_render::RuntimeRenderer>()
        .0
        .set_shared_draw(Some(Box::new(terrain_draw)));
    app.world_mut().spawn(DirectionalLight {
        direction: [0.4, -1.0, 0.2],
        color: [1.0, 1.0, 1.0],
        illuminance: 60_000.0,
        cast_shadows: false,
        static_shadow: false,
    });
    let eye = |p: [f64; 3]| Vec3::new(p[0] as f32, p[1] as f32 + 18.0, p[2] as f32);
    let camera = app
        .world_mut()
        .spawn((
            Transform {
                translation: eye(Flight::START),
                rotation: regia_core::math::Quat::from_rotation_x(-0.35),
                ..Transform::default()
            },
            Camera {
                target: RenderTarget::Texture { asset_id: TARGET },
                exposure_ev: Some(14.0),
                far: 6000.0,
                ..Camera::default()
            },
            TextureViewSettings {
                size: [320, 180],
                ..TextureViewSettings::default()
            },
            StreamingSource {
                drives_origin: true,
                ..StreamingSource::default()
            },
        ))
        .id();

    let shots = std::env::var_os("STREAM_WORLD_SHOTS").map_or_else(|| cooked.dir.clone(), Into::into);
    std::fs::create_dir_all(&shots).expect("shots dir");
    let stops = [("start", Flight::START), ("target", Flight::TARGET)];
    for (name, stop) in stops {
        let local = app
            .world()
            .resource::<WorldOrigin>()
            .from_world([stop[0], stop[1] + 18.0, stop[2]])
            .expect("local");
        app.world_mut()
            .get_mut::<Transform>(camera)
            .expect("camera")
            .translation = Vec3::from_array(local);
        wait_visible(&mut app, stop, CELL);
        // A few more frames: the cut settles and the renderer has the meshes.
        for _ in 0..8 {
            app.update();
        }
        let extracted = app.world().resource::<regia_render::ExtractedFrame>();
        let draws = extracted.meshes.len();
        let cut = app.world().resource::<regia_scene::streaming::HlodCut>();
        let proxies = cut.entries.iter().filter(|e| e.proxy).count();
        let fulls = cut.entries.len() - proxies;
        println!(
            "{name}: {draws} draws, cut {fulls} full + {proxies} proxies, {:?}",
            stats(&app)
        );
        assert!(draws > 100, "{name}: {draws}");
        assert!(proxies > 0, "{name}: far cells draw through proxies");
        assert!(fulls > 0, "{name}: near cells draw in full");
        assert_eq!(cut.holes, 0, "{name}");
        assert!(
            draws < (fulls + 1) * (PROXY_DRAW_CEILING),
            "{name}: members behind proxies are hidden, not drawn: {draws}"
        );
        let world = app.world_mut();
        world.resource_scope(|world, device: Mut<RenderDevice>| {
            let mut runtime = world.resource_mut::<regia_render::RuntimeRenderer>();
            let image = runtime.0.read_texture_target(&device, TARGET).expect("frame");
            assert_eq!((image.width, image.height), (320, 180));
            let distinct: BTreeSet<&[u8]> = image.pixels.chunks(4).collect();
            assert!(distinct.len() > 16, "{name}: a flat image");
            // The terrain's hue shows at both stops, on either side of the rebase.
            let orange = image
                .pixels
                .chunks(4)
                .filter(|p| p[0] > p[2].saturating_add(24))
                .count();
            assert!(orange > 320 * 180 / 50, "{name}: {orange} terrain-colored pixels");
            let file = shots.join(format!("stream_world_{name}.png"));
            image::save_buffer(&file, &image.pixels, 320, 180, image::ExtendedColorType::Rgba8).expect("png");
            println!("{name}: {} colors, {}", distinct.len(), file.display());
        });
    }
    let end = stats(&app);
    assert!(end.rebases >= 1, "{end:?}");
    assert_eq!(
        end.rebases_blocked, 0,
        "a terrain does not hold the origin back: {end:?}"
    );
    assert_eq!(end.failures, 0, "{end:?}");
}

/// The acceptance world's terrain: rolling ground a little above the level's flat collider, orange so
/// the frames can tell it from the props, wide enough for the whole world (the map is centered on the
/// world origin and the world starts there).
fn rolling_terrain(
    device: &regia_render::RenderDevice,
) -> (regia_terrain::TerrainSharedDraw, regia_terrain::Terrain) {
    use regia_terrain::{CdlodConfig, LayerDesc, Terrain, TerrainLayers, TerrainMap, TerrainSharedDraw};
    const SAMPLES: u32 = 513;
    const WIDTH: f32 = 8192.0;
    let spacing = WIDTH / (SAMPLES - 1) as f32;
    let heights: Vec<f32> = (0..SAMPLES)
        .flat_map(|j| (0..SAMPLES).map(move |i| (i as f32 * spacing, j as f32 * spacing)))
        .map(|(x, z)| 3.0 + 2.0 * (x / 97.0).sin() * (z / 131.0).cos())
        .collect();
    let map = TerrainMap::from_heights(SAMPLES, SAMPLES, heights, WIDTH, 64.0, -16.0).expect("map");
    let heightfield = map.to_heightfield();
    let layers = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [0.7, 0.35, 0.1],
            roughness: 1.0,
            tiling: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            blend_sharpness: 0.15,
            triplanar: false,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let config = CdlodConfig::default();
    let root_level = 5;
    let terrain = Terrain::new(
        &heightfield,
        WIDTH,
        64.0,
        -16.0,
        config.clone(),
        root_level,
        layers.clone(),
    )
    .expect("terrain component");
    let draw = TerrainSharedDraw::new(device, map, &layers, config, root_level).expect("terrain");
    (draw, terrain)
}

/// Entities a full cell can add to the draw list: the per-cell props, the bodies and a margin.
const PROXY_DRAW_CEILING: usize = PROPS_PER_CELL as usize + 60;

/// Acceptance B's teleport bench (spec 78 § 18, `cargo xtask stream-bench`): async timing, real time,
/// a 2 km teleport with no warning, on the card the environment selects. Measures, never asserts:
/// the time until every cell in reach is `Visible` and the frames over 33 ms during the fill. Prints
/// one JSON line, and writes it to `STREAM_BENCH_JSON` when set. `STREAM_BENCH_SIZE=WxH` sets the view
/// (default 2560x1440). `STREAM_BENCH_BOUNDS=1` turns on the VRAM planner bound and the render
/// residency wait (ADR-2317, both off by default) to compare the fill with them.
///
/// The line also carries the numbers a VRAM bound is chosen from (ADR-2952): the platform column of
/// the budgets (`SteamDeck=1` picks the Deck's), its `vram_budget_bytes`, the cook's VRAM estimate of
/// the largest cell and of all cells, the peak and final `resident_vram_estimate` over the settle and
/// the fill, the frames that ran over the budget, and the device's allocated and reserved GPU bytes
/// and the process RSS after the fill.
#[test]
#[ignore = "a measurement for an exclusive GPU: cargo xtask stream-bench"]
fn teleport_bench() {
    use regia_render::{Camera, DirectionalLight, RenderDevice, RenderTarget, TextureViewSettings};
    use std::time::Instant;

    let device = RenderDevice::headless().expect("a GPU");
    let size: Vec<u32> = std::env::var("STREAM_BENCH_SIZE")
        .unwrap_or_else(|_| "2560x1440".to_owned())
        .split('x')
        .filter_map(|v| v.parse().ok())
        .collect();
    let (width, height) = (
        size.first().copied().unwrap_or(2560),
        size.get(1).copied().unwrap_or(1440),
    );
    let cooked = cook("stream_world_bench");
    let mut app = App::new();
    app.insert_resource(device);
    app.add_plugins((
        regia_app::MinimalPlugins,
        regia_render::RenderPlugin { headless: true },
        ScenePlugin,
        PhysicsPlugin,
        WorldStreamingPlugin,
    ));
    let server = AssetServer::open(AssetServerSettings {
        project_root: cooked.dir.clone(),
        platform: regia_asset_pipeline::host_platform().to_string(),
        content_dir: None,
    })
    .expect("server");
    let reader = server.payload_reader();
    app.insert_resource(server);
    let mut index = cooked.index.clone();
    index.settings.budgets = None;
    regia_world::open_streamed_from(
        app.world_mut(),
        level(),
        LoadMode::Replace,
        index,
        &cooked.always,
        Arc::new(regia_world::PayloadCellSource::new(reader)),
    )
    .expect("open");
    // `STREAM_BENCH_BOUNDS=1`: the opt-in VRAM planner bound and render residency wait (ADR-2317), so
    // the fill can be compared with the default.
    let bounds = std::env::var("STREAM_BENCH_BOUNDS").is_ok_and(|v| v == "1");
    if bounds {
        app.world_mut()
            .resource_mut::<WorldStreaming>()
            .set_vram_bound(true);
        app.world_mut()
            .resource_mut::<regia_render::StreamRenderResidency>()
            .enabled = true;
    }
    app.world_mut().spawn(DirectionalLight {
        direction: [0.4, -1.0, 0.2],
        color: [1.0, 1.0, 1.0],
        illuminance: 60_000.0,
        cast_shadows: false,
        static_shadow: false,
    });
    let start = Vec3::new(Flight::START[0] as f32, 19.7, Flight::START[2] as f32);
    let camera = app
        .world_mut()
        .spawn((
            Transform::from_translation(start),
            Camera {
                target: RenderTarget::Texture { asset_id: 0x57E5 },
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
    wait_visible(&mut app, Flight::START, CELL);
    // The resident VRAM estimate's peak, and the frames it ran over the level's budget.
    let (mut vram_peak, mut vram_over_frames) = (0u64, 0u32);
    let mut watch_vram = |app: &App| {
        let s = app.world().resource::<StreamStats>();
        vram_peak = vram_peak.max(s.resident_vram_estimate);
        vram_over_frames += u32::from(s.vram_over_budget);
    };
    // The settled frames: the steady cost with nothing to load, and how many already exceed 33 ms
    // (a frame that is slow while idle is not the stream's doing).
    let settled: Vec<f64> = (0..60)
        .map(|_| {
            let frame = Instant::now();
            app.update();
            let ms = frame.elapsed().as_secs_f64() * 1000.0;
            watch_vram(&app);
            ms
        })
        .collect();
    let settle = settled.iter().sum::<f64>() / 60.0;
    let settle_over = settled.iter().filter(|ms| **ms > 33.0).count();

    let target = Flight::TARGET;
    let local = app
        .world()
        .resource::<WorldOrigin>()
        .from_world([target[0], 19.7, target[2]])
        .expect("local");
    app.world_mut()
        .get_mut::<Transform>(camera)
        .expect("camera")
        .translation = Vec3::from_array(local);
    let began = Instant::now();
    let (mut frames, mut over, mut worst) = (0u32, 0u32, 0.0f64);
    // Per fill frame: (ms, the stream's apply and plan time of that frame, loads issued in it).
    let mut samples: Vec<(f64, f64, f64, u64)> = Vec::new();
    let mut loads_before = stats(&app).loads_issued;
    let radius = cooked.index.settings.load_radius_m;
    let filled_s = loop {
        let frame = Instant::now();
        app.update();
        let ms = frame.elapsed().as_secs_f64() * 1000.0;
        watch_vram(&app);
        frames += 1;
        over += u32::from(ms > 33.0);
        worst = worst.max(ms);
        let now = stats(&app);
        samples.push((
            ms,
            f64::from(now.last_apply_ms),
            f64::from(now.last_plan_ms),
            now.loads_issued.saturating_sub(loads_before),
        ));
        loads_before = now.loads_issued;
        let gates = app.world().resource::<StreamGates>();
        let ready =
            app.world()
                .resource::<WorldStreaming>()
                .ready_near(gates, target, radius, Readiness::Visible);
        if ready {
            break began.elapsed().as_secs_f64();
        }
        assert!(frames < 100_000, "the fill never ended: {:?}", stats(&app));
    };
    let end = stats(&app);
    let platform = format!("{:?}", regia_scene::streaming::StreamPlatform::host()).to_lowercase();
    let vram_budget = app
        .world()
        .resource::<WorldStreaming>()
        .budgets(level())
        .map_or(0, |b| b.vram_budget_bytes);
    let cell_vram_max = cooked.index.cells.iter().map(|c| c.vram_bytes).max().unwrap_or(0);
    let cell_vram_total: u64 = cooked.index.cells.iter().map(|c| c.vram_bytes).sum();
    let gpu = regia_render::memory_report(app.world().resource::<RenderDevice>());
    // Attribution: the stream's own work (apply + plan) against the frame. A slow frame whose stream
    // share is small is owned by something else (render, physics, the headless app).
    let mut sorted: Vec<f64> = samples.iter().map(|s| s.0).collect();
    sorted.sort_by(f64::total_cmp);
    let pick = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
    let over66 = sorted.iter().filter(|ms| **ms > 66.0).count();
    let stream_share: f64 = samples.iter().map(|s| s.1 + s.2).sum::<f64>()
        / samples.iter().map(|s| s.0).sum::<f64>().max(f64::EPSILON);
    let mut slow: Vec<&(f64, f64, f64, u64)> = samples.iter().filter(|s| s.0 > 33.0).collect();
    slow.sort_by(|a, b| b.0.total_cmp(&a.0));
    let worst_five: Vec<String> = slow
        .iter()
        .take(5)
        .map(|s| {
            format!(
                "{{\"ms\":{:.1},\"apply_ms\":{:.3},\"plan_ms\":{:.3},\"loads\":{}}}",
                s.0, s.1, s.2, s.3
            )
        })
        .collect();
    let report = format!(
        "{{\"width\":{width},\"height\":{height},\"settled_frame_ms\":{settle:.3},\"fill_s\":{filled_s:.3},\
         \"fill_frames\":{frames},\"frames_over_33ms\":{over},\"worst_frame_ms\":{worst:.2},\"last_apply_ms\":{:.3},\
         \"last_plan_ms\":{:.3},\"loads_issued\":{},\"holes\":{},\"bounds\":{bounds},\"residency_timeouts\":{},\
         \"platform\":\"{platform}\",\"vram_budget_bytes\":{vram_budget},\"cell_vram_max_bytes\":{cell_vram_max},\
         \"cell_vram_total_bytes\":{cell_vram_total},\"resident_vram_peak_bytes\":{vram_peak},\
         \"resident_vram_end_bytes\":{},\"vram_over_budget_frames\":{vram_over_frames},\"resident_cells_end\":{},\
         \"gpu_allocated_bytes\":{},\"gpu_reserved_bytes\":{},\"rss_bytes\":{},\
         \"settled_over_33ms\":{settle_over},\"p50_ms\":{:.2},\"p95_ms\":{:.2},\"frames_over_66ms\":{over66},\
         \"stream_share\":{stream_share:.4},\"worst_five\":[{}]}}",
        end.last_apply_ms,
        end.last_plan_ms,
        end.loads_issued,
        end.holes,
        app.world()
            .get_resource::<regia_render::StreamRenderResidency>()
            .map_or(0, |r| r.timeouts),
        end.resident_vram_estimate,
        end.resident_cells,
        gpu.allocated_bytes,
        gpu.reserved_bytes,
        gpu.rss_bytes.unwrap_or(0),
        pick(0.5),
        pick(0.95),
        worst_five.join(","),
    );
    println!("{report}");
    if let Some(path) = std::env::var_os("STREAM_BENCH_JSON") {
        std::fs::write(path, report).expect("write the report");
    }
}
