// SPDX-License-Identifier: MPL-2.0
//! J24 water acceptance (v0.5 burn-down C11): a recorded flight over a swell sea with a beach,
//! on a real GPU (lavapipe in CI, a card for the cost rows). The single-camera tests
//! (`water_swell_frame`, `water_shore_frame`, `water_extras_frame`) prove each path at one pose and
//! one tick; this file moves the camera and the clock together, so surface-query drift, a shore that
//! falls off after a floating-origin rebase, or a ring-culling gap at some pose would show here.
//!
//! - Along the flight (eight poses, ticks from 0 to a day, across the clock's 640 s lap), the
//!   vertices the GPU draws near the camera sit on the shoaled CPU query within 1 mm, with the
//!   per-ring snap off and on, and vertices over the dry beach stay at the level.
//! - A floating-origin rebase (spec 78 § 12.2) draws the same swell and the same shore: a local
//!   camera plus the offset renders the world camera's frame; the control (the offset withheld
//!   from the water) loses the sea.
//! - Frustum culling of the ring grid changes no pixel at any pose of the flight.
//!
//! No golden image: every check compares the GPU with the CPU, or frames of the same run.
//! `REGIA_WATER_FRAME_DUMP=<dir>` writes the frames as PNGs (evidence; not compared).

mod water_scene;

use std::sync::Arc;

use regia_render::{FrameRenderer, LayerCamera, RenderDevice};
use regia_terrain::water_body::gpu::{RingFrame, ring_vertex_snapped};
use regia_terrain::water_body::{ShoreBakeDesc, ShoreField, WaterFrameStats};
use regia_terrain::{Swell, WaterBody, WaterClock, WaterLook, WaterRenderer, WaterTier};
use water_scene::{Camera, STUDIO_SKY, StreamScene, frame, post};

const W: u32 = 192;
const H: u32 = 108;
const SUN: [f32; 3] = [0.35, -0.65, -0.55];
const LEVEL: f32 = -0.1;
/// Where the beach meets the level (x, m).
const SHORELINE_X: f32 = 6.0;

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A 60 m sea with 20 waves.
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

/// One recorded sample of the flight: where the camera is and which fixed-step tick it is.
struct Sample {
    camera: Camera,
    tick: u64,
}

/// The recorded flight: low and high, near the beach and far out, looking down and along the
/// sea; ticks from 0 to a day, across the 640 s lap of the shader's clock (38 400 ticks at 60 Hz).
const FLIGHT: [Sample; 8] = [
    Sample {
        camera: Camera {
            eye: [-6.0, 3.0, 4.0],
            target: [8.0, LEVEL, -1.0],
            fov_y: 0.9,
        },
        tick: 0,
    },
    Sample {
        camera: Camera {
            // Low over the water, looking up the beach (the shoreline is at x = 6).
            eye: [-10.0, 1.5, -12.0],
            target: [5.0, LEVEL, -20.0],
            fov_y: 0.9,
        },
        tick: 37,
    },
    Sample {
        camera: Camera {
            eye: [0.0, 12.0, 0.0],
            target: [0.5, LEVEL, 0.3],
            fov_y: 0.9,
        },
        tick: 600,
    },
    Sample {
        camera: Camera {
            eye: [-20.0, 0.8, 15.0],
            target: [0.0, LEVEL, 0.0],
            fov_y: 0.9,
        },
        tick: 38_400,
    },
    Sample {
        camera: Camera {
            eye: [-20.0, 0.8, 15.0],
            target: [0.0, LEVEL, 0.0],
            fov_y: 0.9,
        },
        tick: 38_401,
    },
    Sample {
        camera: Camera {
            // Half a metre off the shoreline (x = 6): the probe grid crosses onto the dry beach.
            eye: [5.5, 2.0, -5.0],
            target: [12.0, LEVEL, -5.0],
            fov_y: 0.9,
        },
        tick: 216_000,
    },
    Sample {
        camera: Camera {
            // Over the deep water on the far side (the beach is dry beyond x = 6).
            eye: [-30.0, 4.0, 30.0],
            target: [0.0, LEVEL, 0.0],
            fov_y: 0.9,
        },
        tick: 2_592_000,
    },
    Sample {
        camera: Camera {
            eye: [-40.0, 6.0, -40.0],
            target: [-30.0, LEVEL, -35.0],
            fov_y: 0.9,
        },
        tick: 5_184_007,
    },
];

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

/// Worst GPU-vs-CPU differences of one probed sample (m), and how many probe points were dry.
#[derive(Debug, Default)]
struct Agreement {
    mirror: f32,
    query: f32,
    queried: usize,
    dry: usize,
    dry_moved: f32,
    /// Probe points whose ring the per-ring snap moved.
    snapped: usize,
}

/// Probes the drawn ring vertices around one sample's camera and compares them with the CPU mirror
/// everywhere, and with the shoaled surface query near the camera (where every wave is resolved).
/// Besides the inner square, points in every outer ring (where the per-ring snap moves vertices)
/// are probed for the mirror.
fn probe_sample(water: &WaterRenderer, sea: &WaterBody, field: &ShoreField, sample: &Sample) -> Agreement {
    let clock = clock(sample.tick);
    water.set_clock(clock);
    let camera = layer_camera(sample.camera);
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
    // The middle of every outer ring (ring `r` spans `inner · 2^(r-1)` to `inner · 2^r`), eight
    // directions each: the snap moves whole rings, and at a given eye often only the far ones.
    let inner = first.grid.inner_half_size();
    for ring in 1..first.grid.rings.max(1) {
        let radius = 1.5 * inner * (1u32 << (ring - 1)) as f32;
        for k in 0..8 {
            let angle = k as f32 * std::f32::consts::FRAC_PI_4 + 0.3;
            points.push((0, [radius * angle.cos(), radius * angle.sin()]));
        }
    }
    let probe = water.probe_ring_vertices(&camera, &points).expect("probe");
    assert_eq!(probe.positions.len(), points.len());
    let ring = RingFrame {
        grid: &probe.grid,
        shifts: &probe.shifts,
        origin: probe.origin,
    };
    let seconds = clock.seconds();
    let mut out = Agreement::default();
    for ((_, local), gpu) in points.iter().zip(&probe.positions) {
        let cpu = ring_vertex_snapped(sea, Some(field), &ring, seconds, 32, *local).expect("swell");
        for (c, g) in cpu.iter().zip(gpu) {
            out.mirror = out.mirror.max((c - g).abs());
        }
        let shift = probe.grid.ring_shift(&probe.shifts, *local);
        if shift != [0.0, 0.0] {
            out.snapped += 1;
        }
        let rest = [
            probe.origin[0] as f32 + local[0] + shift[0],
            probe.origin[1] as f32 + local[1] + shift[1],
        ];
        let Some(bed) = field.sample(rest[0], rest[1]) else {
            continue;
        };
        if bed.depth <= 0.0 {
            out.dry += 1;
            out.dry_moved = out.dry_moved.max((gpu[1] - LEVEL).abs());
            continue;
        }
        if probe.grid.fade_cell(*local) <= cell
            && let Some(q) = sea.surface_on(gpu[0], gpu[2], &clock, Some(field))
        {
            out.queried += 1;
            out.query = out.query.max((q.height - gpu[1]).abs());
        }
    }
    out
}

/// Along the whole flight, with the per-ring snap off and on, the GPU draws the vertices near the
/// camera on the shoaled surface query (1 mm, the single-pose bar), the mirror agrees out to the
/// last ring (where the snap moves vertices: some do with it on, none with it off), and the dry
/// beach stays at the level.
#[test]
fn a_recorded_flight_keeps_the_drawn_swell_on_the_surface_query() {
    let _gpu = serial();
    let device = device();
    let sea = sea();
    let field = beach();
    let mut dry_seen = 0;
    let mut snapped_seen = 0;
    for snap in [false, true] {
        let water = WaterRenderer::new(&device);
        water.set_tier(WaterTier::Ultra);
        water.set_bodies(&[&sea]);
        water.set_shores(&[Some(Arc::clone(&field))]);
        water.set_ring_snap(snap);
        for (i, sample) in FLIGHT.iter().enumerate() {
            let a = probe_sample(&water, &sea, &field, sample);
            assert!(
                a.mirror < 1.0e-3,
                "sample {i} (tick {}, snap {snap}): GPU vs CPU vertex {} m",
                sample.tick,
                a.mirror
            );
            assert!(
                a.queried > 0,
                "sample {i}: no wet probe point near the camera: {a:?}"
            );
            assert!(
                a.query < 1.0e-3,
                "sample {i} (tick {}, snap {snap}): GPU vertex vs CPU shoaled query {} m",
                sample.tick,
                a.query
            );
            assert!(
                a.dry_moved < 1.0e-4,
                "sample {i}: a dry vertex moved by {} m",
                a.dry_moved
            );
            if !snap {
                assert_eq!(a.snapped, 0, "sample {i}: a ring moved with the snap off");
            }
            snapped_seen += a.snapped;
            dry_seen += a.dry;
        }
    }
    assert!(dry_seen > 0, "the flight never probed the dry beach");
    assert!(
        snapped_seen > 0,
        "no probed vertex lies in a snapped ring: the snap pass proves nothing"
    );
}

/// Renders the sea (and the beach, when given) with nothing else in the frame, so the picture
/// depends only on the water and the view direction.
fn render(
    device: &RenderDevice,
    sea: &WaterBody,
    field: Option<&Arc<ShoreField>>,
    camera: Camera,
    tick: u64,
    setup: impl Fn(&WaterRenderer),
    label: &str,
) -> (Vec<u8>, WaterFrameStats) {
    let empty = StreamScene {
        objects: Vec::new(),
        stream: sea.clone(),
        pebbles: Vec::new(),
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    let water = WaterRenderer::new(device);
    water.set_tier(WaterTier::High);
    water.set_clock(clock(tick));
    water.set_bodies(&[sea]);
    if let Some(field) = field {
        water.set_shores(&[Some(Arc::clone(field))]);
    }
    water.set_ambient_lux(18_000.0);
    setup(&water);
    renderer.set_translucent_layers(vec![water.layer()]);
    let post = post();
    let pixels = renderer
        .render_offscreen(device, &frame(&empty, camera, W, H, &post, SUN))
        .expect("frame")
        .pixels;
    dump(label, &pixels);
    (pixels, water.stats())
}

fn no_water_frame(device: &RenderDevice, camera: Camera) -> Vec<u8> {
    let empty = StreamScene {
        objects: Vec::new(),
        stream: sea(),
        pebbles: Vec::new(),
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    let post = post();
    renderer
        .render_offscreen(device, &frame(&empty, camera, W, H, &post, SUN))
        .expect("frame")
        .pixels
}

fn dump(label: &str, pixels: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_WATER_FRAME_DUMP") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = std::path::Path::new(&dir).join(format!("flight_{label}.png"));
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

/// Share of pixels whose channels differ by more than two levels.
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

/// A rebase by whole 1024 m cells and 64 m up (exact in `f32`).
const SHIFT: [f32; 3] = [1024.0, 64.0, -1024.0];

fn local(camera: Camera) -> Camera {
    Camera {
        eye: std::array::from_fn(|a| camera.eye[a] - SHIFT[a]),
        target: std::array::from_fn(|a| camera.target[a] - SHIFT[a]),
        fov_y: camera.fov_y,
    }
}

/// Spec 78 § 12.2 with a swell and a shore: after a rebase the camera speaks local coordinates and
/// the water renderer gets the offset; the swell and the shoaled beach must print where they did.
/// The control withholds the offset from the water and must lose the sea.
#[test]
fn a_rebase_keeps_the_swell_and_the_shore_in_place() {
    let _gpu = serial();
    let device = device();
    let sea = sea();
    let field = beach();
    for (i, sample) in [&FLIGHT[0], &FLIGHT[3]].into_iter().enumerate() {
        let (world, stats) = render(
            &device,
            &sea,
            Some(&field),
            sample.camera,
            sample.tick,
            |_| {},
            &format!("rebase_{i}_world"),
        );
        assert!(stats.frames_drawn >= 1 && stats.shore_fields == 1, "{stats:?}");
        let dry = no_water_frame(&device, sample.camera);
        let shows = changed_fraction(&world, &dry);
        assert!(shows > 0.1, "sample {i}: the sea must be in the frame ({shows})");
        let (rebased, rebased_stats) = render(
            &device,
            &sea,
            Some(&field),
            local(sample.camera),
            sample.tick,
            |water| water.set_origin_offset(SHIFT),
            &format!("rebase_{i}_local"),
        );
        assert_eq!(rebased_stats.shore_fields, 1, "{rebased_stats:?}");
        assert_eq!(
            rebased_stats.ring_origin, stats.ring_origin,
            "sample {i}: the ring grid moved with the rebase"
        );
        let off = changed_fraction(&world, &rebased);
        let (control, _) = render(
            &device,
            &sea,
            Some(&field),
            local(sample.camera),
            sample.tick,
            |_| {},
            &format!("rebase_{i}_control"),
        );
        let lost = changed_fraction(&world, &control);
        eprintln!(
            "rebase sample {i}: water shows on {:.1} %, rebased differs on {:.3} %, control on {:.1} %",
            shows * 100.0,
            off * 100.0,
            lost * 100.0
        );
        assert!(
            lost > 0.5 * shows,
            "sample {i}: the control must lose the sea ({lost} vs {shows})"
        );
        assert!(off < 0.005, "sample {i}: the rebased frame differs on {off}");
    }
}

/// Frustum culling of the ring grid (ADR-2342) leaves every pixel as it was at every pose of the
/// flight, the beach included: a chunk culled by mistake would leave a gap in the sea.
#[test]
fn ring_culling_leaves_no_gap_along_the_flight() {
    let _gpu = serial();
    let device = device();
    let sea = sea();
    let field = beach();
    let mut culled_any = false;
    for (i, sample) in FLIGHT.iter().enumerate() {
        let (culled, on) = render(
            &device,
            &sea,
            Some(&field),
            sample.camera,
            sample.tick,
            |water| water.set_ring_culling(true),
            &format!("cull_{i}_on"),
        );
        let (full, off) = render(
            &device,
            &sea,
            Some(&field),
            sample.camera,
            sample.tick,
            |water| water.set_ring_culling(false),
            &format!("cull_{i}_off"),
        );
        assert_eq!(off.ring_chunks_culled, 0, "{off:?}");
        culled_any |= on.ring_chunks_culled > 0;
        if device.capabilities.backend == wgpu::Backend::Metal {
            // ADR-3373: Metal's fast-math libraries move a few seam pixels by a level or three.
            assert!(
                changed_fraction(&culled, &full) < 0.002,
                "sample {i}: culling changed the frame"
            );
        } else {
            assert!(
                culled == full,
                "sample {i}: culling changed pixels ({} changed by more than 2 levels)",
                changed_fraction(&culled, &full)
            );
        }
    }
    assert!(
        culled_any,
        "no pose of the flight culled a chunk: the test proves nothing"
    );
}

/// GPU cost of the water per tier on a card along the flight at 1920 x 1080: the swell on Low,
/// High and Ultra, the FFT ocean candidate (opt-in, ADR-1969) on High and Ultra, all with the
/// beach (`gpu_timing`, the median of 48 timed frames after 16 warm ones). Prints one line per
/// row for the acceptance report; asserts only that the card times the draw. Run with a pinned
/// card: `cargo test -p regia_terrain --test water_flight -- --ignored --nocapture water_tier_cost_bench`.
#[test]
#[ignore = "needs a GPU with TIMESTAMP_QUERY; prints numbers"]
fn water_tier_cost_bench() {
    let _gpu = serial();
    let device = device();
    let field = beach();
    let (w, h) = (1920, 1080);
    let empty = StreamScene {
        objects: Vec::new(),
        stream: sea(),
        pebbles: Vec::new(),
    };
    let rows = [
        ("swell", WaterTier::Low, false),
        ("swell", WaterTier::High, false),
        ("swell", WaterTier::Ultra, false),
        ("ocean", WaterTier::High, true),
        ("ocean", WaterTier::Ultra, true),
    ];
    for (label, tier, ocean) in rows {
        let mut body = sea();
        if ocean {
            body.ocean = Some(regia_terrain::FftOcean::default());
        }
        let mut renderer = FrameRenderer::new();
        renderer.set_studio_sky(&device, Some(STUDIO_SKY));
        let water = WaterRenderer::new(&device);
        if !water.stats().gpu_ms_supported {
            println!("water_tier_cost_bench: no TIMESTAMP_QUERY on this device, nothing measured");
            return;
        }
        water.set_tier(tier);
        water.set_bodies(&[&body]);
        water.set_shores(&[Some(Arc::clone(&field))]);
        water.set_ambient_lux(18_000.0);
        renderer.set_translucent_layers(vec![water.layer()]);
        if ocean {
            let _ = renderer.register_pass(water.ocean_pass());
        }
        let post = post();
        let mut samples = Vec::new();
        for i in 0..64u64 {
            let sample = &FLIGHT[(i as usize / 8) % FLIGHT.len()];
            water.set_clock(clock(sample.tick + i));
            water.set_gpu_timing(if i >= 16 { 1 } else { 0 });
            renderer
                .render_offscreen(&device, &frame(&empty, sample.camera, w, h, &post, SUN))
                .expect("frame");
            if i >= 16 {
                samples.push(water.stats().gpu_ms);
            }
        }
        samples.sort_by(f32::total_cmp);
        let stats = water.stats();
        let median = samples[samples.len() / 2];
        println!(
            "water_tier_cost_bench {label} {tier:?}: median {median:.4} ms, min {:.4}, max {:.4}, \
             {} waves, ring tris {}, ocean size {}, shore fields {}",
            samples[0],
            samples[samples.len() - 1],
            stats.swell_waves,
            stats.ring_triangles,
            stats.ocean_size,
            stats.shore_fields
        );
        assert!(median > 0.0, "the card timed nothing");
    }
}
