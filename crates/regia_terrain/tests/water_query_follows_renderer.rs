// SPDX-License-Identifier: MPL-2.0
//! Queries that follow the drawn surface follow what the renderer actually drew (ADR-3251), on a
//! real GPU (lavapipe in CI).
//!
//! On a device with a 128 MiB storage binding (the mobile minimum-spec profile) an ocean with a
//! 1024 grid and a lingering-foam history does not fit: the renderer refuses it with a named
//! error and draws its swell fallback, and `WaterQuery` with `queries_follow_tier` answers that
//! fallback. On the full lavapipe device the same ocean is drawn, and answered. No wall clock.

use regia_app::prelude::*;
use regia_ecs::ecs::system::SystemState;
use regia_render::minspec::MinspecProfile;
use regia_render::{LayerCamera, RenderDevice};
use regia_terrain::WaterState;
use regia_terrain::water_body::plugin::sync_water;
use regia_terrain::water_body::{
    OceanGridError, WaterDrawnOceans, WaterQuery, cache_drawn_water, check_ocean_grid,
};
use regia_terrain::{FftOcean, WaterBody, WaterClock, WaterLook, WaterRenderer, WaterSettings, WaterTier};

const W: u32 = 64;
const H: u32 = 36;

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn sea() -> WaterBody {
    let mut body = WaterBody::plane([0.0, 0.0, 0.0], [400.0, 400.0], WaterLook::ocean());
    body.ocean = Some(FftOcean {
        grid: 1024,
        foam_decay: 8.0,
        ..FftOcean::default()
    });
    body
}

fn camera() -> LayerCamera {
    LayerCamera {
        eye: [0.0, 6.0, -10.0],
        target: [0.0, 0.0, 10.0],
        fov_y: 1.0,
        near: 0.1,
        far: 500.0,
        width: W,
        height: H,
        exposure: 1.0,
        display_exposure: 1.0,
        hdr: true,
        sun_direction: [0.3, 0.8, 0.2],
        sun_color: [1.0; 3],
        sun_illuminance: 80_000.0,
        sky_ambient: None,
    }
}

fn depth(device: &RenderDevice) -> wgpu::TextureView {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("query test depth"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

/// One frame of the water layer (the ocean selection runs in it).
fn frame(device: &RenderDevice, water: &WaterRenderer, depth: &wgpu::TextureView) {
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    water.layer().simulate(depth, &camera(), &mut encoder);
    device.queue.submit([encoder.finish()]);
}

/// The world the plugin would keep: the settings (Ultra, queries follow the tier), the clock, the
/// renderer and one ocean body. Returns the world and the body.
fn world_with(device: &RenderDevice) -> (World, Entity) {
    let mut world = World::new();
    world.insert_resource(WaterSettings {
        tier: Some(WaterTier::Ultra),
        queries_follow_tier: true,
        ..WaterSettings::default()
    });
    world.insert_resource(WaterClock {
        tick: 321,
        ..WaterClock::default()
    });
    world.init_resource::<WaterDrawnOceans>();
    world.insert_resource(WaterState {
        renderer: WaterRenderer::new(device),
        tier: WaterTier::Ultra,
    });
    let body = world.spawn(sea()).id();
    (world, body)
}

/// Runs the plugin's sync, one water frame, the sync again (it reads the frame's report) and the
/// cache; returns the height the query answers at a few points.
fn follow(device: &RenderDevice) -> (World, Entity, Vec<f32>) {
    let (mut world, body) = world_with(device);
    let sync = world.register_system(sync_water);
    let cache = world.register_system(cache_drawn_water);
    world.run_system(sync).expect("sync");
    let water = world.resource::<WaterState>().renderer.clone();
    let depth = depth(device);
    frame(device, &water, &depth);
    world.run_system(sync).expect("sync");
    world.run_system(cache).expect("cache");
    world.flush();
    let mut state: SystemState<WaterQuery> = SystemState::new(&mut world);
    let query = state.get(&world).expect("query");
    let heights = [(3.0, 4.0), (-17.5, 22.0), (40.0, -8.25)]
        .iter()
        .map(|&(x, z)| query.surface(x, z).expect("wet").1.height)
        .collect();
    (world, body, heights)
}

fn heights_of(body: &WaterBody, ocean_drawn: bool) -> Vec<f32> {
    let clock = WaterClock {
        tick: 321,
        ..WaterClock::default()
    };
    let drawn = body.drawn_with(WaterTier::Ultra, ocean_drawn);
    [(3.0, 4.0), (-17.5, 22.0), (40.0, -8.25)]
        .iter()
        .map(|&(x, z)| drawn.surface(x, z, &clock).expect("wet").height)
        .collect()
}

/// The size check names the limit a grid exceeds; 1024 with a history needs a binding above
/// 151 MB, more than a 128 MiB device allows, and fits without the history.
#[test]
fn the_grid_check_names_the_limit_on_a_128_mib_device() {
    let limits = MinspecProfile::Mobile.clamp_limits(&wgpu::Limits {
        max_storage_buffer_binding_size: 1 << 31,
        max_buffer_size: 1 << 32,
        ..wgpu::Limits::default()
    });
    assert_eq!(limits.max_storage_buffer_binding_size, 128 << 20);
    match check_ocean_grid(&limits, 1024, true) {
        Err(OceanGridError::StorageBinding {
            grid: 1024,
            history: true,
            needed,
            limit,
        }) => {
            // 6 map layers and 3 history layers of 1024² RGBA32F texels: 151 MB.
            assert!(needed == 150_994_944 && limit == 128 << 20, "{needed} {limit}");
        }
        other => panic!("expected a storage-binding refusal, got {other:?}"),
    }
    let refused = check_ocean_grid(&limits, 1024, true).expect_err("refused");
    assert_eq!(refused.code(), "storage_binding");
    assert!(refused.to_string().contains("storage binding"), "{refused}");
    assert_eq!(check_ocean_grid(&limits, 1024, false), Ok(()));
    assert_eq!(check_ocean_grid(&limits, 512, true), Ok(()));
}

/// On the 128 MiB device the renderer refuses the ocean (named in its stats), reports it as not
/// drawn, and the following query answers the swell fallback; on the full device the ocean is
/// drawn and the query answers it.
#[test]
fn queries_follow_the_renderers_ocean_fallback_on_a_128_mib_device() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let small = match RenderDevice::headless_with_profile(MinspecProfile::Mobile) {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    assert!(small.device.limits().max_storage_buffer_binding_size <= 128 << 20);
    let (world, body, heights) = follow(&small);
    let water = &world.resource::<WaterState>().renderer;
    let stats = water.stats();
    assert!(
        matches!(
            stats.ocean_refused,
            Some(OceanGridError::StorageBinding { grid: 1024, .. })
        ),
        "{stats:?}"
    );
    let mut drawn = Vec::new();
    water.oceans_drawn_into(&mut drawn);
    assert_eq!(drawn, [false]);
    assert_eq!(world.resource::<WaterDrawnOceans>().swell_fallback, [body]);
    assert_eq!(
        water
            .check_ocean_grid(&sea().ocean.unwrap_or_default(), 1024)
            .map_err(|e| e.code()),
        Err("storage_binding")
    );
    let swell = heights_of(&sea(), false);
    let ocean = heights_of(&sea(), true);
    assert_ne!(swell, ocean, "the test points must tell the two surfaces apart");
    assert_eq!(
        heights, swell,
        "the query must answer the swell the renderer drew"
    );

    let full = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    if full.device.limits().max_storage_buffer_binding_size < 152_000_000 {
        // A device as small as the profile proves nothing more here.
        return;
    }
    let (world, _, heights) = follow(&full);
    let water = &world.resource::<WaterState>().renderer;
    assert_eq!(water.stats().ocean_refused, None);
    let mut drawn = Vec::new();
    water.oceans_drawn_into(&mut drawn);
    assert_eq!(drawn, [true]);
    assert!(world.resource::<WaterDrawnOceans>().swell_fallback.is_empty());
    assert_eq!(
        heights, ocean,
        "the query must answer the ocean the renderer drew"
    );
}
