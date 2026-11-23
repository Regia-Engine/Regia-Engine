// SPDX-License-Identifier: MPL-2.0
//! Several baked GI volumes on the GPU (ADR-1097): the screen pass's blend
//! (`baked_gi_set.wgsl`, `static_gi_set`) read back over a floor that crosses the seam between
//! two volumes, against its CPU mirror (`sample_static_gi_blend`).
//!
//! - Two tiles, one lit three times the other, cut one probe column apart: every floor texel
//!   matches the CPU blend, the coverage stays full across the seam, and along each texel row
//!   the light changes by no more than the seam cell's linear ramp allows.
//! - One volume set as a full-weight layer reads exactly as the same volume installed alone
//!   (the single-volume path, bit for bit), and a half-weight layer halves the coverage.
//! - A higher-priority volume nested inside the first lights its inside alone.
//!
//! Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI); skips without one.
//! No golden image: every check is against the CPU mirror or between two GPU readbacks.

use std::sync::Arc;

use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiVolume, ProbeData, ProbeFlags, ProbeMeta, SH_Y00, TerrainLayer, VolumeLayout,
};
use regia_core::glam::Mat4;
use regia_render::baked_gi::{BakedGiLayer, sample_static_gi_blend};
use regia_render::{
    BakedGiFrameLights, BakedGiSettings, DdgiScreenTexel, DirectionalLightUniform, FrameQuality,
    FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, box_mesh,
};

const W: u32 = 160;
const H: u32 = 96;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip baked_gi_seams: {err}");
            None
        }
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q
}

/// A flat floor (top at y = 0) seen from above and to the side, the view centred on x = −2.
fn floor_scene() -> LabScene {
    let (vertices, indices) = box_mesh([40.0, 0.25, 20.0]);
    let floor = LabMesh {
        vertices,
        indices,
        material: 0,
        model: Mat4::from_translation([0.0, -0.25, 0.0].into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    };
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 13.5;
    LabScene {
        width: W,
        height: H,
        eye: [-2.0, 7.0, 9.0],
        target: [-2.0, 0.0, 0.0],
        fov_y: 1.0,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 13.5,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.3, -0.8, -0.5], [1.0, 0.97, 0.92], 50_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.45, 0.45, 0.45, 1.0],
            roughness: 0.95,
            ..LabMaterial::default()
        }],
        meshes: vec![floor],
    }
}

/// A flat terrain tile on the floor (4 m columns, levels 0.5, 1.5, 3 m) with column (0, 0) at
/// `(x0, −8)` and `columns` × 5 columns, lit by a uniform sky radiance `sky`.
fn tile(x0: f32, columns: u32, sky: f32) -> Arc<BakedGiVolume> {
    tile_at([x0, -8.0], [columns, 5], sky)
}

/// [`tile`] with column (0, 0) at `origin` (x, z) and `counts` columns.
fn tile_at(origin: [f32; 2], counts: [u32; 2], sky: f32) -> Arc<BakedGiVolume> {
    let layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin,
            spacing: [4.0, 4.0],
            counts,
            levels: vec![0.5, 1.5, 3.0],
            ground: vec![0.0; (counts[0] * counts[1]) as usize],
        }),
        bricks: Vec::new(),
        moment_resolution: 4,
        max_distance: 16.0,
    };
    let k = 4.0 * std::f32::consts::PI * SH_Y00;
    let lit = [
        [sky * k, 0.0, 0.0, 0.0],
        [0.9 * sky * k, 0.0, 0.0, 0.0],
        [0.8 * sky * k, 0.0, 0.0, 0.0],
    ];
    let dark = [[0.0f32; 4]; 3];
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|_| ProbeData {
            sh: [dark, lit, dark, dark],
            moments: vec![[16.0, 256.0]; 16],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags: ProbeFlags::VALID,
            },
        })
        .collect();
    let lights = BakeLights {
        sky_scale: 1.0,
        ..BakeLights::default()
    };
    Arc::new(BakedGiVolume::from_probes(&layout, lights, BakeInfo::default(), &probes).expect("tile"))
}

fn frame_lights(scene: &LabScene) -> BakedGiFrameLights {
    let d = scene.directional.direction;
    let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    BakedGiFrameLights {
        sun: Some(scene.directional.color.map(|c| c * scene.directional.illuminance)),
        to_sun: Some([-d[0] / l, -d[1] / l, -d[2] / l]),
        fill: None,
    }
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn normalize(a: [f32; 3]) -> [f32; 3] {
    let l = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt().max(1e-12);
    [a[0] / l, a[1] / l, a[2] / l]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Where the view ray through full-resolution pixel `px` (its centre) meets the floor y = 0.
fn floor_hit(scene: &LabScene, px: [f32; 2]) -> Option<[f32; 3]> {
    let eye = scene.eye;
    let fwd = normalize(sub(scene.target, eye));
    let right = normalize(cross(fwd, [0.0, 1.0, 0.0]));
    let up = cross(right, fwd);
    let aspect = scene.width as f32 / scene.height as f32;
    let fov_y = if quality().horizontal_plus {
        regia_render::output::horizontal_plus_fov_y(scene.fov_y, aspect)
    } else {
        scene.fov_y
    };
    let t = (fov_y * 0.5).tan();
    let ndc = [
        (px[0] + 0.5) / scene.width as f32 * 2.0 - 1.0,
        1.0 - (px[1] + 0.5) / scene.height as f32 * 2.0,
    ];
    let dir = normalize(std::array::from_fn(|a| {
        fwd[a] + right[a] * ndc[0] * t * aspect + up[a] * ndc[1] * t
    }));
    (dir[1] < -1e-3).then(|| {
        let s = -eye[1] / dir[1];
        std::array::from_fn(|a| eye[a] + dir[a] * s)
    })
}

/// Renders two frames and reads the screen lookup back: half width, height, texels.
fn lookup(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    scene: &LabScene,
) -> (u32, u32, Vec<DdgiScreenTexel>) {
    for _ in 0..2 {
        renderer
            .render_offscreen(device, &scene.as_frame_scene_with_quality(quality()))
            .expect("render");
    }
    let status = renderer.baked_gi_status();
    assert!(status.active, "the volumes lit the frame: {status:?}");
    renderer
        .baked_gi_read_back(device)
        .expect("read back")
        .expect("the screen lookup ran")
}

/// Floor texels the test can place: in view, within 14 m, inside the tiles' z range.
fn floor_texels(scene: &LabScene, hw: u32, hh: u32) -> Vec<(u32, u32, [f32; 3])> {
    let mut out = Vec::new();
    for ty in 1..hh - 1 {
        for tx in 1..hw - 1 {
            let Some(p) = floor_hit(scene, [(tx * 2) as f32, (ty * 2) as f32]) else {
                continue;
            };
            let d = sub(p, scene.eye);
            if (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() > 14.0 || p[2].abs() > 7.0 {
                continue;
            }
            out.push((tx, ty, p));
        }
    }
    out
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_gpu_blend_matches_its_cpu_mirror_across_a_seam() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let scene = floor_scene();
    // x = −16..−4 and 0..12: the seam cell is x = −4..0, in the middle of the view.
    let a = tile(-16.0, 4, 2_000.0);
    let b = tile(0.0, 4, 6_000.0);
    let layers = [BakedGiLayer::new(b.clone())];
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(a.clone()));
    assert_eq!(renderer.set_baked_gi_layers(&layers), 2);
    let (hw, hh, texels) = lookup(&device, &mut renderer, &scene);
    assert_eq!(renderer.baked_gi_status().volumes, 2);
    assert_eq!(renderer.baked_gi_status().volumes_dropped, 0);

    let settings = BakedGiSettings::default();
    let lights = frame_lights(&scene);
    let all = [BakedGiLayer::new(a), BakedGiLayer::new(b)];
    let up = [0.0, 1.0, 0.0];
    let cpu_at =
        |p: [f32; 3]| sample_static_gi_blend(&all, &settings, &lights, p, up, normalize(sub(scene.eye, p)));
    let mut checked = 0;
    let mut in_seam = 0;
    for (tx, ty, p) in floor_texels(&scene, hw, hh) {
        let texel = &texels[(ty * hw + tx) as usize];
        let cpu = cpu_at(p);
        assert!(
            (texel.coverage - cpu.coverage).abs() < 0.02,
            "coverage at {p:?}: gpu {} cpu {}",
            texel.coverage,
            cpu.coverage
        );
        // Inside both tiles' reach the blend covers fully, the seam cell included.
        if p[0] > -15.0 && p[0] < 11.0 {
            assert!(texel.coverage > 0.99, "a hole at {p:?}: {}", texel.coverage);
        }
        // The GPU light lies within the CPU light over the texel's ±½-pixel footprint (1 %
        // margin for RGB9E5 and the depth reconstruction).
        let gpu = texel.at(up);
        let (mut lo, mut hi) = (cpu.irradiance_over_pi, cpu.irradiance_over_pi);
        for (dx, dy) in [(-0.5, -0.5), (0.5, -0.5), (-0.5, 0.5), (0.5, 0.5)] {
            let Some(q) = floor_hit(&scene, [(tx * 2) as f32 + dx, (ty * 2) as f32 + dy]) else {
                continue;
            };
            let e = cpu_at(q).irradiance_over_pi;
            for c in 0..3 {
                lo[c] = lo[c].min(e[c]);
                hi[c] = hi[c].max(e[c]);
            }
        }
        for c in 0..3 {
            assert!(
                gpu[c] >= lo[c] * 0.99 - 1.0 && gpu[c] <= hi[c] * 1.01 + 1.0,
                "texel ({tx}, {ty}) at {p:?}: gpu {gpu:?} outside the CPU range {lo:?}..{hi:?}"
            );
        }
        checked += 1;
        in_seam += usize::from(p[0] > -4.0 && p[0] < 0.0);
    }
    eprintln!("baked GI seam: {checked} floor texels match the CPU blend, {in_seam} in the seam cell");
    assert!(checked >= 200, "too few floor texels ({checked})");
    assert!(in_seam >= 20, "too few texels in the seam cell ({in_seam})");

    // Along each texel row the light moves at most as fast as the seam cell's ramp: 4000 (the
    // difference) over 4 m, times the floor distance between neighboring texels, with 25 %
    // margin; a step at the seam would be the whole difference.
    let mut rows = 0;
    for ty in 1..hh - 1 {
        let mut prev: Option<([f32; 3], f32)> = None;
        for tx in 1..hw - 1 {
            let Some(p) = floor_hit(&scene, [(tx * 2) as f32, (ty * 2) as f32]) else {
                prev = None;
                continue;
            };
            let d = sub(p, scene.eye);
            if (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() > 14.0 || p[2].abs() > 7.0 {
                prev = None;
                continue;
            }
            let e = texels[(ty * hw + tx) as usize].at(up)[0];
            if let Some((q, f)) = prev {
                let dx = (p[0] - q[0]).abs();
                assert!(
                    (e - f).abs() <= 1.25 * 1_000.0 * dx + 20.0,
                    "a step of {} between texels {} m apart at {p:?}",
                    (e - f).abs(),
                    dx
                );
            }
            prev = Some((p, e));
        }
        rows += 1;
    }
    assert!(rows > 10);
}

#[test]
fn one_full_weight_layer_reads_as_the_volume_installed_alone() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let scene = floor_scene();
    let v = tile(-16.0, 8, 3_000.0);
    let mut installed = FrameRenderer::new();
    installed.set_baked_gi(Some(v.clone()));
    let (hw, hh, alone) = lookup(&device, &mut installed, &scene);
    let mut layered = FrameRenderer::new();
    layered.set_baked_gi_layers(&[BakedGiLayer::new(v.clone())]);
    let (lw, lh, as_layer) = lookup(&device, &mut layered, &scene);
    assert_eq!((hw, hh), (lw, lh));
    let bits = |t: &DdgiScreenTexel| -> Vec<u32> {
        t.e.iter()
            .flatten()
            .chain(std::iter::once(&t.coverage))
            .map(|x| x.to_bits())
            .collect()
    };
    assert!(
        alone.iter().zip(&as_layer).all(|(a, b)| bits(a) == bits(b)),
        "the single-volume path is unchanged"
    );
    // At half weight the light stays and the coverage halves.
    let mut half = FrameRenderer::new();
    half.set_baked_gi_layers(&[BakedGiLayer {
        weight: 0.5,
        ..BakedGiLayer::new(v)
    }]);
    let (_, _, halved) = lookup(&device, &mut half, &scene);
    let mut compared = 0;
    for (tx, ty, _) in floor_texels(&scene, hw, hh) {
        let (a, h) = (&alone[(ty * hw + tx) as usize], &halved[(ty * hw + tx) as usize]);
        if a.coverage < 0.99 {
            continue;
        }
        assert!((h.coverage - 0.5).abs() < 1e-3, "{}", h.coverage);
        let (ea, eh) = (a.at([0.0, 1.0, 0.0]), h.at([0.0, 1.0, 0.0]));
        for c in 0..3 {
            assert!((ea[c] - eh[c]).abs() <= 0.01 * ea[c] + 1.0, "{ea:?} {eh:?}");
        }
        compared += 1;
    }
    assert!(compared >= 200, "{compared}");
}

#[test]
fn a_nested_higher_priority_volume_lights_its_inside_alone_on_the_gpu() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else {
        return;
    };
    let scene = floor_scene();
    let world = tile(-24.0, 12, 1_000.0); // x = −24..20
    let set_piece = tile(-8.0, 3, 5_000.0); // x = −8..0
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(world.clone()));
    let nested = BakedGiLayer {
        priority: 1,
        blend_distance: 2.0,
        ..BakedGiLayer::new(set_piece.clone())
    };
    renderer.set_baked_gi_layers(std::slice::from_ref(&nested));
    let (hw, hh, texels) = lookup(&device, &mut renderer, &scene);
    let up = [0.0, 1.0, 0.0];
    let (mut inside, mut outside) = (0, 0);
    for (tx, ty, p) in floor_texels(&scene, hw, hh) {
        let e = texels[(ty * hw + tx) as usize].at(up)[0];
        // Two meters or more inside the set piece's footprint (x −8..0, z −8..8): its light.
        if p[0] > -6.0 && p[0] < -2.0 && p[2].abs() < 6.0 {
            assert!((e - 5_000.0).abs() < 0.02 * 5_000.0, "inside at {p:?}: {e}");
            inside += 1;
        }
        // Outside it: the world's.
        if p[0] < -8.5 || p[0] > 0.5 {
            assert!((e - 1_000.0).abs() < 0.02 * 1_000.0, "outside at {p:?}: {e}");
            outside += 1;
        }
    }
    assert!(
        inside >= 20 && outside >= 50,
        "{inside} inside, {outside} outside"
    );
}

/// GPU time of the baked GI screen pass (`gpu_timings().gi_ms`) and the frame at 1440p on Desktop
/// High: one volume over the floor, the same area as four tiles cut one column apart, and four
/// tiles overlapping by 8 m with an 8 m blend distance. Prints the numbers; asserts nothing about
/// them. Real GPUs only: `--ignored`.
#[test]
#[ignore = "GPU timing on a real GPU: cargo test --release -p regia_render --test baked_gi_seams -- --ignored bench"]
fn bench_blended_volumes_at_1440p() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(dev) = device() else {
        return;
    };
    assert!(dev.capabilities.timestamp_query, "timestamps needed");
    let frames: u32 = regia_core::brand::env_var("BAKED_GI_BENCH_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240);
    eprintln!("bench adapter: {}", dev.capabilities.adapter_name);
    // The floor seen from the scene camera spans about x −14..10, z −6..8: everything below
    // covers x −24..24, z −16..16.
    let one = vec![tile_at([-24.0, -16.0], [13, 9], 2_000.0)];
    let abutting = vec![
        tile_at([-24.0, -16.0], [6, 4], 2_000.0),
        tile_at([0.0, -16.0], [7, 4], 2_500.0),
        tile_at([-24.0, 0.0], [6, 5], 3_000.0),
        tile_at([0.0, 0.0], [7, 5], 3_500.0),
    ];
    let overlapping = vec![
        tile_at([-24.0, -16.0], [9, 7], 2_000.0),
        tile_at([-8.0, -16.0], [9, 7], 2_500.0),
        tile_at([-24.0, -8.0], [9, 7], 3_000.0),
        tile_at([-8.0, -8.0], [9, 7], 3_500.0),
    ];
    let mut results = Vec::new();
    for (name, volumes, blend) in [
        ("one volume", &one, 0.0),
        ("4 abutting tiles", &abutting, 0.0),
        ("4 tiles, 8 m overlap", &overlapping, 8.0),
    ] {
        let mut scene = floor_scene();
        scene.width = 2560;
        scene.height = 1440;
        scene.post = Some(PostSettings {
            auto_exposure: false,
            exposure_ev: 13.5,
            ..PostSettings::default()
        });
        let mut renderer = FrameRenderer::new();
        renderer.set_gpu_timing(&dev, true);
        let layers: Vec<BakedGiLayer> = volumes
            .iter()
            .map(|v| BakedGiLayer {
                blend_distance: blend,
                ..BakedGiLayer::new(v.clone())
            })
            .collect();
        renderer.set_baked_gi_layers(&layers);
        let q = quality();
        let (mut gi, mut frame, mut n) = (0.0f32, 0.0f32, 0.0f32);
        let base = scene.eye;
        for i in 0..(120 + frames) {
            let t = i as f32 * 0.02;
            scene.eye = [base[0] + 0.6 * t.sin(), base[1] + 0.1 * (1.3 * t).sin(), base[2]];
            let fs = scene.as_frame_scene_with_quality(q);
            renderer
                .begin_offscreen(&dev, &fs, wgpu::TextureFormat::Rgba8Unorm)
                .expect("frame");
            if i % 4 == 3 {
                let _ = dev.device.poll(wgpu::PollType::wait_indefinitely());
            } else {
                let _ = dev.device.poll(wgpu::PollType::Poll);
            }
            if i >= 120 {
                let g = renderer.gpu_timings();
                gi += g.gi_ms;
                frame += g.frame_ms;
                n += 1.0;
            }
        }
        let status = renderer.baked_gi_status();
        assert!(status.active && status.volumes == volumes.len(), "{status:?}");
        let avg = |v: f32| v / n.max(1.0);
        eprintln!(
            "bench {name}: gi (baked lookup) {:.3} ms, frame {:.3} ms",
            avg(gi),
            avg(frame)
        );
        results.push(avg(gi));
    }
    eprintln!(
        "bench: 4 abutting tiles − one volume = {:+.3} ms; 4 overlapping − one = {:+.3} ms",
        results[1] - results[0],
        results[2] - results[0]
    );
}
