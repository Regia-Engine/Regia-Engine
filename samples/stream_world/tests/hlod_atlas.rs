// SPDX-License-Identifier: MIT-0
//! ADR-2483, the renderer half of the baked HLOD proxy atlas: a cell cooked for real, its proxy loaded
//! by the renderer's own hook and drawn in a real frame, on whatever `RenderDevice::headless` finds
//! (lavapipe on the gate). Pixels are measured, never compared to a golden; the images go to
//! `HLOD_ATLAS_SHOTS` (or the project directory) for review. The look is the owner's.
//!
//! Before / after for the owner, the generated 8 km² world at HLOD distance, atlas off and on:
//!
//! ```text
//! HLOD_ATLAS_SHOTS=/tmp/hlod_shots cargo test -p stream_world --test hlod_atlas -- --ignored --nocapture
//! ```

use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::{AssetServer, AssetServerSettings};
use regia_asset_pipeline::stream_cook::{CacheAssets, cook_streamed_level, register_cook_types};
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3};
use regia_ecs::reflect::{AppTypeRegistry, TypeRegistry};
use regia_physics::PhysicsPlugin;
use regia_render::{Camera, DirectionalLight, RenderDevice, RenderTarget, TextureViewSettings};
use regia_scene::ScenePlugin;
use regia_scene::Transform;
use regia_scene::format::SceneFile;
use regia_scene::level::{LevelId, LoadMode};
use regia_scene::streaming::{HlodCut, HlodProxyMesh, HlodProxyResidency, StreamingSource};
use regia_world::format::WorldIndex;
use regia_world::{StreamTiming, WorldOrigin, WorldStreaming, WorldStreamingPlugin};

const TARGET: u64 = 0x57E5;
/// The measured frame.
const SMALL: [u32; 2] = [320, 180];
/// The frames for the owner's eye.
const LARGE: [u32; 2] = [1280, 720];

fn level() -> LevelId {
    LevelId(Guid::from_u128(0x5712_ea32))
}

fn registry() -> TypeRegistry {
    let mut app = App::new();
    app.add_plugins(ScenePlugin)
        .add_plugins(PhysicsPlugin)
        .add_plugins(WorldStreamingPlugin);
    regia_render::register_render_types(&mut app);
    let handle = app.world().resource::<AppTypeRegistry>().0.clone();
    let mut registry = TypeRegistry::default();
    std::mem::swap(&mut registry, &mut *handle.write());
    register_cook_types(&mut registry);
    registry
}

/// A level cooked into a project directory.
struct Cooked {
    dir: std::path::PathBuf,
    index: WorldIndex,
    always: regia_scene::cooked::CookedScene,
}

fn cook(name: &str, text: &str) -> Cooked {
    let dir = regia_test_utils::unique_temp_dir(name);
    let platform = regia_asset_pipeline::host_platform();
    let cache = regia_asset::cooked_cache_dir(&dir, platform);
    std::fs::create_dir_all(&cache).expect("cache dir");
    let scene = SceneFile::from_ron_str(text).expect("level parses");
    let assets = CacheAssets::new(&cache);
    let started = std::time::Instant::now();
    let output = cook_streamed_level(
        level(),
        &scene,
        &registry(),
        &assets,
        Some(&cache),
        "assets/world.level.ron",
    )
    .expect("cook")
    .expect("streamed");
    let bytes: u64 = std::fs::read_dir(&cache)
        .expect("cache")
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum();
    // Information for the report only, nothing asserts on it.
    println!(
        "{name}: cooked in {:.1} s, {} proxies, {} proxy bytes, {bytes} bytes in the cache",
        started.elapsed().as_secs_f64(),
        output.report.proxies,
        output.report.proxy_bytes,
    );
    let read = |id| std::fs::read(cache.join(regia_asset::cooked_file_name(id))).expect("cooked file");
    let index =
        WorldIndex::from_bytes(&read(regia_world::format::world_index_id(level())), level()).expect("index");
    let always = regia_world::format::always_loaded_from_bytes(
        &read(regia_world::format::always_loaded_id(level())),
        level(),
    )
    .expect("always");
    Cooked { dir, index, always }
}

/// What a frame of a cooked level showed.
struct Frame {
    size: [u32; 2],
    pixels: Vec<u8>,
    /// Proxies in the cut.
    proxies: usize,
    /// Proxy entities that came with an atlas material.
    with_atlas: usize,
}

/// One frame from `eye` looking along `rotation`, with every wanted proxy resident.
fn frame(cooked: &Cooked, size: [u32; 2], eye: [f64; 3], rotation: Quat, sun: [f32; 3]) -> Option<Frame> {
    let Ok(device) = RenderDevice::headless() else {
        eprintln!("hlod_atlas: no GPU adapter; skipped");
        return None;
    };
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
    app.world_mut().spawn(DirectionalLight {
        direction: sun,
        color: [1.0, 1.0, 1.0],
        illuminance: 60_000.0,
        cast_shadows: false,
        static_shadow: false,
    });
    let local = app
        .world()
        .resource::<WorldOrigin>()
        .from_world(eye)
        .expect("local");
    app.world_mut().spawn((
        Transform {
            translation: Vec3::from_array(local),
            rotation,
            ..Transform::default()
        },
        Camera {
            target: RenderTarget::Texture { asset_id: TARGET },
            exposure_ev: Some(14.0),
            far: 6000.0,
            ..Camera::default()
        },
        TextureViewSettings {
            size,
            ..TextureViewSettings::default()
        },
        StreamingSource {
            drives_origin: false,
            ..StreamingSource::default()
        },
    ));
    // The cut settles, and every proxy it wants has been read by the renderer's worker.
    let mut settled = 0;
    for step in 0..6000 {
        app.update();
        let proxies = app
            .world()
            .resource::<HlodCut>()
            .entries
            .iter()
            .filter(|e| e.proxy)
            .count();
        let world = app.world_mut();
        let spawned = world.query::<&HlodProxyMesh>().iter(world).count();
        let resident = world.resource::<HlodProxyResidency>().resident.len();
        settled = if proxies > 0 && spawned > 0 && resident >= spawned {
            settled + 1
        } else {
            0
        };
        if settled >= 8 {
            break;
        }
        assert!(
            step < 5999,
            "the proxies never settled: {proxies} cut, {spawned} spawned, {resident} resident"
        );
        std::thread::yield_now();
    }
    let proxies = app
        .world()
        .resource::<HlodCut>()
        .entries
        .iter()
        .filter(|e| e.proxy)
        .count();
    let world = app.world_mut();
    let with_atlas = world
        .query::<&HlodProxyMesh>()
        .iter(world)
        .filter(|proxy| proxy.material.is_some())
        .count();
    let pixels = world.resource_scope(|world, device: Mut<RenderDevice>| {
        let mut runtime = world.resource_mut::<regia_render::RuntimeRenderer>();
        let image = runtime.0.read_texture_target(&device, TARGET).expect("frame");
        assert_eq!([image.width, image.height], size);
        image.pixels
    });
    Some(Frame {
        size,
        pixels,
        proxies,
        with_atlas,
    })
}

fn save(cooked: &Cooked, name: &str, frame: &Frame) {
    let shots = std::env::var_os("HLOD_ATLAS_SHOTS").map_or_else(|| cooked.dir.clone(), Into::into);
    std::fs::create_dir_all(&shots).expect("shots dir");
    let file = shots.join(format!("{name}.png"));
    let [width, height] = frame.size;
    image::save_buffer(
        &file,
        &frame.pixels,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )
    .expect("png");
    println!(
        "{name}: {} proxies in the cut, {} with an atlas, {}",
        frame.proxies,
        frame.with_atlas,
        file.display()
    );
}

/// Two walls in the far cell, red on the left and green on the right, and a gray box near the camera.
fn two_walls(atlas_px: u32) -> String {
    const PRIMITIVE: &str = "regia_render::primitives::Primitive";
    const TRANSFORM: &str = "regia_scene::transform::Transform";
    const STREAMING: &str = "regia_scene::streaming::LevelStreaming";
    let wall = |id: u32, x: f32, color: &str| {
        format!(
            "(guid: \"{}\", components: {{ \"{TRANSFORM}\": (translation: ({x}, 14.0, 32.0)), \
             \"{PRIMITIVE}\": (format_version: 1, shape: Cuboid(size: (28.0, 28.0, 4.0)), color: ({color}, 1.0), \
             roughness: 0.8, metallic: 0.0) }}),",
            Guid::from_u128(0x5712_0000 + u128::from(id)),
        )
    };
    format!(
        "(format_version: 1, entities: [{}{}(guid: \"{}\", components: {{ \"{TRANSFORM}\": (translation: (32.0, \
         1.0, 290.0)), \"{PRIMITIVE}\": (format_version: 1, shape: Cuboid(size: (1.0, 1.0, 1.0)), color: (0.5, \
         0.5, 0.5, 1.0), roughness: 0.8, metallic: 0.0) }}),], resources: {{ \"{STREAMING}\": (enabled: true, \
         hlod_atlas_px: {atlas_px}, cell_size_m: 64.0, load_radius_m: 128.0, unload_radius_m: 160.0) }})",
        wall(1, 16.0, "0.9, 0.04, 0.04"),
        wall(2, 48.0, "0.04, 0.9, 0.04"),
        Guid::from_u128(0x5712_0003),
    )
}

/// Pixels whose `a` channel clearly leads channel `b`, with their mean column.
fn dominant(pixels: &[u8], a: usize, b: usize) -> (usize, f32) {
    let (mut count, mut columns) = (0usize, 0u64);
    for (i, p) in pixels.as_chunks::<4>().0.iter().enumerate() {
        if p[a] > 40 && u32::from(p[a]) > 2 * u32::from(p[b]) {
            count += 1;
            columns += (i as u64) % u64::from(SMALL[0]);
        }
    }
    (
        count,
        if count > 0 {
            columns as f32 / count as f32
        } else {
            0.0
        },
    )
}

/// The renderer draws a cooked atlas: the far cell's flat proxy shows one mixed color, its baked proxy
/// shows the red wall on the left and the green wall on the right.
#[test]
fn a_baked_proxy_draws_each_source_color_where_the_flat_proxy_draws_one() {
    let eye = [32.0, 14.0, 252.0];
    let sun = [-0.2, -0.4, -1.0];

    let flat_cook = cook("hlod_atlas_off", &two_walls(0));
    let Some(flat) = frame(&flat_cook, SMALL, eye, Quat::IDENTITY, sun) else {
        return;
    };
    save(&flat_cook, "two_walls_flat", &flat);
    assert!(flat.proxies > 0, "the far cell draws through its proxy");
    assert_eq!(flat.with_atlas, 0);
    let (red, _) = dominant(&flat.pixels, 0, 1);
    let (green, _) = dominant(&flat.pixels, 1, 0);
    assert!(
        red < 20 && green < 20,
        "a flat proxy is one mixed color: {red} red and {green} green pixels"
    );

    let baked_cook = cook("hlod_atlas_on", &two_walls(4));
    assert!(
        baked_cook
            .index
            .hlod
            .as_ref()
            .is_some_and(|hlod| hlod.node_atlases.iter().any(Option::is_some)),
        "the cook baked an atlas"
    );
    let baked =
        frame(&baked_cook, SMALL, eye, Quat::IDENTITY, sun).expect("the device was there a moment ago");
    save(&baked_cook, "two_walls_baked", &baked);
    assert!(baked.proxies > 0, "the far cell draws through its proxy");
    assert!(baked.with_atlas > 0, "the proxy carries its atlas material");
    let (red, red_column) = dominant(&baked.pixels, 0, 1);
    let (green, green_column) = dominant(&baked.pixels, 1, 0);
    println!("baked: {red} red pixels at column {red_column}, {green} green at {green_column}");
    assert!(red > 200, "the red wall shows through the atlas: {red} pixels");
    assert!(
        green > 200,
        "the green wall shows through the atlas: {green} pixels"
    );
    assert!(
        red_column < green_column,
        "each color is on its own wall: red at column {red_column}, green at {green_column}"
    );
    let _ = std::fs::remove_dir_all(&flat_cook.dir);
    let _ = std::fs::remove_dir_all(&baked_cook.dir);
}

/// The owner's before / after: the generated world from the flight's start, flat and baked, looking
/// along the world at its far cells. Takes minutes; writes the images and prints the cook costs.
#[test]
#[ignore = "before / after images for the owner: see the module docs"]
fn before_and_after_of_the_generated_world() {
    let eye = [
        stream_world::Flight::START[0],
        30.0,
        stream_world::Flight::START[2],
    ];
    // Looking along +Z, slightly down, where the world runs on for kilometers.
    let rotation = Quat::from_rotation_y(std::f32::consts::PI) * Quat::from_rotation_x(-0.06);
    let sun = [0.4, -1.0, 0.2];
    let text = stream_world::level_text(0x5eed);
    let flat_cook = cook("hlod_world_off", &text);
    let Some(flat) = frame(&flat_cook, LARGE, eye, rotation, sun) else {
        return;
    };
    save(&flat_cook, "world_flat", &flat);
    for px in [2u32, 8] {
        let name = format!("world_baked_{px}");
        let baked_text = text.replace(
            "(enabled: true, cell_size_m:",
            &format!("(enabled: true, hlod_atlas_px: {px}, cell_size_m:"),
        );
        let baked_cook = cook(&format!("hlod_world_{px}"), &baked_text);
        let baked = frame(&baked_cook, LARGE, eye, rotation, sun).expect("the device was there a moment ago");
        save(&baked_cook, &name, &baked);
        let _ = std::fs::remove_dir_all(&baked_cook.dir);
    }
    let _ = std::fs::remove_dir_all(&flat_cook.dir);
}
