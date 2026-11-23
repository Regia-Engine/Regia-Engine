// SPDX-License-Identifier: MPL-2.0
//! Swell on a plane water body (ADR-1620) on a real GPU (lavapipe in CI): the vertex shader's
//! placement of ring-grid vertices against the CPU surface, and the swell sea through the real
//! `FrameRenderer` (hash-stable per tick, moving between ticks, capped by the tier).
//!
//! No golden image: every check compares the GPU with the CPU, or frames of the same run.
//! `REGIA_WATER_FRAME_DUMP=<dir>` writes the frames as PNGs (evidence; not compared).

mod water_scene;

use regia_render::{FrameRenderer, LayerCamera, RenderDevice};
use regia_terrain::water_body::WaterFrameStats;
use regia_terrain::water_body::gpu::{RingFrame, ring_vertex_snapped};
use regia_terrain::{Swell, SwellWave, WaterBody, WaterClock, WaterLook, WaterRenderer, WaterTier};
use water_scene::{Camera, STUDIO_SKY, StreamScene, frame, post, stream_scene};

const W: u32 = 256;
const H: u32 = 144;
const SUN: [f32; 3] = [0.35, -0.65, -0.55];
/// The sea level: just under the stream scene's banks, over its channel.
const LEVEL: f32 = -0.1;

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A 60 m sea with 20 waves (so Low, High and Ultra each draw a different count).
fn sea() -> WaterBody {
    let mut sea = WaterBody::plane([0.0, LEVEL, 0.0], [60.0, 60.0], WaterLook::ocean());
    sea.swell = Some(Swell::open_sea([1.0, 0.35], 0.35, 6.0, 20));
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

/// The GPU places ring-grid vertices where the CPU mirror does (within 1 mm), and near the camera,
/// where every wave is resolved, on the surface `WaterBody::surface` answers (within 1 mm).
#[test]
fn ring_vertices_match_the_cpu_surface_on_the_gpu() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for snap in [false, true] {
        let device = device();
        let water = WaterRenderer::new(&device);
        let sea = sea();
        let clock = WaterClock {
            tick: 4_321,
            ..WaterClock::default()
        };
        water.set_tier(WaterTier::Ultra);
        water.set_clock(clock);
        water.set_bodies(&[&sea]);
        water.set_ring_snap(snap);
        let camera = layer_camera(VIEW);
        // A pass to learn the grid, then the probe points on it.
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
        // Farther out too: the fade there differs from the query, but the mirror must still agree.
        for k in 1..=8 {
            points.push((0, [k as f32 * 7.3, -(k as f32) * 3.1]));
        }
        let probe = water.probe_ring_vertices(&camera, &points).expect("probe");
        assert_eq!(probe.positions.len(), points.len());
        let seconds = clock.seconds();
        let mut worst_mirror = 0.0f32;
        let mut worst_query = 0.0f32;
        let ring = RingFrame {
            grid: &probe.grid,
            shifts: &probe.shifts,
            origin: probe.origin,
        };
        for ((_, local), gpu) in points.iter().zip(&probe.positions) {
            let cpu = ring_vertex_snapped(&sea, None, &ring, seconds, 32, *local).expect("swell");
            for (c, g) in cpu.iter().zip(gpu) {
                worst_mirror = worst_mirror.max((c - g).abs());
            }
            if probe.grid.fade_cell(*local) <= cell {
                let q = sea.surface(gpu[0], gpu[2], &clock).expect("inside the sea");
                worst_query = worst_query.max((q.height - gpu[1]).abs());
            }
        }
        assert!(worst_mirror < 1.0e-3, "GPU vs CPU vertex: {worst_mirror} m");
        assert!(
            worst_query < 1.0e-3,
            "GPU vertex vs CPU surface query: {worst_query} m"
        );
        // With the per-ring snap on, it really moves some probed vertices (the parity above covers it).
        let snapped = points
            .iter()
            .filter(|(_, l)| probe.grid.ring_shift(&probe.shifts, *l) != [0.0, 0.0])
            .count();
        if snap {
            assert!(
                snapped > 0,
                "no probed vertex lies in a snapped ring: {:?}",
                probe.shifts
            );
        } else {
            // Off (the default): no ring moves, the vertices are the unsnapped grid's.
            assert_eq!(snapped, 0, "{:?}", probe.shifts);
            assert!(probe.shifts.iter().all(|d| *d == [0.0, 0.0]));
        }
        // The swell really moves the vertices (the test is not comparing two flat planes).
        let spread = probe
            .positions
            .iter()
            .map(|p| (p[1] - LEVEL).abs())
            .fold(0.0f32, f32::max);
        assert!(spread > 0.05, "{spread}");
    }
}

fn render(
    device: &RenderDevice,
    scene: &StreamScene,
    sea: Option<&WaterBody>,
    tier: WaterTier,
    tick: u64,
    label: &str,
) -> (Vec<u8>, WaterFrameStats) {
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    let water = WaterRenderer::new(device);
    if let Some(sea) = sea {
        water.set_tier(tier);
        water.set_clock(WaterClock {
            tick,
            ..WaterClock::default()
        });
        water.set_bodies(&[sea]);
        water.set_ambient_lux(18_000.0);
        renderer.set_translucent_layers(vec![water.layer()]);
    }
    let post = post();
    let pixels = renderer
        .render_offscreen(device, &frame(scene, VIEW, W, H, &post, SUN))
        .expect("frame")
        .pixels;
    dump(label, &pixels);
    (pixels, water.stats())
}

fn dump(label: &str, pixels: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_WATER_FRAME_DUMP") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = std::path::Path::new(&dir).join(format!("swell_{label}.png"));
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

/// Two renders of one tick are byte-identical; the next second's frame differs; the sea differs
/// from the flat sea of the same level; the ring grid and the tier's wave cap show in the stats.
#[test]
fn swell_sea_is_hash_stable_and_moves() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let sea = sea();
    let (a, stats) = render(&device, &scene, Some(&sea), WaterTier::High, 600, "high_t600");
    let (b, _) = render(
        &device,
        &scene,
        Some(&sea),
        WaterTier::High,
        600,
        "high_t600_again",
    );
    assert_eq!(fnv1a(&a), fnv1a(&b), "two renders of tick 600 differ");
    assert!(stats.frames_drawn >= 1, "{stats:?}");
    assert_eq!(stats.swell_bodies, 1);
    assert_eq!(stats.swell_waves, 16, "High draws 16 of the 20 waves: {stats:?}");
    assert_eq!(stats.swell_waves_dropped, 0);
    assert!(stats.rings >= 1 && stats.ring_triangles > 0, "{stats:?}");
    let (later, _) = render(&device, &scene, Some(&sea), WaterTier::High, 660, "high_t660");
    let moved = changed_fraction(&a, &later);
    assert!(moved > 0.02, "the swell did not move in a second ({moved})");
    let mut flat = sea.clone();
    flat.swell = None;
    let (calm, calm_stats) = render(&device, &scene, Some(&flat), WaterTier::High, 600, "high_flat");
    assert_eq!(calm_stats.swell_bodies, 0);
    let waved = changed_fraction(&a, &calm);
    assert!(waved > 0.02, "the swell sea looks like the flat sea ({waved})");
    let (_, low) = render(&device, &scene, Some(&sea), WaterTier::Low, 600, "low_t600");
    assert_eq!(low.swell_waves, 8, "{low:?}");
    let (_, ultra) = render(&device, &scene, Some(&sea), WaterTier::Ultra, 600, "ultra_t600");
    assert_eq!(ultra.swell_waves, 20, "{ultra:?}");
    assert!(ultra.ring_half_cells >= low.ring_half_cells);
}

/// A single long wave: the crest seen from the side stands above the trough by about its height
/// (the CPU query, which the grid matches near the camera, says where).
#[test]
fn single_wave_surface_queries_bracket_its_height() {
    let mut sea = WaterBody::plane([0.0, 0.0, 0.0], [100.0, 100.0], WaterLook::ocean());
    sea.swell = Some(Swell {
        waves: vec![SwellWave {
            wavelength: 20.0,
            height: 1.0,
            direction: [1.0, 0.0],
            steepness: 0.6,
            phase: 0.0,
        }],
        ..Swell::default()
    });
    let clock = WaterClock::default();
    let heights: Vec<f32> = (0..200)
        .map(|i| sea.surface(i as f32 * 0.1, 0.0, &clock).expect("inside").height)
        .collect();
    let hi = heights.iter().copied().fold(f32::MIN, f32::max);
    let lo = heights.iter().copied().fold(f32::MAX, f32::min);
    assert!(
        (hi - 0.5).abs() < 2.0e-3 && (lo + 0.5).abs() < 2.0e-3,
        "{lo}..{hi}"
    );
}
