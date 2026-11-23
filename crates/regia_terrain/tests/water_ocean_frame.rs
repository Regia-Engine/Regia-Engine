// SPDX-License-Identifier: MPL-2.0
//! The FFT ocean on a real GPU (ADR-1969; lavapipe in CI): the compute pass against its CPU mirror,
//! the vertex shader's placement of ring-grid vertices against the CPU surface, the shore blend,
//! and the ocean through the real `FrameRenderer` (hash-stable per tick and seed, moving between
//! ticks, gated by the tier), at every grid an ocean can ask for (ADR-2126).
//!
//! No golden image: every check compares the GPU with the CPU, or frames of the same run.
//! `REGIA_WATER_FRAME_DUMP=<dir>` writes the frames as PNGs (evidence; not compared).

mod water_scene;

use std::sync::Arc;

use regia_app::prelude::{App, Update};
use regia_render::{FrameRenderer, LayerCamera, RenderDevice, RuntimeRenderer};
use regia_terrain::water_body::gpu::ring_vertex_ocean;
use regia_terrain::water_body::ocean::{
    OCEAN_BANDS, OCEAN_GRIDS, OCEAN_MAX_SIZE, OCEAN_QUERY_SIZE, OceanFrame, frame_of, loop_fraction,
    tables_for,
};
use regia_terrain::water_body::plugin::sync_water;
use regia_terrain::water_body::{
    OCEAN_PASS_NAME, OceanMaps, OceanSpectrum, ShoreBakeDesc, ShoreField, WaterFrameStats, WaveMode,
};
use regia_terrain::{
    FftOcean, Swell, WaterBody, WaterClock, WaterLook, WaterRenderer, WaterSettings, WaterState, WaterTier,
};
use water_scene::{Camera, STUDIO_SKY, StreamScene, frame, post, stream_scene};

const W: u32 = 256;
const H: u32 = 144;
const SUN: [f32; 3] = [0.35, -0.65, -0.55];
const LEVEL: f32 = -0.1;

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn ocean() -> FftOcean {
    FftOcean {
        wind_speed: 14.0,
        ..FftOcean::default()
    }
}

/// A sea wider than the scene, with the ocean and no swell of its own.
fn sea() -> WaterBody {
    let mut sea = WaterBody::plane([0.0, LEVEL, 0.0], [400.0, 400.0], WaterLook::ocean());
    sea.ocean = Some(ocean());
    sea
}

/// Looking over the sea from 3 m up.
const VIEW: Camera = Camera {
    eye: [-6.0, 3.0, 4.0],
    target: [8.0, LEVEL, -1.0],
    fov_y: 0.9,
};

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

fn clock(tick: u64) -> WaterClock {
    WaterClock {
        tick,
        ..WaterClock::default()
    }
}

/// The compute pass and the CPU mirror agree on every field of every band, at the High and the
/// Ultra map size, at two times.
#[test]
fn gpu_maps_match_the_cpu_mirror() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    let sea = sea();
    let o = ocean();
    water.set_bodies(&[&sea]);
    for (tier, size) in [
        (WaterTier::High, OCEAN_QUERY_SIZE),
        (WaterTier::Ultra, OCEAN_MAX_SIZE),
    ] {
        water.set_tier(tier);
        for tick in [240u64, 86_400] {
            let c = clock(tick);
            water.set_clock(c);
            let maps = water
                .read_ocean_maps()
                .expect("readback")
                .expect("an ocean is drawn");
            assert_eq!(maps.size, size);
            assert_eq!(maps.texels.len() as u32, 2 * OCEAN_BANDS as u32 * size * size);
            let cpu = frame_of(&o, c.seconds(), size);
            let mut worst = [0.0f32; 8];
            let mut rise = 0.0f32;
            for (b, grid) in cpu.grids.iter().enumerate() {
                for row in 0..size {
                    for column in 0..size {
                        let i = (row * size + column) as usize;
                        let t0 = maps.texel(b, 0, column, row);
                        let t1 = maps.texel(b, 1, column, row);
                        let want = [
                            grid.dx[i],
                            grid.h[i],
                            grid.dz[i],
                            grid.dxz[i],
                            grid.hx[i],
                            grid.hz[i],
                            grid.dxx[i],
                            grid.dzz[i],
                        ];
                        let got = [t0[0], t0[1], t0[2], t0[3], t1[0], t1[1], t1[2], t1[3]];
                        for ((w, g), e) in want.iter().zip(got).zip(&mut worst) {
                            *e = e.max((w - g).abs());
                        }
                        rise = rise.max(t0[1].abs());
                    }
                }
            }
            eprintln!("{tier:?} tick {tick}: worst field error {worst:?}, peak height {rise}");
            assert!(worst.iter().all(|e| *e < 1.0e-3), "{tier:?} {tick}: {worst:?}");
            assert!(rise > 0.2, "the test is not comparing flat maps: {rise}");
        }
    }
}

/// The largest difference per field between GPU maps and the CPU reference at the same instant, and
/// the highest height. The reference is built apart from the frame memo: a 1024 frame is 144 MB.
fn compare_with_reference(maps: &OceanMaps, seconds: f64) -> ([f32; 8], f32) {
    let size = maps.size;
    let cpu = OceanFrame::at(&tables_for(&maps.ocean, size), loop_fraction(seconds));
    let mut worst = [0.0f32; 8];
    let mut rise = 0.0f32;
    for (b, grid) in cpu.grids.iter().enumerate() {
        for row in 0..size {
            for column in 0..size {
                let i = (row * size + column) as usize;
                let t0 = maps.texel(b, 0, column, row);
                let t1 = maps.texel(b, 1, column, row);
                let want = [
                    grid.dx[i],
                    grid.h[i],
                    grid.dz[i],
                    grid.dxz[i],
                    grid.hx[i],
                    grid.hz[i],
                    grid.dxx[i],
                    grid.dzz[i],
                ];
                let got = [t0[0], t0[1], t0[2], t0[3], t1[0], t1[1], t1[2], t1[3]];
                for ((w, g), e) in want.iter().zip(got).zip(&mut worst) {
                    *e = e.max((w - g).abs());
                }
                rise = rise.max(t0[1].abs());
            }
        }
    }
    (worst, rise)
}

/// A sea asking for `grid`.
fn sea_on_grid(ocean: FftOcean, grid: u32) -> WaterBody {
    let mut sea = sea();
    sea.ocean = Some(FftOcean { grid, ..ocean });
    sea
}

/// The compute pass matches the CPU reference FFT at every grid, for both spectra, and each grid
/// runs the dispatches its size asks for: one spectrum and `2 log2 N` FFT stages per band.
#[test]
fn gpu_maps_match_the_cpu_reference_at_every_grid() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    water.set_tier(WaterTier::High);
    for (grid, spectrum) in [
        (128, OceanSpectrum::Phillips),
        (256, OceanSpectrum::Jonswap),
        (256, OceanSpectrum::Phillips),
        (512, OceanSpectrum::Jonswap),
        (1024, OceanSpectrum::Jonswap),
        (1024, OceanSpectrum::Phillips),
    ] {
        let sea = sea_on_grid(FftOcean { spectrum, ..ocean() }, grid);
        water.set_bodies(&[&sea]);
        let c = clock(1_801);
        water.set_clock(c);
        let maps = water
            .read_ocean_maps()
            .expect("readback")
            .expect("an ocean is drawn");
        assert_eq!(maps.size, grid);
        assert_eq!(maps.texels.len() as u32, 2 * OCEAN_BANDS as u32 * grid * grid);
        let (worst, rise) = compare_with_reference(&maps, c.seconds());
        eprintln!("{grid} {spectrum:?}: worst field error {worst:?}, peak height {rise}");
        assert!(
            worst.iter().all(|e| *e < 1.0e-3),
            "{grid} {spectrum:?}: {worst:?}"
        );
        assert!(rise > 0.2, "the test is not comparing flat maps: {rise}");
        let stats = water.stats();
        assert_eq!(stats.ocean_size, grid, "{stats:?}");
        assert_eq!(
            stats.ocean_dispatches,
            3 + 3 * 2 * grid.trailing_zeros(),
            "{stats:?}"
        );
    }
}

/// The maps' normal and foam are what the displacement says: on a real GPU, at a large grid, the
/// slopes and the Jacobian match central differences of the displacement, each normal is
/// perpendicular to its tangents, and foam follows the Jacobian at the ocean's onset.
#[test]
fn normal_and_foam_maps_follow_the_displacement() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    water.set_tier(WaterTier::High);
    let spec = FftOcean {
        wind_speed: 24.0,
        foam_onset: 0.02,
        crest_foam: 2.0,
        ..ocean()
    };
    let size = 512u32;
    water.set_bodies(&[&sea_on_grid(spec, size)]);
    water.set_clock(clock(3_333));
    let maps = water.read_ocean_maps().expect("readback").expect("drawn");
    let mask = size - 1;
    // Bands 0 and 1 stop at 16 texels per wavelength on this grid, so a central difference holds.
    for band in 0..2usize {
        let cell = spec.tiles[band] / size as f32;
        let (mut slope_peak, mut slope_error) = (0.0f32, 0.0f32);
        let (mut stretch_peak, mut stretch_error) = (0.0f32, 0.0f32);
        let mut foamed = 0u32;
        let mut clear = 0u32;
        for row in (0..size).step_by(7) {
            for column in (0..size).step_by(5) {
                let east = maps.displacement(band, (column + 1) & mask, row);
                let west = maps.displacement(band, column.wrapping_sub(1) & mask, row);
                let south = maps.displacement(band, column, (row + 1) & mask);
                let north = maps.displacement(band, column, row.wrapping_sub(1) & mask);
                let s = maps.band_sample(band, column, row);
                let hx = (east[1] - west[1]) / (2.0 * cell);
                let hz = (south[1] - north[1]) / (2.0 * cell);
                slope_error = slope_error
                    .max((hx - s.dpdx[1]).abs())
                    .max((hz - s.dpdz[1]).abs());
                slope_peak = slope_peak.max(s.dpdx[1].abs()).max(s.dpdz[1].abs());
                let dxx = (east[0] - west[0]) / (2.0 * cell);
                let dzz = (south[2] - north[2]) / (2.0 * cell);
                let dxz = (east[2] - west[2]) / (2.0 * cell);
                stretch_error = stretch_error
                    .max((1.0 + dxx - s.dpdx[0]).abs())
                    .max((1.0 + dzz - s.dpdz[2]).abs())
                    .max((dxz - s.dpdx[2]).abs());
                stretch_peak = stretch_peak.max(dxx.abs()).max(dzz.abs());
                let n = s.normal();
                let dot = |t: [f32; 3]| n[0] * t[0] + n[1] * t[1] + n[2] * t[2];
                assert!(dot(s.dpdx).abs() < 1.0e-3 && dot(s.dpdz).abs() < 1.0e-3, "{s:?}");
                assert!(n[1] > 0.0 || s.jacobian <= 0.0, "{n:?}");
                let foam = maps.band_foam(band, column, row);
                let want = regia_terrain::water_body::swell::crest_foam(
                    s.jacobian,
                    spec.crest_foam,
                    spec.foam_onset,
                );
                assert_eq!(foam, want);
                if foam > 0.0 {
                    foamed += 1;
                } else {
                    clear += 1;
                }
            }
        }
        eprintln!(
            "band {band}: slope error {slope_error} of {slope_peak}, stretch error {stretch_error} of {stretch_peak}, foam {foamed} of {}",
            foamed + clear
        );
        assert!(slope_peak > 0.01, "flat slopes: {slope_peak}");
        assert!(slope_error < 0.05 * slope_peak, "{slope_error} of {slope_peak}");
        assert!(stretch_peak > 0.01, "flat stretch: {stretch_peak}");
        assert!(
            stretch_error < 0.05 * stretch_peak,
            "{stretch_error} of {stretch_peak}"
        );
        if band == 1 {
            assert!(foamed > 0 && clear > 0, "foam {foamed}, clear {clear}");
        }
    }
}

/// A grid is a drawing choice on the tiers that draw: it picks the maps, switching it rebuilds them,
/// Low stays without compute, and asking for the tier's own size gives the same maps.
#[test]
fn the_grid_picks_the_maps_on_the_tiers_that_draw() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    water.set_tier(WaterTier::High);
    water.set_clock(clock(120));
    let read = |grid: u32| {
        water.set_bodies(&[&sea_on_grid(ocean(), grid)]);
        water.read_ocean_maps().expect("readback")
    };
    let by_tier = read(0).expect("drawn");
    assert_eq!(by_tier.size, OCEAN_QUERY_SIZE);
    let explicit = read(OCEAN_QUERY_SIZE).expect("drawn");
    assert_eq!(
        by_tier.texels, explicit.texels,
        "the same size gives the same maps"
    );
    for grid in [512, 256] {
        assert_eq!(read(grid).expect("drawn").size, grid);
    }
    water.set_tier(WaterTier::Ultra);
    assert_eq!(read(0).expect("drawn").size, OCEAN_MAX_SIZE);
    assert_eq!(read(512).expect("drawn").size, 512);
    water.set_tier(WaterTier::Low);
    assert_eq!(read(512), None, "Low draws no maps, whatever the ocean asks");
    assert_eq!(water.stats().ocean_size, 0);
    assert_eq!(water.stats().ocean_dropped, 1);
    // A grid that skipped validation never reaches the transform: like any invalid body, the frame
    // does not draw it (`build_mesh`), and nothing is dispatched.
    water.set_tier(WaterTier::High);
    assert_eq!(read(100), None, "an invalid grid draws nothing");
    assert_eq!(water.stats().ocean_size, 0);
    assert_eq!(water.stats().ocean_bodies, 0);
    // Back on a good grid, it draws again.
    assert_eq!(read(256).expect("drawn").size, 256);
}

/// The cost measurement runs on its own simulation: the frame's maps and counters stay as they
/// were. Its times are reported, never compared.
#[test]
fn measuring_the_cost_leaves_the_frame_alone() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    let sea = sea();
    water.set_tier(WaterTier::High);
    water.set_clock(clock(240));
    water.set_bodies(&[&sea]);
    let before = water.read_ocean_maps().expect("readback").expect("drawn");
    let stats = water.stats();
    let stamped = device
        .device
        .features()
        .contains(wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
    for grid in OCEAN_GRIDS {
        let cost = water
            .measure_ocean_cost(&ocean(), grid, 2)
            .expect("measure")
            .expect("this device runs every grid");
        eprintln!("{cost:?}");
        assert_eq!(cost.size, grid);
        assert_eq!(cost.frames, 2);
        assert_eq!(cost.dispatches, 3 + 3 * 2 * grid.trailing_zeros());
        assert!(cost.gpu_bytes >= 4 * 6 * u64::from(grid * grid) * 16, "{cost:?}");
        assert!(cost.modes > 1000, "{cost:?}");
        assert_eq!(cost.compute_ms.is_some(), stamped, "{cost:?}");
        assert_eq!(cost.copy_ms.is_some(), stamped, "{cost:?}");
    }
    assert_eq!(water.measure_ocean_cost(&ocean(), 100, 2).expect("measure"), None);
    assert_eq!(water.stats(), stats, "the frame's counters did not move");
    let after = water.read_ocean_maps().expect("readback").expect("drawn");
    assert_eq!(before, after, "the frame's maps did not move");
}

/// The default look does not change: a plane without an ocean draws no compute, and a plane switched
/// to the ocean and back draws the bytes it drew before, whether or not it had a swell of its own.
#[test]
fn switching_back_to_the_swell_restores_the_frame_bytes() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let water = WaterRenderer::new(&device);
    water.set_tier(WaterTier::High);
    water.set_clock(clock(600));
    water.set_ambient_lux(18_000.0);
    let draw = |body: &WaterBody| {
        let mut renderer = FrameRenderer::new();
        renderer.set_studio_sky(&device, Some(STUDIO_SKY));
        water.set_bodies(&[body]);
        renderer.set_translucent_layers(vec![water.layer()]);
        renderer.register_pass(water.ocean_pass()).expect("pass name");
        let post = post();
        renderer
            .render_offscreen(&device, &frame(&scene, VIEW, W, H, &post, SUN))
            .expect("frame")
            .pixels
    };
    let mut plain = WaterBody::plane([0.0, LEVEL, 0.0], [400.0, 400.0], WaterLook::ocean());
    plain.swell = Some(Swell::open_sea([1.0, 0.35], 0.8, 30.0, 12));
    let original = plain.clone();
    let swell_frame = draw(&plain);
    assert_eq!(water.stats().ocean_bodies, 0);
    assert_eq!(water.stats().ocean_size, 0);
    assert_eq!(water.stats().ocean_evolves, 0, "no ocean, no compute");

    plain.set_wave_mode(WaveMode::Fft).expect("plane");
    let ocean_frame = draw(&plain);
    assert_eq!(water.stats().ocean_bodies, 1);
    assert_eq!(water.stats().ocean_evolves, 1);
    assert!(
        changed_fraction(&swell_frame, &ocean_frame) > 0.02,
        "fft draws another sea"
    );

    plain.set_wave_mode(WaveMode::Swell).expect("plane");
    let back = draw(&plain);
    assert_eq!(fnv1a(&swell_frame), fnv1a(&back), "back on the swell, same bytes");
    assert_eq!(water.stats().ocean_size, 0);
    // Parking aside, the body is the one it was.
    plain.parked_ocean = None;
    assert_eq!(plain, original);

    // An ocean with no swell of its own draws its stand-in in swell mode: what Low draws.
    let mut bare = sea();
    let mut stand_in = bare.clone();
    stand_in.swell = Some(ocean().stand_in_swell());
    stand_in.ocean = None;
    let stand_in_frame = draw(&stand_in);
    bare.set_wave_mode(WaveMode::Swell).expect("plane");
    assert_eq!(
        fnv1a(&stand_in_frame),
        fnv1a(&draw(&bare)),
        "parked, it draws the stand-in"
    );
    bare.set_wave_mode(WaveMode::Fft).expect("plane");
    assert_eq!(bare, sea());
}

/// A frame at each large grid is hash-stable and draws from those maps.
#[test]
fn ocean_frames_are_stable_at_the_large_grids() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let (base, _, _) = render(&device, &scene, &sea(), WaterTier::High, 600, true, "grid_tier");
    for grid in [256, 512, 1024] {
        let sea = sea_on_grid(ocean(), grid);
        let label = format!("grid_{grid}");
        let (a, stats, runs) = render(&device, &scene, &sea, WaterTier::High, 600, true, &label);
        let (b, _, _) = render(&device, &scene, &sea, WaterTier::High, 600, true, &label);
        assert_eq!(fnv1a(&a), fnv1a(&b), "{grid}: two renders of a tick differ");
        assert_eq!(runs, Some(1), "{stats:?}");
        assert_eq!(stats.ocean_size, grid, "{stats:?}");
        assert_eq!(stats.ocean_bodies, 1, "{stats:?}");
        assert_eq!(stats.ocean_evolves, 1, "{stats:?}");
        // Same long waves, finer detail: a frame that moved is expected, a blank one is not.
        let moved = changed_fraction(&base, &a);
        eprintln!("grid {grid}: {moved} of the pixels differ from the tier's grid");
        assert!(a.iter().any(|v| *v != a[0]), "{grid}: a flat frame");
    }
}

/// Two readbacks of one tick are equal, the seed changes the sea, and a tick that repeats does no
/// new dispatches.
#[test]
fn gpu_maps_are_deterministic_by_seed_and_time() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let read = |sea: &WaterBody, tick: u64| {
        let water = WaterRenderer::new(&device);
        water.set_tier(WaterTier::High);
        water.set_clock(clock(tick));
        water.set_bodies(&[sea]);
        let maps = water.read_ocean_maps().expect("readback").expect("drawn");
        let again = water.read_ocean_maps().expect("readback").expect("drawn");
        assert_eq!(maps, again);
        assert_eq!(water.stats().ocean_evolves, 1, "{:?}", water.stats());
        assert_eq!(water.stats().ocean_size, OCEAN_QUERY_SIZE);
        maps
    };
    let sea = sea();
    let a = read(&sea, 1000);
    assert_eq!(a, read(&sea, 1000), "same seed, same time");
    assert_ne!(a, read(&sea, 1001), "the sea moves");
    let mut other = sea.clone();
    other.ocean = Some(FftOcean { seed: 9, ..ocean() });
    assert_ne!(a, read(&other, 1000), "the seed picks the sea");
}

/// The GPU places ring-grid vertices where the CPU mirror does (within 1 mm) and, near the camera,
/// where every band is unfaded, on the surface `WaterBody::surface` answers.
#[test]
fn ring_vertices_match_the_cpu_surface_on_the_gpu() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    let sea = sea();
    let c = clock(7_777);
    water.set_tier(WaterTier::High);
    water.set_clock(c);
    water.set_bodies(&[&sea]);
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
    // Farther out, where the fade differs from the query but the mirror must still agree.
    for k in 1..=12 {
        points.push((0, [k as f32 * 17.3, -(k as f32) * 13.1]));
    }
    let probe = water.probe_ring_vertices(&camera, &points).expect("probe");
    assert_eq!(probe.positions.len(), points.len());
    let maps = frame_of(&ocean(), c.seconds(), OCEAN_QUERY_SIZE);
    let ring = (&probe.grid, probe.origin);
    let finest = maps
        .tables
        .bands
        .iter()
        .map(|b| b.rms_k as f32)
        .fold(0.0f32, f32::max);
    let mut worst_mirror = 0.0f32;
    let mut worst_query = 0.0f32;
    let mut checked = 0;
    for ((_, local), gpu) in points.iter().zip(&probe.positions) {
        let cpu = ring_vertex_ocean(&sea, None, &maps, ring, c.seconds(), 16, *local).expect("ocean");
        for (a, b) in cpu.iter().zip(gpu) {
            worst_mirror = worst_mirror.max((a - b).abs());
        }
        // Where every band is unfaded the vertex lies on the queried surface.
        let spacing = probe.grid.fade_cell(*local);
        if spacing * finest <= std::f32::consts::FRAC_PI_2 {
            let q = sea.surface(gpu[0], gpu[2], &c).expect("inside the sea");
            worst_query = worst_query.max((q.height - gpu[1]).abs());
            checked += 1;
        }
    }
    eprintln!("mirror {worst_mirror} m, query {worst_query} m over {checked} points");
    assert!(worst_mirror < 1.0e-3, "GPU vs CPU vertex: {worst_mirror} m");
    assert!(checked > 20, "{checked}");
    assert!(worst_query < 5.0e-3, "GPU vertex vs CPU surface: {worst_query} m");
    let spread = probe
        .positions
        .iter()
        .map(|p| (p[1] - LEVEL).abs())
        .fold(0.0f32, f32::max);
    assert!(spread > 0.1, "{spread}");
}

/// A beach falling away along −x, with its shoreline at x = 200: 60 m deep at the far edge.
fn beach() -> Arc<ShoreField> {
    let field = ShoreField::bake(
        &ShoreBakeDesc {
            level: LEVEL,
            min: [-400.0, -400.0],
            max: [400.0, 400.0],
            cell: 2.0,
        },
        |x, _| LEVEL + (x - 200.0) / 10.0,
    )
    .expect("bake");
    Arc::new(field)
}

/// Over a beach the GPU vertices follow the blend of the CPU mirror, through the whole span from
/// the ocean offshore to the shoaled swell in the shallows.
#[test]
fn shore_blend_matches_the_cpu_on_the_gpu() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    let mut sea = sea();
    sea.swell = Some(Swell::open_sea([1.0, 0.35], 0.8, 30.0, 12));
    let field = beach();
    let c = clock(4_000);
    water.set_tier(WaterTier::High);
    water.set_clock(c);
    water.set_bodies(&[&sea]);
    water.set_shores(&[Some(Arc::clone(&field))]);
    let mut camera = layer_camera(VIEW);
    camera.eye = [0.0, 40.0, 0.0];
    camera.target = [120.0, LEVEL, 0.0];
    let points: Vec<(u32, [f32; 2])> = (0..96).map(|i| (0, [-100.0 + i as f32 * 3.1, 7.0])).collect();
    let probe = water.probe_ring_vertices(&camera, &points).expect("probe");
    let maps = frame_of(&ocean(), c.seconds(), OCEAN_QUERY_SIZE);
    let mut worst = 0.0f32;
    let o = ocean();
    let mut weights = Vec::new();
    for ((_, local), gpu) in points.iter().zip(&probe.positions) {
        let ring = (&probe.grid, probe.origin);
        let cpu = ring_vertex_ocean(&sea, Some(&field), &maps, ring, c.seconds(), 16, *local).expect("ocean");
        for (a, b) in cpu.iter().zip(gpu) {
            worst = worst.max((a - b).abs());
        }
        let rest_x = probe.origin[0] as f32 + local[0];
        let depth = field
            .sample(rest_x, probe.origin[1] as f32 + local[1])
            .map(|b| b.depth);
        weights.push(o.shore_weight(depth));
    }
    eprintln!("blend mirror {worst} m");
    assert!(worst < 1.0e-3, "GPU vs CPU vertex over the beach: {worst} m");
    assert!(weights.contains(&1.0), "{weights:?}");
    assert!(weights.iter().any(|w| *w > 0.2 && *w < 0.8), "{weights:?}");
    assert!(weights.contains(&0.0), "{weights:?}");
}

fn render(
    device: &RenderDevice,
    scene: &StreamScene,
    body: &WaterBody,
    tier: WaterTier,
    tick: u64,
    register: bool,
    label: &str,
) -> (Vec<u8>, WaterFrameStats, Option<u64>) {
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    let water = WaterRenderer::new(device);
    water.set_tier(tier);
    water.set_clock(clock(tick));
    water.set_bodies(&[body]);
    water.set_ambient_lux(18_000.0);
    renderer.set_translucent_layers(vec![water.layer()]);
    if register {
        renderer.register_pass(water.ocean_pass()).expect("pass name");
    }
    let post = post();
    let pixels = renderer
        .render_offscreen(device, &frame(scene, VIEW, W, H, &post, SUN))
        .expect("frame")
        .pixels;
    dump(label, &pixels);
    let runs = renderer
        .graph_report()
        .extensions
        .iter()
        .find(|p| p.name == OCEAN_PASS_NAME)
        .map(|p| p.runs);
    (pixels, water.stats(), runs)
}

fn dump(label: &str, pixels: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_WATER_FRAME_DUMP") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = std::path::Path::new(&dir).join(format!("ocean_{label}.png"));
    let Ok(file) = std::fs::File::create(&path) else {
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn changed_fraction(a: &[u8], b: &[u8]) -> f32 {
    let changed = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > 2))
        .count();
    changed as f32 / (W * H) as f32
}

/// Two renders of one tick are byte-identical, the extension pass and the layer's own evolve
/// give the same bytes, the sea moves in a second, the seed changes it, and the frame differs from
/// the swell of the same sea.
#[test]
fn ocean_frame_is_hash_stable_and_moves() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let sea = sea();
    let (a, stats, runs) = render(&device, &scene, &sea, WaterTier::High, 600, true, "high_t600");
    let (b, _, _) = render(
        &device,
        &scene,
        &sea,
        WaterTier::High,
        600,
        true,
        "high_t600_again",
    );
    assert_eq!(fnv1a(&a), fnv1a(&b), "two renders of tick 600 differ");
    assert_eq!(runs, Some(1), "the frame ran the extension pass once: {stats:?}");
    assert!(stats.frames_drawn >= 1, "{stats:?}");
    assert_eq!(stats.ocean_bodies, 1, "{stats:?}");
    assert_eq!(stats.ocean_size, OCEAN_QUERY_SIZE, "{stats:?}");
    assert_eq!(
        stats.ocean_evolves, 1,
        "the layer did not evolve a second time: {stats:?}"
    );
    assert_eq!(stats.ocean_dispatches, 3 + 3 * 2 * 7, "{stats:?}");
    assert!(stats.rings >= 1 && stats.ring_triangles > 0, "{stats:?}");
    // A host that does not register the pass gets the same frame from `simulate`.
    let (bare, bare_stats, bare_runs) =
        render(&device, &scene, &sea, WaterTier::High, 600, false, "high_bare");
    assert_eq!(fnv1a(&a), fnv1a(&bare), "the two recording paths differ");
    assert_eq!(bare_runs, None);
    assert_eq!(bare_stats.ocean_evolves, 1, "{bare_stats:?}");
    let (later, _, _) = render(&device, &scene, &sea, WaterTier::High, 660, true, "high_t660");
    let moved = changed_fraction(&a, &later);
    assert!(moved > 0.02, "the ocean did not move in a second ({moved})");
    let mut seeded = sea.clone();
    seeded.ocean = Some(FftOcean { seed: 5, ..ocean() });
    let (other, _, _) = render(&device, &scene, &seeded, WaterTier::High, 600, true, "high_seed5");
    assert!(changed_fraction(&a, &other) > 0.02, "the seed changed nothing");
    let (ultra, ultra_stats, _) = render(&device, &scene, &sea, WaterTier::Ultra, 600, true, "ultra_t600");
    assert_eq!(ultra_stats.ocean_size, OCEAN_MAX_SIZE, "{ultra_stats:?}");
    assert_eq!(ultra_stats.ocean_dispatches, 3 + 3 * 2 * 8, "{ultra_stats:?}");
    assert!(ultra.len() == a.len());
}

/// Low draws the stand-in swell alone: byte-identical to the same body with that swell and no
/// ocean, no compute. `set_ocean_draw(false)` does the same on High.
#[test]
fn low_keeps_the_swell() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let sea = sea();
    let mut swell_only = sea.clone();
    swell_only.swell = Some(ocean().stand_in_swell());
    swell_only.ocean = None;
    let (low, stats, _) = render(&device, &scene, &sea, WaterTier::Low, 600, true, "low_t600");
    let (plain, _, _) = render(
        &device,
        &scene,
        &swell_only,
        WaterTier::Low,
        600,
        false,
        "low_swell",
    );
    assert_eq!(fnv1a(&low), fnv1a(&plain), "Low draws the stand-in swell");
    assert_eq!(stats.ocean_size, 0, "{stats:?}");
    assert_eq!(stats.ocean_bodies, 0, "{stats:?}");
    assert_eq!(stats.ocean_dropped, 1, "{stats:?}");
    assert_eq!(stats.ocean_evolves, 0, "{stats:?}");
    assert_eq!(stats.swell_bodies, 1, "{stats:?}");

    let draw = |body: &WaterBody, on: bool| {
        let mut renderer = FrameRenderer::new();
        renderer.set_studio_sky(&device, Some(STUDIO_SKY));
        let water = WaterRenderer::new(&device);
        water.set_tier(WaterTier::High);
        water.set_clock(clock(600));
        water.set_bodies(&[body]);
        water.set_ocean_draw(on);
        water.set_ambient_lux(18_000.0);
        renderer.set_translucent_layers(vec![water.layer()]);
        let post = post();
        let pixels = renderer
            .render_offscreen(&device, &frame(&scene, VIEW, W, H, &post, SUN))
            .expect("frame")
            .pixels;
        (pixels, water.stats())
    };
    let (off, off_stats) = draw(&sea, false);
    let (plain, _) = draw(&swell_only, true);
    assert_eq!(fnv1a(&off), fnv1a(&plain), "the switch off draws the swell");
    assert_eq!(off_stats.ocean_bodies, 0, "{off_stats:?}");
    assert_eq!(off_stats.ocean_size, 0, "{off_stats:?}");
    let (on, on_stats) = draw(&sea, true);
    assert_eq!(on_stats.ocean_bodies, 1, "{on_stats:?}");
    assert!(changed_fraction(&on, &off) > 0.02, "the ocean changes the frame");
}

/// A second body with another spectrum falls to its swell and is counted; one with the same
/// spectrum shares the maps.
#[test]
fn one_ocean_per_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    let a = sea();
    let mut same = sea();
    if let regia_terrain::WaterShape::Plane { center, .. } = &mut same.shape {
        center[0] = 1000.0;
    }
    let mut other = sea();
    other.ocean = Some(FftOcean { seed: 8, ..ocean() });
    water.set_tier(WaterTier::High);
    water.set_clock(clock(30));
    water.set_bodies(&[&a, &same, &other]);
    let camera = layer_camera(VIEW);
    water
        .probe_ring_vertices(&camera, &[(0, [0.0, 0.0])])
        .expect("probe");
    let stats = water.stats();
    assert_eq!(stats.ocean_bodies, 2, "{stats:?}");
    assert_eq!(stats.ocean_dropped, 1, "{stats:?}");
}

/// `sync_water` asks the renderer for its passes and keeps no flag of its own, so a rebuilt renderer
/// gets the compute pass back, once. A runtime renderer starts with its own passes (the lens effects
/// pass, ADR-1863); the ocean pass comes after them.
#[test]
fn a_rebuilt_renderer_gets_the_ocean_pass_back() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut app = App::new();
    app.init_resource::<WaterSettings>()
        .init_resource::<WaterClock>()
        .insert_resource(WaterState {
            renderer: WaterRenderer::new(&device),
            tier: WaterTier::High,
        })
        .insert_resource(RuntimeRenderer::default())
        .add_systems(Update, sync_water);
    let mut sea = WaterBody::plane([0.0; 3], [100.0, 100.0], WaterLook::ocean());
    sea.ocean = Some(FftOcean::default());
    app.world_mut().spawn(sea);
    let passes = |app: &App| app.world().resource::<RuntimeRenderer>().0.registered_passes();
    let mut want = passes(&app);
    assert!(!want.iter().any(|name| name == OCEAN_PASS_NAME), "{want:?}");
    want.push(OCEAN_PASS_NAME.to_string());
    app.update();
    app.update();
    assert_eq!(passes(&app), want);
    app.insert_resource(RuntimeRenderer::default());
    app.update();
    assert_eq!(passes(&app), want);
}
