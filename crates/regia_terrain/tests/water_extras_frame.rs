// SPDX-License-Identifier: MPL-2.0
//! Water extras (ADR-2342) on a real GPU (lavapipe in CI): ring-grid frustum culling changes no
//! pixel, the underwater view fogs the scene and draws the surface from below, and the timed water
//! draw reports milliseconds where the device can time passes.
//!
//! Every extra is off by default (ADR-2342): the default frames equal the frames with every switch
//! off. No golden image: every check compares frames of the same run.
//! `REGIA_WATER_FRAME_DUMP=<dir>` writes the underwater plates as PNGs for a look by eye (evidence;
//! not compared).

mod water_scene;

use regia_render::{FrameRenderer, LayerCamera, RenderDevice};
use regia_terrain::water_body::WaterFrameStats;
use regia_terrain::{FftOcean, Swell, WaterBody, WaterClock, WaterLook, WaterRenderer, WaterTier};
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

fn sea(level: f32) -> WaterBody {
    let mut sea = WaterBody::plane([0.0, level, 0.0], [60.0, 60.0], WaterLook::ocean());
    sea.swell = Some(Swell::open_sea([1.0, 0.35], 0.35, 6.0, 20));
    sea
}

/// A sea wider than the scene with an FFT ocean and no swell of its own (it rides the ring grid
/// with the ocean's stand-in).
fn ocean_sea(level: f32) -> WaterBody {
    let mut sea = WaterBody::plane([0.0, level, 0.0], [400.0, 400.0], WaterLook::ocean());
    sea.ocean = Some(FftOcean {
        wind_speed: 14.0,
        ..FftOcean::default()
    });
    sea
}

/// Over the sea from 3 m up, looking along it: the part of the grid behind the eye is outside the
/// frustum.
const ABOVE: Camera = Camera {
    eye: [-6.0, 3.0, 4.0],
    target: [8.0, LEVEL, -1.0],
    fov_y: 0.9,
};

/// The sea of the underwater frames: high over the scene, so the eye is in the water column
/// (1.3 m under the surface) above the scene's banks.
const HIGH_LEVEL: f32 = 3.0;

/// Looking out across the water and a little up.
const UNDER: Camera = Camera {
    eye: [-6.0, 1.7, 4.0],
    target: [8.0, 2.4, -1.0],
    fov_y: 0.9,
};

/// Looking up at the surface from under it: Snell's window and the mirror around it.
const UNDER_UP: Camera = Camera {
    eye: [-6.0, 1.7, 4.0],
    target: [-3.0, 3.6, 1.5],
    fov_y: 1.1,
};

#[derive(Clone, Copy)]
struct Settings {
    /// `None` leaves the renderer's default.
    culling: Option<bool>,
    /// `None` leaves the renderer's default.
    underwater: Option<bool>,
    /// `None` leaves the renderer's default.
    snap: Option<bool>,
    level: f32,
    ocean: bool,
}

const DEFAULT: Settings = Settings {
    culling: Some(true),
    underwater: Some(true),
    snap: None,
    level: LEVEL,
    ocean: false,
};

/// Every switch at the renderer's default.
const UNSET: Settings = Settings {
    culling: None,
    underwater: None,
    snap: None,
    level: LEVEL,
    ocean: false,
};

/// Every switch explicitly off.
const OFF: Settings = Settings {
    culling: Some(false),
    underwater: Some(false),
    snap: Some(false),
    level: LEVEL,
    ocean: false,
};

fn render(
    device: &RenderDevice,
    scene: &StreamScene,
    camera: Camera,
    set: &Settings,
) -> (Vec<u8>, WaterFrameStats) {
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    let water = WaterRenderer::new(device);
    water.set_tier(WaterTier::High);
    water.set_clock(WaterClock {
        tick: 600,
        ..WaterClock::default()
    });
    let body = if set.ocean {
        ocean_sea(set.level)
    } else {
        sea(set.level)
    };
    water.set_bodies(&[&body]);
    water.set_ambient_lux(18_000.0);
    if let Some(on) = set.culling {
        water.set_ring_culling(on);
    }
    if let Some(on) = set.underwater {
        water.set_underwater(on);
    }
    if let Some(on) = set.snap {
        water.set_ring_snap(on);
    }
    renderer.set_translucent_layers(vec![water.layer()]);
    let post = post();
    let pixels = renderer
        .render_offscreen(device, &frame(scene, camera, W, H, &post, SUN))
        .expect("frame")
        .pixels;
    (pixels, water.stats())
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

fn mean(pixels: &[u8]) -> [f32; 3] {
    let mut sum = [0.0f64; 3];
    for p in pixels.as_chunks::<4>().0 {
        for c in 0..3 {
            sum[c] += f64::from(p[c]);
        }
    }
    let n = f64::from(W * H);
    [(sum[0] / n) as f32, (sum[1] / n) as f32, (sum[2] / n) as f32]
}

/// Culling the ring grid by the frustum leaves every pixel as it was, and drops chunks behind the
/// eye (fewer triangles drawn than uploaded).
#[test]
fn ring_culling_changes_no_pixel() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let (culled, on) = render(&device, &scene, ABOVE, &DEFAULT);
    let (full, off) = render(
        &device,
        &scene,
        ABOVE,
        &Settings {
            culling: Some(false),
            ..DEFAULT
        },
    );
    if device.capabilities.backend == wgpu::Backend::Metal {
        // ADR-3373: on Metal (fast-math shader libraries) the culled draw list moves a few seam
        // pixels by a level or three (M5: 9 pixels, at most 3, in one row band), while two culled
        // frames match exactly. A wrong cull drops whole chunks: hundreds of pixels by far more.
        let (count, worst) = pixel_difference(&culled, &full);
        assert!(
            count <= 32 && worst <= 4,
            "culling changed {count} pixels, by up to {worst}"
        );
    } else {
        assert_eq!(culled, full, "culling changed pixels");
    }
    assert!(on.ring_chunks > 1 && on.ring_chunks_culled > 0, "{on:?}");
    assert!(on.ring_triangles_drawn < on.ring_triangles, "{on:?}");
    assert_eq!(off.ring_chunks_culled, 0);
    assert_eq!(off.ring_triangles_drawn, off.ring_triangles);
    assert!(on.frames_drawn >= 1);
}

/// How many pixels of two RGBA8 frames differ, and the largest channel difference.
fn pixel_difference(a: &[u8], b: &[u8]) -> (usize, u8) {
    let (mut count, mut worst) = (0, 0u8);
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        if pa != pb {
            count += 1;
            worst = worst.max(pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0));
        }
    }
    (count, worst)
}

/// ADR-3373 diagnostics for a backend where `ring_culling_changes_no_pixel` fails (Metal): how many
/// pixels culling moves, by how much and where, against two culled frames of their own (the
/// frame's determinism on that backend). `cargo test -p regia_terrain --test water_extras_frame --
/// --ignored --nocapture ring_culling_diagnostics`.
#[test]
#[ignore = "diagnostics: prints what ring culling changes, asserts nothing"]
fn ring_culling_diagnostics() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let (culled, on) = render(&device, &scene, ABOVE, &DEFAULT);
    let (again, _) = render(&device, &scene, ABOVE, &DEFAULT);
    let (full, off) = render(
        &device,
        &scene,
        ABOVE,
        &Settings {
            culling: Some(false),
            ..DEFAULT
        },
    );
    let info = device.adapter.get_info();
    println!(
        "adapter {} ({:?}); culled {on:?}; full {off:?}",
        info.name, info.backend
    );
    for (label, other) in [("culled again", &again), ("not culled", &full)] {
        let (mut count, mut worst) = (0usize, 0u8);
        let (mut x0, mut y0, mut x1, mut y1) = (W, H, 0, 0);
        for (i, (a, b)) in culled
            .as_chunks::<4>()
            .0
            .iter()
            .zip(other.as_chunks::<4>().0)
            .enumerate()
        {
            if a == b {
                continue;
            }
            count += 1;
            worst = worst.max(a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0));
            let (x, y) = (i as u32 % W, i as u32 / W);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
        println!(
            "{label}: {count} pixels differ, largest channel difference {worst}, box x {x0}..={x1} y {y0}..={y1}"
        );
    }
}

/// Under the surface the water column fogs the scene (a blue-green shift, most pixels change), the
/// frame is hash-stable, and above the surface the switch changes nothing.
#[test]
fn underwater_view_fogs_the_scene() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let high = Settings {
        level: HIGH_LEVEL,
        ..DEFAULT
    };
    let (under, stats) = render(&device, &scene, UNDER, &high);
    assert!(stats.underwater, "{stats:?}");
    assert!(
        stats.underwater_depth_m > 0.9 && stats.underwater_depth_m < 1.6,
        "{}",
        stats.underwater_depth_m
    );
    let (again, _) = render(&device, &scene, UNDER, &high);
    assert_eq!(under, again, "the underwater frame is not deterministic");
    let (off_pixels, off_stats) = render(
        &device,
        &scene,
        UNDER,
        &Settings {
            underwater: Some(false),
            ..high
        },
    );
    assert!(!off_stats.underwater);
    dump("underwater_on", &under);
    dump("underwater_off", &off_pixels);
    let (up, up_stats) = render(&device, &scene, UNDER_UP, &high);
    assert!(up_stats.underwater);
    dump("underwater_up_window", &up);
    assert!(
        changed_fraction(&under, &off_pixels) > 0.25,
        "the fog should reach most pixels: {}",
        changed_fraction(&under, &off_pixels)
    );
    // Water absorbs red first: under water the red channel falls below the green and blue ones
    // by more than it does without the fog.
    let (m_on, m_off) = (mean(&under), mean(&off_pixels));
    assert!(
        m_on[0] / (m_on[1] + m_on[2]).max(1.0) < m_off[0] / (m_off[1] + m_off[2]).max(1.0),
        "red share under water {m_on:?} vs without the fog {m_off:?}"
    );
    // Above the surface the switch is inert.
    let (a, sa) = render(&device, &scene, ABOVE, &DEFAULT);
    let (b, _) = render(
        &device,
        &scene,
        ABOVE,
        &Settings {
            underwater: Some(false),
            ..DEFAULT
        },
    );
    assert!(!sa.underwater && sa.underwater_depth_m == 0.0);
    assert_eq!(a, b);
}

/// The timed draw returns milliseconds on a device with timestamp queries, `None` without, and
/// leaves the frame's own draw (the armed state and the frame count) alone.
#[test]
fn timed_draw_reports_or_declines() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    water.set_tier(WaterTier::High);
    water.set_bodies(&[&sea(LEVEL)]);
    let depth = device
        .device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("water timing test depth"),
            size: wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default());
    let camera = LayerCamera {
        eye: ABOVE.eye,
        target: ABOVE.target,
        fov_y: ABOVE.fov_y,
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
    };
    let supported = water.stats().gpu_ms_supported;
    let ms = water.measure_gpu_ms(&depth, &camera).expect("measure");
    let stats = water.stats();
    assert_eq!(stats.frames_drawn, 0, "a timed draw is not a frame");
    if supported {
        let ms = ms.expect("timestamps supported");
        assert!(ms.is_finite() && ms >= 0.0, "{ms}");
        assert_eq!(stats.gpu_ms_samples, 1);
        assert_eq!(stats.gpu_ms_pixels, W * H);
    } else {
        assert!(ms.is_none());
        assert_eq!(stats.gpu_ms_samples, 0);
    }
}

/// Every extra is off by default and the default frame is the frame with every switch off: under
/// the surface (where the view would draw), over it, and over an FFT ocean.
#[test]
fn defaults_are_off_and_change_nothing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    for (camera, level, ocean) in [
        (UNDER, HIGH_LEVEL, false),
        (ABOVE, LEVEL, false),
        (ABOVE, LEVEL, true),
    ] {
        let (unset, stats) = render(
            &device,
            &scene,
            camera,
            &Settings {
                level,
                ocean,
                ..UNSET
            },
        );
        let (off, _) = render(&device, &scene, camera, &Settings { level, ocean, ..OFF });
        assert_eq!(unset, off, "a default differs from off (ocean {ocean})");
        assert!(!stats.underwater && stats.underwater_depth_m == 0.0, "{stats:?}");
        assert_eq!(stats.ring_chunks_culled, 0);
        assert_eq!(stats.ring_triangles_drawn, stats.ring_triangles);
        assert_eq!((stats.gpu_ms_samples, stats.gpu_ms), (0, 0.0));
        assert!(stats.frames_drawn >= 1);
    }
}

/// Culling keeps every pixel over an FFT ocean too: its chunks are bounded by the ocean's strict
/// reach, not the stand-in swell's.
#[test]
fn ring_culling_keeps_the_ocean() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let on = Settings {
        ocean: true,
        ..DEFAULT
    };
    let (culled, stats) = render(&device, &scene, ABOVE, &on);
    let (full, _) = render(
        &device,
        &scene,
        ABOVE,
        &Settings {
            culling: Some(false),
            ..on
        },
    );
    assert!(stats.ocean_bodies == 1 || stats.ocean_size == 0, "{stats:?}");
    assert_eq!(culled, full, "culling changed pixels over the ocean");
    assert!(stats.ring_chunks_culled > 0, "{stats:?}");
}

/// The per-ring snap moves only the far rings: the frame stays deterministic and the near water
/// (the bottom rows, inside the inner square) does not change.
#[test]
fn ring_snap_is_deterministic() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let snap = Settings {
        snap: Some(true),
        ..DEFAULT
    };
    let (a, _) = render(&device, &scene, ABOVE, &snap);
    let (b, _) = render(&device, &scene, ABOVE, &snap);
    assert_eq!(a, b, "the snapped frame is not deterministic");
    let (off, _) = render(&device, &scene, ABOVE, &DEFAULT);
    let row = (W * 4) as usize;
    let near = (H as usize - 8) * row;
    assert_eq!(a[near..], off[near..], "the snap moved the near water");
}

fn dump(label: &str, pixels: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_WATER_FRAME_DUMP") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = std::path::Path::new(&dir).join(format!("extras_{label}.png"));
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

/// Card-only bench (ignored; lavapipe has no timestamps): the water draw's GPU time at 1920x1080
/// on High through `gpu_timing`, per configuration, as the median of 48 timed frames after 16
/// warm frames. Prints one line per configuration for `docs/perf-log`. Run with a pinned card:
/// `cargo test -p regia_terrain --test water_extras_frame -- --ignored --nocapture water_gpu_ms_bench`.
#[test]
#[ignore = "needs a GPU with TIMESTAMP_QUERY; prints numbers, asserts only that they exist"]
fn water_gpu_ms_bench() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let (w, h) = (1920, 1080);
    let configs: [(&str, Camera, Settings); 6] = [
        ("swell", ABOVE, OFF),
        (
            "swell+culling",
            ABOVE,
            Settings {
                culling: Some(true),
                ..OFF
            },
        ),
        (
            "swell+snap",
            ABOVE,
            Settings {
                snap: Some(true),
                ..OFF
            },
        ),
        ("ocean", ABOVE, Settings { ocean: true, ..OFF }),
        (
            "ocean+culling",
            ABOVE,
            Settings {
                ocean: true,
                culling: Some(true),
                ..OFF
            },
        ),
        (
            "underwater",
            UNDER,
            Settings {
                underwater: Some(true),
                level: HIGH_LEVEL,
                ..OFF
            },
        ),
    ];
    for (label, camera, set) in configs {
        let mut renderer = FrameRenderer::new();
        renderer.set_studio_sky(&device, Some(STUDIO_SKY));
        let water = WaterRenderer::new(&device);
        if !water.stats().gpu_ms_supported {
            println!("water_gpu_ms_bench: no TIMESTAMP_QUERY on this device, nothing measured");
            return;
        }
        water.set_tier(WaterTier::High);
        let body = if set.ocean {
            ocean_sea(set.level)
        } else {
            sea(set.level)
        };
        water.set_bodies(&[&body]);
        water.set_ambient_lux(18_000.0);
        water.set_ring_culling(set.culling.unwrap_or(false));
        water.set_underwater(set.underwater.unwrap_or(false));
        water.set_ring_snap(set.snap.unwrap_or(false));
        renderer.set_translucent_layers(vec![water.layer()]);
        let post = post();
        let mut samples = Vec::new();
        for i in 0..64u64 {
            water.set_clock(WaterClock {
                tick: 600 + i,
                ..WaterClock::default()
            });
            water.set_gpu_timing(if i >= 16 { 1 } else { 0 });
            renderer
                .render_offscreen(&device, &frame(&scene, camera, w, h, &post, SUN))
                .expect("frame");
            let stats = water.stats();
            if i >= 16 {
                samples.push(stats.gpu_ms);
            }
        }
        samples.sort_by(f32::total_cmp);
        let stats = water.stats();
        let median = samples[samples.len() / 2];
        println!(
            "water_gpu_ms_bench {label}: median {median:.4} ms, min {:.4}, max {:.4}, {} px, \
             ring tris {}/{} drawn, chunks culled {}/{}, ocean bodies {}, underwater {}",
            samples[0],
            samples[samples.len() - 1],
            stats.gpu_ms_pixels,
            stats.ring_triangles_drawn,
            stats.ring_triangles,
            stats.ring_chunks_culled,
            stats.ring_chunks,
            stats.ocean_bodies,
            stats.underwater
        );
        assert!(median.is_finite() && median > 0.0, "{label}: {samples:?}");
        assert_eq!(stats.gpu_ms_samples, 48);
    }
}
