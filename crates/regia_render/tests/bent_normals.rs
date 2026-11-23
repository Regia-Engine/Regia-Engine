// SPDX-License-Identifier: MPL-2.0
//! Bent normals and the cone specular occlusion (`FrameQuality::bent_normals`, ADR-1375).
//!
//! A polished metal floor in front of a wall, seen from the open side under the IBL sky, with the
//! sun behind the wall. At the wall's foot the floor's mirror direction points into the wall: the
//! AO-based specular occlusion still lets most of the sky reflection through there; the cone form
//! (the specular cone against the visibility cone around the bent normal, which leans away from
//! the wall) takes it out. The open floor beside the wall keeps its reflection, and a profile row
//! without GTAO renders exactly as with bent normals off. No golden: the frames are compared with
//! each other.

use regia_core::glam::{Vec3, Vec4};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderDevice, SsaoMode, box_mesh, translate_mesh,
};

const W: u32 = 320;
const H: u32 = 240;
const EYE: [f32; 3] = [0.0, 1.6, 4.0];
const TARGET: [f32; 3] = [0.0, 0.3, 0.0];
const FOV_Y: f32 = 0.9;

/// The wall: centre and half extents (front face at z = 0, 1 m wide, 3 m tall).
const WALL: ([f32; 3], [f32; 3]) = ([0.0, 1.5, -0.2], [0.5, 1.5, 0.2]);

fn scene() -> LabScene {
    // The wall on a 12 × 12 m polished metal floor.
    let slabs: [([f32; 3], [f32; 3], usize); 2] =
        [([0.0, -0.1, 2.0], [6.0, 0.1, 6.0], 1), (WALL.0, WALL.1, 0)];
    let meshes = slabs
        .into_iter()
        .map(|(c, h, material)| {
            let (v, indices) = box_mesh(h);
            LabMesh {
                vertices: translate_mesh(&v, c),
                indices,
                material,
                ..LabMesh::unskinned()
            }
        })
        .collect();
    let post = PostSettings {
        exposure_ev: 13.5,
        ..PostSettings::golden_deterministic()
    };
    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 13.5,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        // High sun from behind the wall: the floor at its foot is in its shadow, so the indirect
        // light (the sky reflection) is what the frame shows there.
        directional: DirectionalLightUniform::new([0.2, -0.8, 0.55], [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.55, 0.55, 0.55, 1.0],
                roughness: 0.8,
                ..LabMaterial::default()
            },
            // Polished metal: its indirect light is the sky reflection alone.
            LabMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
                metallic: 1.0,
                roughness: 0.2,
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

/// A GTAO row (or another SSAO row) without reflection occlusion, so the specular occlusion is
/// the AO form or the cone form alone.
fn quality(ssao: SsaoMode, bent: bool) -> FrameQuality {
    FrameQuality {
        ssao,
        bent_normals: bent,
        ..FrameQuality::LAB
    }
}

fn render(device: &RenderDevice, quality: FrameQuality) -> Vec<u8> {
    let lab = scene();
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(device, &frame).expect("frame").pixels
}

/// The floor point pixel `(x, y)` shows, or `None` when the wall (or the sky) is in front of it.
fn floor_point(x: u32, y: u32) -> Option<Vec3> {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(EYE),
        Vec3::from_array(TARGET),
        Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV_Y, W as f32 / H as f32, 0.1, 200.0);
    let inv = (proj * view).inverse();
    let ndc = (
        (x as f32 + 0.5) / W as f32 * 2.0 - 1.0,
        1.0 - (y as f32 + 0.5) / H as f32 * 2.0,
    );
    let far = inv * Vec4::new(ndc.0, ndc.1, 1.0, 1.0);
    let eye = Vec3::from_array(EYE);
    let dir = (far.truncate() / far.w - eye).normalize();
    if dir.y >= 0.0 {
        return None;
    }
    let t_floor = -eye.y / dir.y;
    // Slab test against the wall box: a hit before the floor hides it.
    let (c, h) = (Vec3::from_array(WALL.0), Vec3::from_array(WALL.1));
    let (lo, hi) = (c - h, c + h);
    let (mut t0, mut t1) = (0.0f32, f32::INFINITY);
    let axes = dir
        .to_array()
        .into_iter()
        .zip(eye.to_array())
        .zip(lo.to_array().into_iter().zip(hi.to_array()));
    for ((d, e), (l, h)) in axes {
        if d.abs() < 1e-9 {
            if e < l || e > h {
                t1 = -1.0;
            }
            continue;
        }
        let (a, b) = ((l - e) / d, (h - e) / d);
        t0 = t0.max(a.min(b));
        t1 = t1.min(a.max(b));
    }
    if t0 <= t1 && t0 < t_floor {
        return None;
    }
    Some(eye + dir * t_floor)
}

fn luma(px: &[u8], x: u32, y: u32) -> f64 {
    let i = ((y * W + x) * 4) as usize;
    0.2126 * f64::from(px[i]) + 0.7152 * f64::from(px[i + 1]) + 0.0722 * f64::from(px[i + 2])
}

/// Over the pixels whose floor point passes `pick`: the mean luma of `a` and of `b`, the largest
/// per-pixel difference, and the count.
fn floor_region(a: &[u8], b: &[u8], pick: impl Fn(Vec3) -> bool) -> (f64, f64, f64, usize) {
    let (mut sa, mut sb, mut worst, mut n) = (0.0, 0.0, 0.0f64, 0usize);
    for y in 0..H {
        for x in 0..W {
            if floor_point(x, y).is_some_and(&pick) {
                let (la, lb) = (luma(a, x, y), luma(b, x, y));
                sa += la;
                sb += lb;
                worst = worst.max((la - lb).abs());
                n += 1;
            }
        }
    }
    let k = n.max(1) as f64;
    (sa / k, sb / k, worst, n)
}

/// `REGIA_TEST_DUMP=<dir>` writes the frames there (to look at them).
fn dump(name: &str, px: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_TEST_DUMP") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    let written = std::fs::File::create(&path)
        .map_err(|e| e.to_string())
        .and_then(|file| {
            let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
            writer.write_image_data(px).map_err(|e| e.to_string())
        });
    eprintln!("dump {}: {written:?}", path.display());
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_polished_floor_at_a_walls_foot_stops_reflecting_the_sky_and_the_open_floor_keeps_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device");
    let off = render(&device, quality(SsaoMode::GtaoHalfRes, false));
    let on = render(&device, quality(SsaoMode::GtaoHalfRes, true));
    assert!(off != on, "bent normals changed nothing");
    assert_eq!(
        on,
        render(&device, quality(SsaoMode::GtaoHalfRes, true)),
        "bent normals are not deterministic"
    );
    dump("bent_off", &off);
    dump("bent_on", &on);

    // The floor's strip along the wall's foot, in the wall's shadow: its mirror direction points
    // into the wall.
    let (foot_on, foot_off, _, n) =
        floor_region(&on, &off, |p| p.x.abs() <= 0.4 && (0.03..=0.3).contains(&p.z));
    eprintln!("wall foot ({n} px): luma {foot_off:.1} -> {foot_on:.1}");
    assert!(n > 50, "the wall's foot is not in view ({n} px)");
    assert!(
        foot_on < foot_off * 0.9,
        "the floor at the wall's foot must lose sky reflection: luma {foot_off:.1} -> {foot_on:.1}"
    );

    // Open floor, more than 1.5 m from the wall (past the SSAO kernel's 1.2 m reach): unchanged.
    let (open_on, open_off, worst, n) = floor_region(&on, &off, |p| {
        // Distance to the wall's footprint (x in ±0.5, z in −0.4..0).
        let dx = (p.x.abs() - WALL.1[0]).max(0.0);
        let dz = (p.z - 0.0).max(-0.4 - p.z).max(0.0);
        (dx * dx + dz * dz).sqrt() > 1.5
    });
    eprintln!("open floor ({n} px): luma {open_off:.1} -> {open_on:.1}, worst pixel {worst:.1}");
    assert!(n > 2_000, "the open floor is not in view ({n} px)");
    assert!(
        (open_on - open_off).abs() <= 0.5 && worst <= 2.0,
        "the open floor must keep its reflection: luma {open_off:.1} -> {open_on:.1}, worst pixel {worst:.1}"
    );
}

/// The frame's last columns and rows hold no neighbour on their far side in the depth buffer: their
/// AO texels take no occlusion by coordinate, not by a difference that happens to round to zero
/// (ADR-1626: on the RTX 3080 Ti it did not, and the right edge carried dark fringes of random
/// lean). Away from the wall the bent normals change nothing there, whichever card renders.
#[test]
fn the_frames_far_edges_carry_no_bent_normal_fringe() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device");
    let off = render(&device, quality(SsaoMode::GtaoHalfRes, false));
    let on = render(&device, quality(SsaoMode::GtaoHalfRes, true));
    let (mut worst, mut n) = (0.0f64, 0usize);
    for y in 0..H {
        for x in 0..W {
            let on_edge = x >= W - 4 || y >= H - 4;
            let far_from_wall = floor_point(x, y).is_some_and(|p| p.x.abs() > 1.5);
            if on_edge && far_from_wall {
                worst = worst.max((luma(&on, x, y) - luma(&off, x, y)).abs());
                n += 1;
            }
        }
    }
    assert!(n > 20, "no floor at the frame's far edges ({n} px)");
    assert!(
        worst <= 2.0,
        "a bent-normal fringe at the far edges: worst pixel {worst:.1} over {n} px"
    );
}

#[test]
fn bent_normals_need_a_gtao_row() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The half-res AO row (Deck) has no bent normals: turning them on changes nothing.
    let device = RenderDevice::headless().expect("headless device");
    let off = render(&device, quality(SsaoMode::HalfRes, false));
    let on = render(&device, quality(SsaoMode::HalfRes, true));
    assert!(off == on, "bent normals must stay inactive without a GTAO row");
}

/// A courtyard for the bench: walls, crates and a raised hull on a floor, so most of the frame
/// has occluders in the SSAO kernel's reach.
fn courtyard() -> LabScene {
    let mut slabs: Vec<([f32; 3], [f32; 3], usize)> = vec![
        ([0.0, -0.1, 0.0], [30.0, 0.1, 30.0], 0),
        // Walls on three sides.
        ([0.0, 2.0, -12.0], [12.0, 2.0, 0.3], 0),
        ([-12.0, 2.0, 0.0], [0.3, 2.0, 12.0], 0),
        ([12.0, 2.0, 0.0], [0.3, 2.0, 12.0], 0),
        // A hull on four legs: the ground under it.
        ([0.0, 1.6, -4.0], [3.0, 0.6, 1.5], 1),
    ];
    for (x, z) in [(-2.6, -5.2), (2.6, -5.2), (-2.6, -2.8), (2.6, -2.8)] {
        slabs.push(([x, 0.5, z], [0.15, 0.5, 0.15], 1));
    }
    for i in 0..8 {
        let x = -9.0 + 2.5 * i as f32;
        slabs.push(([x, 0.5, -10.5], [0.5, 0.5, 0.5], 0));
        slabs.push(([-10.5, 0.4, -8.0 + 2.2 * i as f32], [0.4, 0.4, 0.4], 0));
    }
    let meshes = slabs
        .into_iter()
        .map(|(c, h, material)| {
            let (v, indices) = box_mesh(h);
            LabMesh {
                vertices: translate_mesh(&v, c),
                indices,
                material,
                ..LabMesh::unskinned()
            }
        })
        .collect();
    LabScene {
        width: 2560,
        height: 1440,
        eye: [4.0, 1.7, 6.0],
        target: [-2.0, 0.8, -6.0],
        fov_y: 0.9,
        near: 0.1,
        far: 300.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 13.5,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            auto_exposure: false,
            exposure_ev: 13.5,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.3, -0.7, 0.5], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.5, 0.48, 0.45, 1.0],
                roughness: 0.7,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.7, 0.7, 0.72, 1.0],
                metallic: 1.0,
                roughness: 0.35,
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

/// GPU cost of bent normals at 2560 × 1440 on Desktop High and Ultra: the courtyard with the
/// preset's own quality (DDGI on), bent normals off and on in alternating rounds (three each),
/// `BENT_BENCH_FRAMES` (default 240) timed frames after 60 warm-up frames per run, the camera
/// swaying so the static-screen reuse never skips the SSAO. Prints the SSAO, forward, scene and
/// frame means and their differences. Real GPUs only: `--ignored`.
#[test]
#[ignore = "GPU timing on a real GPU: REGIA_GPU_INDEX=<n> cargo test --release -p regia_render --test bent_normals -- --ignored bench --nocapture"]
fn bench_bent_normals_at_1440p() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use regia_render::{GpuTimings, RenderProfile, RenderProfilePreset};
    let dev = RenderDevice::headless().expect("headless device");
    assert!(dev.capabilities.timestamp_query, "timestamps needed");
    let frames: u32 = regia_core::brand::env_var("BENT_BENCH_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240);
    eprintln!("bench adapter: {}", dev.capabilities.adapter_name);
    for preset in [
        RenderProfilePreset::DesktopHigh,
        RenderProfilePreset::DesktopUltra,
    ] {
        let base = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
        // [ssao, forward, scene, frame] sums and frame counts, off then on.
        let mut sums = [[0.0f64; 4]; 2];
        let mut counts = [0.0f64; 2];
        for round in 0..3 {
            for (k, bent) in [false, true].into_iter().enumerate() {
                let q = FrameQuality {
                    bent_normals: bent,
                    ..base
                };
                let mut lab = courtyard();
                let mut renderer = FrameRenderer::new();
                renderer.set_gpu_timing(&dev, true);
                let eye = lab.eye;
                let mut run = GpuTimings::default();
                let mut n = 0.0f64;
                for i in 0..(60 + frames) {
                    let t = i as f32 * 0.02;
                    lab.eye = [eye[0] + 0.6 * t.sin(), eye[1] + 0.1 * (1.3 * t).sin(), eye[2]];
                    let frame = lab.as_frame_scene_with_quality(q);
                    renderer
                        .begin_offscreen(&dev, &frame, wgpu::TextureFormat::Rgba8Unorm)
                        .expect("frame");
                    if i % 4 == 3 {
                        let _ = dev.device.poll(wgpu::PollType::wait_indefinitely());
                    } else {
                        let _ = dev.device.poll(wgpu::PollType::Poll);
                    }
                    if i >= 60 {
                        let g = renderer.gpu_timings();
                        run.ssao_ms += g.ssao_ms;
                        run.forward_ms += g.forward_ms;
                        run.scene_ms += g.scene_ms;
                        run.frame_ms += g.frame_ms;
                        n += 1.0;
                    }
                }
                let row = [run.ssao_ms, run.forward_ms, run.scene_ms, run.frame_ms];
                for (sum, v) in sums[k].iter_mut().zip(row) {
                    *sum += f64::from(v);
                }
                counts[k] += n;
                eprintln!(
                    "bench {preset:?} round {round} bent {bent}: ssao {:.3} ms, forward {:.3}, scene {:.3}, frame {:.3}",
                    f64::from(row[0]) / n,
                    f64::from(row[1]) / n,
                    f64::from(row[2]) / n,
                    f64::from(row[3]) / n,
                );
            }
        }
        let mean = |k: usize, j: usize| sums[k][j] / counts[k].max(1.0);
        eprintln!(
            "bench {preset:?} mean off → on: ssao {:.3} → {:.3} ms ({:+.3}), forward {:.3} → {:.3} ({:+.3}), scene {:.3} → {:.3} ({:+.3}), frame {:.3} → {:.3} ({:+.3})",
            mean(0, 0),
            mean(1, 0),
            mean(1, 0) - mean(0, 0),
            mean(0, 1),
            mean(1, 1),
            mean(1, 1) - mean(0, 1),
            mean(0, 2),
            mean(1, 2),
            mean(1, 2) - mean(0, 2),
            mean(0, 3),
            mean(1, 3),
            mean(1, 3) - mean(0, 3),
        );
    }
}

/// The courtyard's shaded, crate-lined corner at 1280 × 720 with Desktop High's own quality (DDGI
/// on), bent normals off and on, after 90 frames so the probes and the temporal resolve settle:
/// for judging the look by eye
/// (`REGIA_TEST_DUMP=<dir>` writes `courtyard_off.png` / `courtyard_on.png`). Prints the mean
/// luma of both. Real GPUs only (DDGI): `--ignored`.
#[test]
#[ignore = "look check on a real GPU: REGIA_GPU_INDEX=<n> REGIA_TEST_DUMP=<dir> cargo test --release -p regia_render --test bent_normals -- --ignored look --nocapture"]
fn look_courtyard_bent_normals_off_and_on() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use regia_render::{RenderProfile, RenderProfilePreset};
    let dev = RenderDevice::headless().expect("headless device");
    let base = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let (w, h) = (1280u32, 720u32);
    let mut frames = Vec::new();
    for bent in [false, true] {
        let q = FrameQuality {
            bent_normals: bent,
            ..base
        };
        // The courtyard's crate-lined corner in the walls' shadow: the sun behind the far walls,
        // a darker floor, the camera close.
        let mut lab = courtyard();
        lab.width = w;
        lab.height = h;
        lab.eye = [-6.5, 1.4, -4.5];
        lab.target = [-11.0, 0.4, -10.5];
        lab.exposure_ev = 12.0;
        lab.post = Some(PostSettings {
            auto_exposure: false,
            exposure_ev: 12.0,
            ..PostSettings::default()
        });
        lab.directional = DirectionalLightUniform::new([0.45, -0.55, 0.7], [1.0, 0.97, 0.92], 100_000.0);
        lab.materials[0].base_color = [0.3, 0.29, 0.27, 1.0];
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_enabled(Some(true));
        for _ in 0..90 {
            let frame = lab.as_frame_scene_with_quality(q);
            renderer
                .begin_offscreen(&dev, &frame, wgpu::TextureFormat::Rgba8Unorm)
                .expect("frame");
            let _ = dev.device.poll(wgpu::PollType::wait_indefinitely());
        }
        let frame = lab.as_frame_scene_with_quality(q);
        let px = renderer.render_offscreen(&dev, &frame).expect("frame").pixels;
        let mean = px
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
            .sum::<f64>()
            / f64::from(w * h);
        eprintln!("courtyard bent {bent}: mean luma {mean:.2}");
        frames.push(px);
    }
    let Ok(dir) = regia_core::brand::env_var("REGIA_TEST_DUMP") else {
        return;
    };
    for (name, px) in ["courtyard_off", "courtyard_on"].into_iter().zip(&frames) {
        let path = std::path::Path::new(&dir).join(format!("{name}.png"));
        let written = std::fs::File::create(&path)
            .map_err(|e| e.to_string())
            .and_then(|file| {
                let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
                writer.write_image_data(px).map_err(|e| e.to_string())
            });
        eprintln!("dump {}: {written:?}", path.display());
    }
}
