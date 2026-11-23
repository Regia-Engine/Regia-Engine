// SPDX-License-Identifier: MPL-2.0
//! The FFT ocean's foam beyond the instant and its Low queries on a real GPU (ADR-2843; lavapipe in
//! CI): the lingering-foam history against its CPU mirror on fixed steps, its restarts, its memory
//! and dispatches only when it is on, the drawn foam it and the whitecap rescale add, and a floater
//! on Low against the vertices Low draws.
//!
//! No golden image: every check compares the GPU with the CPU mirror, or frames of the same run.

mod water_scene;

use regia_app::prelude::World;
use regia_ecs::ecs::system::SystemState;
use regia_render::{FrameRenderer, LayerCamera, RenderDevice};
use regia_terrain::water_body::ocean::{OCEAN_BANDS, OCEAN_QUERY_SIZE, frame_of};
use regia_terrain::water_body::ocean_foam::FoamHistory;
use regia_terrain::water_body::{WaterDebug, WaterFrameStats, WaterQuery};
use regia_terrain::{FftOcean, WaterBody, WaterClock, WaterLook, WaterRenderer, WaterSettings, WaterTier};
use water_scene::{Camera, STUDIO_SKY, StreamScene, frame, post, stream_scene};

const W: u32 = 256;
const H: u32 = 144;
const SUN: [f32; 3] = [0.35, -0.65, -0.55];
const LEVEL: f32 = -0.1;

/// Worst difference allowed between the GPU history and the CPU mirror. The maps agree within
/// about 1e-6 m (`gpu_maps_match_the_cpu_mirror`); the crest's smoothstep has a slope of at most
/// 1.5 / 0.3 times the band's normalization, and the history only carries it forward.
const HISTORY_BOUND: f32 = 2.0e-3;

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A windy sea whose foam lingers two seconds.
fn lingering() -> FftOcean {
    FftOcean {
        wind_speed: 16.0,
        foam_decay: 2.0,
        foam_growth: 0.25,
        ..FftOcean::default()
    }
}

fn sea(ocean: FftOcean, half: f32) -> WaterBody {
    let mut sea = WaterBody::plane([0.0, LEVEL, 0.0], [half, half], WaterLook::ocean());
    sea.ocean = Some(ocean);
    sea
}

fn clock(tick: u64) -> WaterClock {
    WaterClock {
        tick,
        ..WaterClock::default()
    }
}

/// Every history texel of `maps` against the mirror: the worst difference.
fn worst_history_error(maps: &regia_terrain::water_body::OceanMaps, cpu: &FoamHistory) -> f32 {
    let n = maps.size;
    let mut worst = 0.0f32;
    for b in 0..OCEAN_BANDS {
        for row in 0..n {
            for column in 0..n {
                let gpu = maps.history(b, column, row).expect("history layers");
                worst = worst.max((gpu - cpu.texel(b, column, row)).abs());
            }
        }
    }
    worst
}

/// On fixed steps of a sixtieth of a second, the GPU history follows the CPU mirror texel for
/// texel within [`HISTORY_BOUND`], foam lingers where the crests have gone, and the cost shows in
/// the stats: three more dispatches and the history's memory.
#[test]
fn history_matches_the_cpu_mirror_on_fixed_steps() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    let ocean = lingering();
    let body = sea(ocean, 400.0);
    water.set_tier(WaterTier::High);
    water.set_bodies(&[&body]);
    let mut cpu = FoamHistory::new(&ocean, OCEAN_QUERY_SIZE);
    let mut worst = 0.0f32;
    let mut lingered = 0usize;
    for tick in 600..660u64 {
        let c = clock(tick);
        water.set_clock(c);
        let maps = water
            .read_ocean_maps()
            .expect("readback")
            .expect("an ocean is drawn");
        assert_eq!(maps.size, OCEAN_QUERY_SIZE);
        let n = OCEAN_QUERY_SIZE as usize;
        assert_eq!(
            maps.texels.len(),
            3 * OCEAN_BANDS * n * n,
            "two map layers and one history layer per band"
        );
        let frame = frame_of(&ocean, c.seconds(), OCEAN_QUERY_SIZE);
        cpu.step(&frame, c.seconds());
        worst = worst.max(worst_history_error(&maps, &cpu));
        if tick == 659 {
            // Texels that hold foam no crest of this instant would give.
            let scales = regia_terrain::water_body::ocean_foam::band_foam_scales(&frame.tables);
            for ((g, scale), history) in frame.grids.iter().zip(scales).zip(&cpu.bands) {
                for (i, h) in history.iter().enumerate() {
                    let j = (1.0 + g.dxx[i]) * (1.0 + g.dzz[i]) - g.dxz[i] * g.dxz[i];
                    let now =
                        regia_terrain::water_body::ocean_foam::band_injection(j, scale, ocean.foam_onset);
                    if *h > now + 0.05 {
                        lingered += 1;
                    }
                }
            }
        }
    }
    eprintln!("history: worst GPU vs CPU {worst}, lingering texels {lingered}");
    assert!(worst < HISTORY_BOUND, "GPU history vs mirror: {worst}");
    assert!(lingered > 100, "foam lingers behind the crests: {lingered}");
    let stats = water.stats();
    let n = u64::from(OCEAN_QUERY_SIZE);
    assert_eq!(stats.ocean_history_bytes, 2 * 3 * n * n * 16, "{stats:?}");
    assert_eq!(stats.ocean_history_steps, 60, "{stats:?}");
    assert_eq!(
        stats.ocean_history_restarts, 1,
        "only the first step restarts: {stats:?}"
    );
    assert_eq!(stats.ocean_dispatches, 3 + 3 * 2 * 7 + 3, "{stats:?}");
}

/// The history restarts from no foam on a camera cut, on a change of the bodies and on a jump of
/// the clock, and the mirror restarted the same way agrees again.
#[test]
fn history_restarts_on_a_cut_a_body_change_and_a_jump() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    let ocean = lingering();
    let mut body = sea(ocean, 400.0);
    water.set_tier(WaterTier::High);
    water.set_bodies(&[&body]);
    water.set_camera_cuts(4);
    let mut cpu = FoamHistory::new(&ocean, OCEAN_QUERY_SIZE);
    let total = |maps: &regia_terrain::water_body::OceanMaps| -> f32 {
        let n = maps.size;
        (0..OCEAN_BANDS)
            .flat_map(|b| (0..n * n).map(move |i| (b, i)))
            .map(|(b, i)| maps.history(b, i % n, i / n).unwrap_or(0.0))
            .sum()
    };
    let step = |tick: u64, cpu: &mut FoamHistory| {
        let c = clock(tick);
        water.set_clock(c);
        let maps = water.read_ocean_maps().expect("readback").expect("ocean");
        cpu.step(&frame_of(&ocean, c.seconds(), OCEAN_QUERY_SIZE), c.seconds());
        assert!(worst_history_error(&maps, cpu) < HISTORY_BOUND, "tick {tick}");
        (total(&maps), water.stats())
    };
    for tick in 1000..1030 {
        step(tick, &mut cpu);
    }
    let (held, stats) = step(1030, &mut cpu);
    assert!(held > 1.0, "foam built up: {held}");
    assert_eq!(stats.ocean_history_restarts, 1, "{stats:?}");
    // The same count again is no cut; a new count is.
    water.set_camera_cuts(4);
    let (kept, stats) = step(1031, &mut cpu);
    assert!(
        kept > 1.0 && stats.ocean_history_restarts == 1,
        "{kept} {stats:?}"
    );
    water.set_camera_cuts(5);
    cpu.reset();
    let (cut, stats) = step(1032, &mut cpu);
    assert!(cut < 1.0e-6, "a cut restarts from no foam: {cut}");
    assert_eq!(stats.ocean_history_restarts, 2, "{stats:?}");
    for tick in 1033..1050 {
        step(tick, &mut cpu);
    }
    // A change of the bodies (here the look) restarts it.
    body.look.normal_strength *= 0.5;
    water.set_bodies(&[&body]);
    cpu.reset();
    let (changed, stats) = step(1050, &mut cpu);
    assert!(changed < 1.0e-6, "{changed}");
    assert_eq!(stats.ocean_history_restarts, 3, "{stats:?}");
    for tick in 1051..1060 {
        step(tick, &mut cpu);
    }
    // Ten seconds later in one step: longer than FOAM_MAX_STEP.
    let (jumped, stats) = step(1660, &mut cpu);
    assert!(jumped < 1.0e-6, "{jumped}");
    assert_eq!(stats.ocean_history_restarts, 4, "{stats:?}");
    // A host reset does the same.
    for tick in 1661..1670 {
        step(tick, &mut cpu);
    }
    water.reset_ocean_foam();
    cpu.reset();
    let (reset, stats) = step(1670, &mut cpu);
    assert!(reset < 1.0e-6, "{reset}");
    assert_eq!(stats.ocean_history_restarts, 5, "{stats:?}");
}

/// A default ocean allocates and dispatches nothing for the history, and the cost measurement
/// shows what turning it on adds at each grid.
#[test]
fn history_is_allocated_only_when_on() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    let plain = FftOcean {
        wind_speed: 16.0,
        ..FftOcean::default()
    };
    water.set_tier(WaterTier::High);
    water.set_bodies(&[&sea(plain, 400.0)]);
    water.set_clock(clock(600));
    let maps = water.read_ocean_maps().expect("readback").expect("ocean");
    let n = OCEAN_QUERY_SIZE as usize;
    assert_eq!(maps.texels.len(), 2 * OCEAN_BANDS * n * n);
    assert_eq!(maps.history(0, 0, 0), None);
    let stats = water.stats();
    assert_eq!(stats.ocean_history_bytes, 0, "{stats:?}");
    assert_eq!(stats.ocean_history_steps, 0, "{stats:?}");
    assert_eq!(stats.ocean_dispatches, 3 + 3 * 2 * 7, "{stats:?}");
    for size in [128u32, 256] {
        let off = water
            .measure_ocean_cost(&plain, size, 2)
            .expect("measure")
            .expect("ran");
        let on = water
            .measure_ocean_cost(&lingering(), size, 2)
            .expect("measure")
            .expect("ran");
        let texels = u64::from(size) * u64::from(size);
        assert_eq!(off.history_bytes, 0);
        assert_eq!(off.history_dispatches, 0);
        assert_eq!(on.history_bytes, 2 * 3 * texels * 16, "{on:?}");
        assert_eq!(on.history_dispatches, 3);
        assert_eq!(on.dispatches, off.dispatches + 3);
        assert_eq!(on.gpu_bytes, off.gpu_bytes + on.history_bytes);
        eprintln!(
            "grid {size}: history {} bytes, submit {:.3} ms on vs {:.3} ms off (lavapipe, not compared)",
            on.history_bytes, on.submit_ms, off.submit_ms
        );
    }
}

fn layer_camera(camera: Camera) -> LayerCamera {
    LayerCamera {
        eye: camera.eye,
        target: camera.target,
        fov_y: camera.fov_y,
        near: 0.1,
        far: 2000.0,
        width: W,
        height: H,
        exposure: 1.0,
        display_exposure: 1.0,
        hdr: true,
        sun_direction: [-SUN[0], -SUN[1], -SUN[2]],
        sun_color: [1.0, 1.0, 1.0],
        sun_illuminance: 80_000.0,
        sky_ambient: None,
    }
}

/// Renders `ticks` of `body` with one renderer and the masks view; returns the last frame's foam
/// (the masks view's red channel, one byte per pixel) and the stats.
fn foam_pixels(
    device: &RenderDevice,
    scene: &StreamScene,
    body: &WaterBody,
    camera: Camera,
    ticks: std::ops::Range<u64>,
) -> (Vec<u8>, WaterFrameStats) {
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    let water = WaterRenderer::new(device);
    water.set_tier(WaterTier::High);
    water.set_bodies(&[body]);
    water.set_ambient_lux(18_000.0);
    water.set_debug(WaterDebug::Masks);
    renderer.set_translucent_layers(vec![water.layer()]);
    let post = post();
    let mut last = Vec::new();
    for tick in ticks {
        water.set_clock(clock(tick));
        last = renderer
            .render_offscreen(device, &frame(scene, camera, W, H, &post, SUN))
            .expect("frame")
            .pixels;
    }
    let foam = last.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
    (foam, water.stats())
}

/// The pixels whose foam is above a tenth.
fn foamy(foam: &[u8]) -> usize {
    foam.iter().filter(|&&r| r > 25).count()
}

/// The pixels where `with` shows less foam than `without`, beyond two levels of rounding.
fn foam_lost(with: &[u8], without: &[u8]) -> usize {
    assert_eq!(with.len(), without.len());
    with.iter()
        .zip(without)
        .filter(|&(&w, &o)| u16::from(w) + 2 < u16::from(o))
        .count()
}

/// Looking over the sea from 3 m up.
const VIEW: Camera = Camera {
    eye: [-6.0, 3.0, 4.0],
    target: [8.0, LEVEL, -1.0],
    fov_y: 0.9,
};

/// Low over a wide sea toward the horizon: most pixels are hundreds of meters away.
const HORIZON: Camera = Camera {
    eye: [0.0, 12.0, 0.0],
    target: [600.0, LEVEL, 40.0],
    fov_y: 0.5,
};

/// Lingering foam adds foam to the drawn frame and takes none away at any pixel; the whitecap
/// rescale adds foam in the distance, again taking none away (the drawn foam is the larger of
/// the instant foam and the history, and the rescale only ever scales the compression up).
#[test]
fn lingering_foam_and_the_rescale_show_in_the_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let plain = FftOcean {
        wind_speed: 16.0,
        ..FftOcean::default()
    };
    let (off_foam, off_stats) = foam_pixels(&device, &scene, &sea(plain, 400.0), VIEW, 600..630);
    let (on_foam, on_stats) = foam_pixels(&device, &scene, &sea(lingering(), 400.0), VIEW, 600..630);
    let (off, on) = (foamy(&off_foam), foamy(&on_foam));
    let lost = foam_lost(&on_foam, &off_foam);
    eprintln!("near view foam pixels: {off} instant, {on} lingering, {lost} lost");
    assert_eq!(off_stats.ocean_history_steps, 0, "{off_stats:?}");
    assert_eq!(on_stats.ocean_history_steps, 30, "{on_stats:?}");
    assert!(on > off, "lingering foam shows: {on} vs {off}");
    assert_eq!(lost, 0, "lingering foam takes no foam away");
    let rescaled = FftOcean {
        whitecap_rescale: true,
        ..plain
    };
    let (far_foam, _) = foam_pixels(&device, &scene, &sea(plain, 4000.0), HORIZON, 600..601);
    let (kept_foam, _) = foam_pixels(&device, &scene, &sea(rescaled, 4000.0), HORIZON, 600..601);
    let (far, kept) = (foamy(&far_foam), foamy(&kept_foam));
    let lost = foam_lost(&kept_foam, &far_foam);
    eprintln!("horizon view foam pixels: {far} plain, {kept} rescaled, {lost} lost");
    assert_eq!(lost, 0, "the rescale takes no foam away");
    assert!(
        kept > far,
        "the rescale keeps whitecaps in the distance: {kept} vs {far}"
    );
}

/// The water height a floater asks for at `(x, z)` through the gameplay query.
fn floater_height(world: &mut World, x: f32, z: f32) -> Option<f32> {
    let mut state: SystemState<WaterQuery> = SystemState::new(world);
    let query = state.get(world).ok()?;
    query.surface(x, z).map(|(_, s)| s.height)
}

/// On Low a floater rides the drawn water when queries follow the tier: at every vertex near the
/// camera that Low draws on the GPU, the query's height is the vertex's within 5 mm. With the
/// default (queries answer the ocean on every tier) it is off by up to the ocean's waves.
#[test]
fn a_floater_on_low_rides_the_drawn_surface() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    let ocean = FftOcean {
        wind_speed: 14.0,
        ..FftOcean::default()
    };
    let body = sea(ocean, 400.0);
    let c = clock(7_777);
    water.set_tier(WaterTier::Low);
    water.set_clock(c);
    water.set_bodies(&[&body]);
    let camera = layer_camera(VIEW);
    let first = water
        .probe_ring_vertices(&camera, &[(0, [0.0, 0.0])])
        .expect("probe");
    let cell = first.grid.cell();
    let mut points: Vec<(u32, [f32; 2])> = Vec::new();
    for j in -6i32..=6 {
        for i in -6i32..=6 {
            points.push((0, [i as f32 * cell * 1.5, j as f32 * cell * 2.5]));
        }
    }
    let probe = water.probe_ring_vertices(&camera, &points).expect("probe");
    assert_eq!(probe.positions.len(), points.len());
    let mut world = World::new();
    world.insert_resource(c);
    world.insert_resource(WaterSettings {
        tier: Some(WaterTier::Low),
        queries_follow_tier: true,
        ..WaterSettings::default()
    });
    world.spawn(body.clone());
    let drawn = body.drawn_for(WaterTier::Low, true);
    let finest = drawn
        .swell
        .as_ref()
        .expect("the stand-in")
        .waves
        .iter()
        .map(|w| std::f32::consts::TAU / w.wavelength)
        .fold(0.0f32, f32::max);
    let (mut worst, mut checked) = (0.0f32, 0);
    for ((_, local), gpu) in points.iter().zip(&probe.positions) {
        // Where the grid fades no wave, the vertex lies on the drawn surface.
        if probe.grid.fade_cell(*local) * finest > std::f32::consts::FRAC_PI_2 {
            continue;
        }
        let height = floater_height(&mut world, gpu[0], gpu[2]).expect("inside the sea");
        worst = worst.max((height - gpu[1]).abs());
        checked += 1;
    }
    eprintln!("Low, queries follow the tier: worst {worst} m over {checked} vertices");
    assert!(checked > 100, "{checked}");
    assert!(worst < 5.0e-3, "floater vs drawn vertex on Low: {worst} m");
    // The default: the floater rides the ocean, which Low does not draw.
    world.insert_resource(WaterSettings {
        tier: Some(WaterTier::Low),
        ..WaterSettings::default()
    });
    let mut off = 0.0f32;
    for gpu in &probe.positions {
        if let Some(height) = floater_height(&mut world, gpu[0], gpu[2]) {
            off = off.max((height - gpu[1]).abs());
        }
        let ocean_height = body.surface(gpu[0], gpu[2], &c).map(|s| s.height);
        assert_eq!(
            floater_height(&mut world, gpu[0], gpu[2]),
            ocean_height,
            "the default answers the ocean"
        );
    }
    eprintln!("Low, default queries: worst {off} m");
    assert!(off > 0.1, "the default is off by the ocean's waves: {off} m");
}
