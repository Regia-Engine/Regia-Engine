// SPDX-License-Identifier: MPL-2.0
//! The shore bake on a real GPU (spec 87, ADR-1828; lavapipe in CI): the vertex shader shoals
//! the swell by the same baked depth the CPU query uses, vertices over dry texels do not move,
//! the tier gate binds the field on High and Ultra only, and frames stay hash-stable per tick.
//!
//! No golden image: every check compares the GPU with the CPU, or frames of the same run.
//! `REGIA_WATER_FRAME_DUMP=<dir>` writes the frames as PNGs (evidence; not compared).

mod water_scene;

use std::sync::Arc;

use regia_render::{FrameRenderer, LayerCamera, RenderDevice};
use regia_terrain::water_body::gpu::{RingFrame, ring_vertex_snapped};
use regia_terrain::water_body::{ShoreBakeDesc, ShoreField, WaterFrameStats};
use regia_terrain::{Swell, WaterBody, WaterClock, WaterLook, WaterRenderer, WaterTier};
use water_scene::{Camera, STUDIO_SKY, StreamScene, frame, post, stream_scene};

const W: u32 = 256;
const H: u32 = 144;
const SUN: [f32; 3] = [0.35, -0.65, -0.55];
const LEVEL: f32 = -0.1;
/// Where the beach meets the level (x, m).
const SHORELINE_X: f32 = 6.0;

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn sea() -> WaterBody {
    let mut sea = WaterBody::plane([0.0, LEVEL, 0.0], [60.0, 60.0], WaterLook::ocean());
    sea.swell = Some(Swell::open_sea([1.0, 0.35], 0.35, 6.0, 20));
    sea
}

/// A beach rising 1:8 toward +x: 8 m deep 64 m out, dry beyond x = 6.
fn beach() -> Arc<ShoreField> {
    let field = ShoreField::bake(
        &ShoreBakeDesc {
            level: LEVEL,
            min: [-60.0, -60.0],
            max: [60.0, 60.0],
            cell: 0.5,
        },
        |x, _| LEVEL + (x - SHORELINE_X) / 8.0,
    )
    .expect("bake");
    Arc::new(field)
}

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

/// With a shore field the GPU vertex equals the CPU mirror within 1 mm, near the camera it lies
/// on the shoaled CPU surface within 1 mm, over dry texels the vertex sits at its rest point, and
/// the shoaled grid differs from the unshoaled one near the shore.
#[test]
fn shoaled_ring_vertices_match_the_cpu_on_the_gpu() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for snap in [false, true] {
        let device = device();
        let water = WaterRenderer::new(&device);
        let sea = sea();
        let field = beach();
        let clock = WaterClock {
            tick: 4_321,
            ..WaterClock::default()
        };
        water.set_tier(WaterTier::Ultra);
        water.set_clock(clock);
        water.set_bodies(&[&sea]);
        water.set_shores(&[Some(Arc::clone(&field))]);
        water.set_ring_snap(snap);
        let camera = layer_camera(VIEW);
        let first = water
            .probe_ring_vertices(&camera, &[(0, [0.0, 0.0])])
            .expect("probe");
        let cell = first.grid.cell();
        let mut points: Vec<(u32, [f32; 2])> = Vec::new();
        for j in -6i32..=6 {
            for i in -6i32..=12 {
                points.push((0, [i as f32 * cell * 1.5, j as f32 * cell * 2.5]));
            }
        }
        for k in 1..=8 {
            points.push((0, [k as f32 * 7.3, -(k as f32) * 3.1]));
        }
        let probe = water.probe_ring_vertices(&camera, &points).expect("probe");
        assert_eq!(water.stats().shore_fields, 1);
        let seconds = clock.seconds();
        let mut worst_mirror = 0.0f32;
        let mut worst_query = 0.0f32;
        let mut dry = 0;
        let mut shoaled = 0;
        let ring = RingFrame {
            grid: &probe.grid,
            shifts: &probe.shifts,
            origin: probe.origin,
        };
        for ((_, local), gpu) in points.iter().zip(&probe.positions) {
            let cpu = ring_vertex_snapped(&sea, Some(&field), &ring, seconds, 32, *local).expect("swell");
            for (c, g) in cpu.iter().zip(gpu) {
                worst_mirror = worst_mirror.max((c - g).abs());
            }
            let shift = probe.grid.ring_shift(&probe.shifts, *local);
            let rest = [
                probe.origin[0] as f32 + local[0] + shift[0],
                probe.origin[1] as f32 + local[1] + shift[1],
            ];
            let bed = field.sample(rest[0], rest[1]).expect("inside the field");
            if bed.depth <= 0.0 {
                dry += 1;
                // The waves are scaled to zero on land (up to rounding at the shoreline itself).
                assert!((gpu[1] - LEVEL).abs() < 1.0e-4, "a dry vertex moved: {gpu:?}");
                continue;
            }
            let flat = ring_vertex_snapped(&sea, None, &ring, seconds, 32, *local).expect("swell");
            if (flat[1] - cpu[1]).abs() > 1.0e-3 {
                shoaled += 1;
            }
            if probe.grid.fade_cell(*local) <= cell {
                let q = sea.surface_on(gpu[0], gpu[2], &clock, Some(&field)).expect("wet");
                worst_query = worst_query.max((q.height - gpu[1]).abs());
            }
        }
        assert!(worst_mirror < 1.0e-3, "GPU vs CPU vertex: {worst_mirror} m");
        assert!(
            worst_query < 1.0e-3,
            "GPU vertex vs CPU shoaled query: {worst_query} m"
        );
        assert!(dry > 0, "no probe point over the beach");
        assert!(shoaled > 0, "the field changed no vertex");
    }
}

fn render(
    device: &RenderDevice,
    scene: &StreamScene,
    shore: Option<Arc<ShoreField>>,
    tier: WaterTier,
    tick: u64,
    label: &str,
) -> (Vec<u8>, WaterFrameStats) {
    let sea = sea();
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    let water = WaterRenderer::new(device);
    water.set_tier(tier);
    water.set_clock(WaterClock {
        tick,
        ..WaterClock::default()
    });
    water.set_bodies(&[&sea]);
    water.set_shores(&[shore]);
    water.set_ambient_lux(18_000.0);
    renderer.set_translucent_layers(vec![water.layer()]);
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
    let path = std::path::Path::new(&dir).join(format!("shore_{label}.png"));
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

/// High binds the field and its frame is hash-stable and differs from the unshoaled sea; Low
/// binds none and draws what it drew without a field.
#[test]
fn shore_frame_is_tiered_and_hash_stable() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let field = beach();
    let (a, stats) = render(
        &device,
        &scene,
        Some(Arc::clone(&field)),
        WaterTier::High,
        600,
        "high",
    );
    let (b, _) = render(
        &device,
        &scene,
        Some(Arc::clone(&field)),
        WaterTier::High,
        600,
        "high_again",
    );
    assert_eq!(fnv1a(&a), fnv1a(&b), "two renders of tick 600 differ");
    assert_eq!(stats.shore_fields, 1, "{stats:?}");
    assert_eq!(stats.shore_fields_dropped, 0);
    assert!(stats.shore_texels >= 241 * 241, "{stats:?}");
    let (open, open_stats) = render(&device, &scene, None, WaterTier::High, 600, "high_no_shore");
    assert_eq!(open_stats.shore_fields, 0);
    let diff = changed_fraction(&a, &open);
    assert!(diff > 0.005, "the shore changed nothing on screen ({diff})");
    let (low, low_stats) = render(
        &device,
        &scene,
        Some(Arc::clone(&field)),
        WaterTier::Low,
        600,
        "low",
    );
    assert_eq!(low_stats.shore_fields, 0, "Low binds no field: {low_stats:?}");
    let (low_open, _) = render(&device, &scene, None, WaterTier::Low, 600, "low_no_shore");
    assert_eq!(fnv1a(&low), fnv1a(&low_open), "Low must not draw the field");
}

/// A small beach field around a plane centered at `x` (40 × 40 texels).
fn small_beach(x: f32) -> Arc<ShoreField> {
    let field = ShoreField::bake(
        &ShoreBakeDesc {
            level: LEVEL,
            min: [x - 10.0, -10.0],
            max: [x + 10.0, 10.0],
            cell: 0.5,
        },
        move |px, _| LEVEL + (px - x - 4.0) / 8.0,
    )
    .expect("bake");
    Arc::new(field)
}

/// Spec 87 § 9: a frame binds at most `MAX_SHORE_FIELDS` (4) distinct fields. A fifth distinct
/// field is left out and counted in `shore_fields_dropped`; a field shared by two planes takes one
/// layer and is not dropped; freeing a layer lets the dropped field in on the next frame.
#[test]
fn a_fifth_distinct_shore_field_is_dropped_and_counted() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    // Six planes 100 m apart; the first carries the swell the probe needs.
    let planes: Vec<WaterBody> = (0..6)
        .map(|i| {
            if i == 0 {
                sea()
            } else {
                WaterBody::plane([i as f32 * 100.0, LEVEL, 0.0], [20.0, 20.0], WaterLook::ocean())
            }
        })
        .collect();
    let refs: Vec<&WaterBody> = planes.iter().collect();
    let fields: Vec<Arc<ShoreField>> = (0..5).map(|i| small_beach(i as f32 * 100.0)).collect();
    water.set_tier(WaterTier::High);
    water.set_clock(WaterClock {
        tick: 600,
        ..WaterClock::default()
    });
    water.set_bodies(&refs);
    // Planes 0-3 own fields 0-3, plane 4 shares field 0, plane 5 brings a fifth distinct field.
    let mut shores: Vec<Option<Arc<ShoreField>>> = fields[..4].iter().map(|f| Some(Arc::clone(f))).collect();
    shores.push(Some(Arc::clone(&fields[0])));
    shores.push(Some(Arc::clone(&fields[4])));
    water.set_shores(&shores);
    let camera = layer_camera(VIEW);
    water
        .probe_ring_vertices(&camera, &[(0, [0.0, 0.0])])
        .expect("probe");
    let stats = water.stats();
    assert_eq!(stats.shore_fields, 4, "{stats:?}");
    assert_eq!(
        stats.shore_fields_dropped, 1,
        "only the fifth distinct field drops: {stats:?}"
    );
    // Plane 3 loses its field: the fifth one now fits and nothing is dropped.
    shores[3] = None;
    water.set_shores(&shores);
    water
        .probe_ring_vertices(&camera, &[(0, [0.0, 0.0])])
        .expect("probe");
    let stats = water.stats();
    assert_eq!(stats.shore_fields, 4, "{stats:?}");
    assert_eq!(stats.shore_fields_dropped, 0, "{stats:?}");
}
