// SPDX-License-Identifier: MPL-2.0
//! Split baked GI volumes on the GPU (ADR-2618): a small layout forced to split by a lowered cap
//! (`BakedGiVolume::split_parts`' test-only parameter) lights a frame across the split boundary
//! as the unsplit volume does, on the CPU mirror and in the screen pass; a minimum-spec device
//! (128 MiB storage bindings) installs every part of a volume larger than one binding; the
//! brick-loop probe reads back a histogram of the bricks each texel tested.
//!
//! Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI); skips without one.
//! No golden image: every check is between two readbacks or against the CPU mirror.

use std::sync::Arc;

use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiVolume, DetailBrick, GI_COMPONENTS, META_RECORD_BYTES, MOMENT_TEXEL_BYTES,
    ProbeData, ProbeFlags, ProbeMeta, SH_RECORD_BYTES, SH_Y00, TerrainLayer, VolumeLayout,
};
use regia_core::glam::Mat4;
use regia_render::baked_gi::{BakedGiLayer, sample_static_gi_blend, split_fades};
use regia_render::minspec::MinspecProfile;
use regia_render::{
    BakedGiFrameLights, BakedGiSettings, DdgiScreenTexel, DirectionalLightUniform, FrameQuality,
    FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, box_mesh,
};

const W: u32 = 160;
const H: u32 = 96;

/// Serialises device creation across this binary's tests (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q
}

/// A flat floor (top at y = 0) seen from above, the view centred on the split band (x ≈ −1).
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
        eye: [-1.0, 7.0, 9.0],
        target: [-1.0, 0.0, 0.0],
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

fn uniform_sh(l: f32) -> [[f32; 4]; 3] {
    let k = 4.0 * std::f32::consts::PI * SH_Y00;
    [
        [l * k, 0.0, 0.0, 0.0],
        [0.9 * l * k, 0.0, 0.0, 0.0],
        [0.8 * l * k, 0.0, 0.0, 0.0],
    ]
}

/// A terrain layer of 30 × 9 two-metre columns (x = −30..28, z = −8..8) lit by a sky that rises
/// along X, and a long detail brick (x = −26..26, z = −1..1) three times as bright: no cut can
/// avoid it, so it goes whole into both parts and the band runs through it.
fn small_volume() -> BakedGiVolume {
    let terrain = TerrainLayer {
        origin: [-30.0, -8.0],
        spacing: [2.0, 2.0],
        counts: [30, 9],
        levels: vec![0.5, 1.5, 3.0],
        ground: vec![0.0; 270],
    };
    let mut layout = VolumeLayout {
        terrain: Some(terrain.clone()),
        bricks: vec![DetailBrick {
            origin: [-26.0, -0.5, -1.0],
            spacing: [1.0, 1.0, 1.0],
            counts: [53, 3, 3],
            first_probe: 0,
            flags: 0,
        }],
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    let nominal = layout.nominal_positions();
    let dark = [[0.0f32; 4]; 3];
    let probes: Vec<ProbeData> = nominal
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let sky = 1_500.0 + 120.0 * (p[0] + 30.0);
            let sky = if i >= terrain.probe_count() {
                3.0 * sky
            } else {
                sky
            };
            ProbeData {
                sh: [dark, uniform_sh(sky), dark, dark],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            }
        })
        .collect();
    let lights = BakeLights {
        sky_scale: 1.0,
        ..BakeLights::default()
    };
    BakedGiVolume::from_probes(&layout, lights, BakeInfo::default(), &probes).expect("volume")
}

/// The small volume split in two by a cap of 75 % of its size (the test-only parameter; each
/// half holds the brick across the cut and four shared columns).
fn split_small(whole: &BakedGiVolume) -> Vec<Arc<BakedGiVolume>> {
    let cap = whole.layout().runtime_buffer_bytes() * 3 / 4;
    let parts = whole.split_parts(cap).expect("splits");
    assert_eq!(parts.len(), 2, "two parts along X");
    assert!(
        parts.iter().all(|p| p.bricks.len() == 1),
        "the brick across the cut is in both parts"
    );
    parts.into_iter().map(Arc::new).collect()
}

/// The CPU mirror: across the band (and everywhere else on the floor and above it) the blend of
/// the two parts reads as the unsplit volume, within float noise.
#[test]
fn split_parts_blend_to_the_unsplit_lookup_on_the_cpu() {
    let whole = small_volume();
    let parts = split_small(&whole);
    let layers: Vec<BakedGiLayer> = parts.iter().map(|p| BakedGiLayer::new(p.clone())).collect();
    let refs: Vec<&BakedGiLayer> = layers.iter().collect();
    let fades = split_fades(&refs);
    assert!(
        fades.iter().all(Option::is_some),
        "each part sees its neighbor installed"
    );
    let band = parts[0].split.as_ref().expect("split part").rect[2]
        - parts[1].split.as_ref().expect("split part").rect[0];
    let band_lo = parts[1].split.as_ref().expect("split part").rect[0];
    let band_hi = parts[0].split.as_ref().expect("split part").rect[2];
    assert!((band - 2.0).abs() < 1e-4, "one terrain cell of band, got {band}");
    let settings = BakedGiSettings::default();
    let lights = BakedGiFrameLights::default();
    let alone = [BakedGiLayer::new(Arc::new(whole.clone()))];
    let mut in_band = 0;
    let mut worst = 0.0f32;
    for xi in 0..=240 {
        let x = -26.0 + xi as f32 * 0.2;
        for (y, n) in [
            (0.0, [0.0, 1.0, 0.0]),
            (1.2, [0.6, 0.0, 0.8]),
            (2.0, [-0.7, 0.7, 0.0]),
        ] {
            for z in [-5.0, -1.3, 0.4, 2.9] {
                let p = [x, y, z];
                let v = [0.0, 0.6, 0.8];
                let a = sample_static_gi_blend(&alone, &settings, &lights, p, n, v);
                let b = sample_static_gi_blend(&layers, &settings, &lights, p, n, v);
                if (band_lo..=band_hi).contains(&x) {
                    in_band += 1;
                }
                assert!(
                    (a.coverage - b.coverage).abs() < 1e-4,
                    "coverage at {p:?}: {} vs {}",
                    a.coverage,
                    b.coverage
                );
                for c in 0..3 {
                    let (u, w) = (a.irradiance_over_pi[c], b.irradiance_over_pi[c]);
                    let rel = (u - w).abs() / u.abs().max(w.abs()).max(1e-3);
                    worst = worst.max(rel);
                    assert!(rel < 1e-4, "irradiance at {p:?} channel {c}: {u} vs {w}");
                }
            }
        }
    }
    assert!(in_band > 100, "{in_band} samples in the band");
    eprintln!("worst relative difference split vs unsplit (CPU): {worst:e}");
}

/// Renders two frames and reads the screen lookup back.
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
    assert!(
        renderer.baked_gi_status().active,
        "{:?}",
        renderer.baked_gi_status()
    );
    renderer
        .baked_gi_read_back(device)
        .expect("read back")
        .expect("the screen lookup ran")
}

/// The acceptance frame: the screen lookup over a floor that crosses the split band equals the
/// unsplit volume's within RGB9E5 rounding (1 %, about two steps of the 9-bit mantissa, plus
/// 1e-3 absolute) and its coverage within 1e-3.
#[test]
fn a_frame_across_the_split_band_equals_the_unsplit_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip baked_gi_split: {err}");
            return;
        }
    };
    let scene = floor_scene();
    let whole = Arc::new(small_volume());
    let parts = split_small(&whole);

    let mut unsplit = FrameRenderer::new();
    unsplit.set_baked_gi(Some(whole.clone()));
    let (hw, hh, a) = lookup(&device, &mut unsplit, &scene);

    let mut split = FrameRenderer::new();
    split.set_baked_gi(Some(parts[0].clone()));
    split.set_baked_gi_layers(&[BakedGiLayer::new(parts[1].clone())]);
    let (hw2, hh2, b) = lookup(&device, &mut split, &scene);
    assert_eq!((hw, hh), (hw2, hh2));
    let status = split.baked_gi_status();
    assert_eq!(status.volumes, 2);
    assert_eq!(status.volume_list.len(), 2);
    assert!(status.volume_list.iter().all(|v| v.split.is_some()));
    assert_eq!(status.volume_list[0].joined, [false, false, true, false]);
    assert_eq!(status.volume_list[1].joined, [true, false, false, false]);

    let mut lit = 0;
    let mut worst = 0.0f32;
    for (k, (ta, tb)) in a.iter().zip(&b).enumerate() {
        assert!(
            (ta.coverage - tb.coverage).abs() < 1e-3,
            "texel {k}: coverage {} vs {}",
            ta.coverage,
            tb.coverage
        );
        if ta.coverage <= 0.0 {
            continue;
        }
        lit += 1;
        for (ea, eb) in ta.e.iter().zip(&tb.e) {
            for c in 0..3 {
                let d = (ea[c] - eb[c]).abs();
                let m = ea[c].abs().max(eb[c].abs());
                worst = worst.max(d / m.max(1e-3));
                assert!(
                    d <= 0.01 * m + 1e-3,
                    "texel {k} channel {c}: {} vs {}",
                    ea[c],
                    eb[c]
                );
            }
        }
    }
    assert!(lit > (hw * hh / 3) as usize, "{lit} lit texels of {}", hw * hh);
    eprintln!("worst relative difference split vs unsplit (GPU): {worst:e}");
}

/// A frame with one part lost (ADR-3006): the part it has lights its own side of the floor with the
/// light of the unsplit frame, the other side holds no light at all, and nothing fails.
#[test]
fn a_frame_with_a_part_lost_keeps_the_other_part_lit() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip baked_gi_split: {err}");
            return;
        }
    };
    let scene = floor_scene();
    let whole = Arc::new(small_volume());
    let parts = split_small(&whole);

    let mut unsplit = FrameRenderer::new();
    unsplit.set_baked_gi(Some(whole.clone()));
    let (_, _, full) = lookup(&device, &mut unsplit, &scene);

    for (kept, lost_name) in [(0usize, "part 1"), (1, "part 0")] {
        let mut renderer = FrameRenderer::new();
        renderer.set_baked_gi(Some(parts[kept].clone()));
        let (_, _, texels) = lookup(&device, &mut renderer, &scene);
        let status = renderer.baked_gi_status();
        assert_eq!(status.error, None, "{lost_name} lost");
        assert_eq!(status.volume_list.len(), 1);
        assert_eq!(
            status.volume_list[0].joined, [false; 4],
            "no neighbor to fade toward"
        );
        let (mut same, mut dark) = (0, 0);
        for (k, (t, f)) in texels.iter().zip(&full).enumerate() {
            if f.coverage >= 0.999 && t.coverage == 0.0 {
                dark += 1;
            }
            if t.coverage < 0.999 || f.coverage < 0.999 {
                continue;
            }
            same += 1;
            for (ea, eb) in t.e.iter().zip(&f.e) {
                for c in 0..3 {
                    let d = (ea[c] - eb[c]).abs();
                    assert!(
                        d <= 0.01 * ea[c].abs().max(eb[c].abs()) + 1e-3,
                        "{lost_name} lost, texel {k} channel {c}: {} vs the unsplit {}",
                        ea[c],
                        eb[c]
                    );
                }
            }
        }
        assert!(
            same > 100,
            "{lost_name} lost: the other part lights {same} texels"
        );
        assert!(
            dark > 100,
            "{lost_name} lost: its side holds no light ({dark} texels)"
        );
    }
}

/// A volume of about 200 MiB (above one 128 MiB binding) splits into two parts; a device clamped
/// to the mobile minimum-spec profile installs both, each in its own storage slot, where the
/// single-buffer upload before ADR-2618 left the second out.
#[test]
fn the_minimum_spec_profile_installs_every_part() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless_with_profile(MinspecProfile::Mobile) {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip baked_gi_split min-spec: {err}");
            return;
        }
    };
    let binding: u64 = device.device.limits().max_storage_buffer_binding_size;
    assert!(binding <= 128 << 20, "the profile clamps the binding: {binding}");
    // 700 × 560 columns × 4 levels at 4 × 4 moments (136 bytes a probe): about 204 MiB, built
    // from sections, so no full-precision probes.
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-700.0, -340.0],
            spacing: [2.0, 2.0],
            counts: [700, 560],
            levels: vec![0.5, 1.5, 4.0, 10.0],
            ground: vec![0.0; 700 * 560],
        }),
        bricks: Vec::new(),
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    let bytes = layout.runtime_buffer_bytes();
    assert!(bytes > binding, "{} MiB fits one binding", bytes >> 20);
    let n = layout.probe_count();
    let mut meta = vec![0u8; n * META_RECORD_BYTES];
    for m in meta.as_chunks_mut::<META_RECORD_BYTES>().0 {
        m[6..8].copy_from_slice(&ProbeFlags::VALID.to_le_bytes());
    }
    let whole = BakedGiVolume::from_sections(
        &layout,
        BakeLights::default(),
        BakeInfo::default(),
        [1.0; GI_COMPONENTS],
        meta,
        vec![0u8; n * GI_COMPONENTS * SH_RECORD_BYTES],
        vec![0u8; n * 16 * MOMENT_TEXEL_BYTES],
    )
    .expect("sections");
    let parts = whole.split_parts(binding).expect("splits");
    assert_eq!(parts.len(), 2);
    drop(whole);
    let parts: Vec<Arc<BakedGiVolume>> = parts.into_iter().map(Arc::new).collect();
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(parts[0].clone()));
    renderer.set_baked_gi_layers(&[BakedGiLayer::new(parts[1].clone())]);
    let mut scene = floor_scene();
    scene.width = 64;
    scene.height = 40;
    renderer
        .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality()))
        .expect("render");
    let status = renderer.baked_gi_status();
    assert_eq!(status.error, None);
    assert_eq!(status.volumes, 2, "{status:?}");
    assert_eq!(status.volumes_dropped, 0);
    assert_eq!(status.volume_slots, 2);
    assert_eq!(status.volume_list[1].slot, 1);
    assert!(
        status.gpu_bytes > binding,
        "both parts are on the GPU: {}",
        status.gpu_bytes
    );
}

/// The brick-loop probe: a histogram of the bricks each texel tested reaches the status a few
/// frames after it is switched on, counts every lit texel once, and leaves the light unchanged.
#[test]
fn the_brick_visit_probe_reads_back_a_histogram() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip baked_gi_split probe: {err}");
            return;
        }
    };
    let scene = floor_scene();
    let whole = Arc::new(small_volume());
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(whole));
    let (_, _, before) = lookup(&device, &mut renderer, &scene);
    assert!(
        renderer.baked_gi_status().brick_visits.is_none(),
        "off by default"
    );
    renderer.set_baked_gi_settings(BakedGiSettings {
        brick_visit_probe: true,
        ..BakedGiSettings::default()
    });
    // The eye moves by a millimetre each frame: a held view is served by the static-screen
    // cache and records no lookup to count.
    let nudged = |k: usize| {
        let mut s = floor_scene();
        s.eye[0] += 1.0e-3 * (k % 2) as f32;
        s
    };
    let mut visits = None;
    for k in 0..12 {
        renderer
            .render_offscreen(&device, &nudged(k + 1).as_frame_scene_with_quality(quality()))
            .expect("render");
        device.wait_idle().expect("idle");
        if let Some(v) = renderer.baked_gi_status().brick_visits {
            visits = Some(v);
            break;
        }
    }
    let visits = visits.expect("a histogram within twelve frames");
    assert_eq!(visits.histogram.len(), regia_render::baked_gi::BRICK_VISIT_BINS);
    // Back at the first view (a moved key, so the lookup records again) with the probe on.
    renderer
        .render_offscreen(&device, &nudged(1).as_frame_scene_with_quality(quality()))
        .expect("render");
    renderer
        .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality()))
        .expect("render");
    let (hw, hh, after) = renderer
        .baked_gi_read_back(&device)
        .expect("read back")
        .expect("lookup");
    let lit = after.iter().filter(|t| t.coverage > 0.0).count() as u64;
    assert!(
        visits.texels > 0 && visits.texels <= u64::from(hw * hh),
        "{visits:?}"
    );
    assert!(
        visits.texels >= lit / 2,
        "most lit texels are counted: {visits:?}, {lit} lit"
    );
    // One brick: a texel tests at most one (the grid lists it for the cells it reaches).
    assert_eq!(visits.max, 1, "{visits:?}");
    assert!(visits.histogram[1] > 0 && visits.histogram[0] > 0, "{visits:?}");
    for (a, b) in before.iter().zip(&after) {
        assert_eq!(
            a.coverage.to_bits(),
            b.coverage.to_bits(),
            "the probe changes no light"
        );
        assert_eq!(a.e, b.e);
    }
}

/// A terrain layer under a 12 × 12 field of overlapping 6 m bricks (a grid cell lists several),
/// for the brick-loop timing.
fn brick_field() -> BakedGiVolume {
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-64.0, -64.0],
            spacing: [2.0, 2.0],
            counts: [64, 64],
            levels: vec![0.5, 1.5, 3.0],
            ground: vec![0.0; 64 * 64],
        }),
        bricks: (0..144)
            .map(|k| DetailBrick {
                origin: [-30.0 + (k % 12) as f32 * 5.0, -0.5, -30.0 + (k / 12) as f32 * 5.0],
                spacing: [1.0, 1.0, 1.0],
                counts: [7, 3, 7],
                first_probe: 0,
                flags: 0,
            })
            .collect(),
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    let dark = [[0.0f32; 4]; 3];
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|i| ProbeData {
            sh: [dark, uniform_sh(1_000.0 + (i % 97) as f32 * 20.0), dark, dark],
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
    BakedGiVolume::from_probes(&layout, lights, BakeInfo::default(), &probes).expect("brick field")
}

/// The brick-loop timing for a real card or phone (ADR-2618, ADR-1835's open item): 1280 × 720
/// over a field of overlapping bricks, the GI screen pass's GPU time (`GpuTimings::gi_ms`, p50 and
/// p95 of 120 frames) with the probe off, then the bricks-per-texel histogram with it on, printed
/// as one JSON line. Needs timestamp queries (not lavapipe); prints and asserts nothing about time.
/// `cargo test --release -p regia_render --test baked_gi_split -- --ignored brick_loop_timing --nocapture`
#[test]
#[ignore = "hardware timing: run by hand on a card or phone (lane report)"]
fn brick_loop_timing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("device");
    let mut scene = floor_scene();
    scene.width = 1280;
    scene.height = 720;
    scene.eye = [0.0, 18.0, 26.0];
    scene.target = [0.0, 0.0, 0.0];
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(Arc::new(brick_field())));
    renderer.set_gpu_timing(&device, true);
    // The eye moves by a millimetre every other frame, so the static-screen cache never serves
    // a frame and every frame runs the lookup.
    let scenes: Vec<LabScene> = (0..2)
        .map(|k| {
            let mut s = scene.clone();
            s.eye[0] += 1.0e-3 * k as f32;
            s
        })
        .collect();
    let frames: Vec<_> = scenes
        .iter()
        .map(|s| s.as_frame_scene_with_quality(quality()))
        .collect();
    for k in 0..20 {
        renderer.render_offscreen(&device, &frames[k % 2]).expect("warm");
    }
    let mut ms: Vec<f32> = (0..120)
        .map(|k| {
            renderer
                .render_offscreen(&device, &frames[k % 2])
                .expect("render");
            renderer.gpu_timings().gi_ms
        })
        .collect();
    ms.sort_by(f32::total_cmp);
    renderer.set_baked_gi_settings(BakedGiSettings {
        brick_visit_probe: true,
        ..BakedGiSettings::default()
    });
    let mut visits = None;
    for k in 0..12 {
        renderer
            .render_offscreen(&device, &frames[k % 2])
            .expect("render");
        device.wait_idle().expect("idle");
        visits = renderer.baked_gi_status().brick_visits.or(visits);
    }
    let v = visits.unwrap_or_default();
    let info = device.adapter.get_info();
    println!(
        "{{\"adapter\": {:?}, \"gi_ms_p50\": {:.4}, \"gi_ms_p95\": {:.4}, \"texels\": {}, \"mean_bricks\": {:.3}, \"max_bricks\": {}, \"histogram\": {:?}}}",
        info.name,
        ms[ms.len() / 2],
        ms[ms.len() * 95 / 100],
        v.texels,
        v.mean,
        v.max,
        &v.histogram[..=(v.max as usize).min(v.histogram.len().saturating_sub(1))]
    );
}

/// ADR-3237: a terrain layer under a tower brick of half-metre cells that is larger than the cap, a
/// small brick inside the tower (it competes for the brick pick) and an interior room. The light
/// varies along every axis, so a lookup that read another probe, another brick or another cell
/// than the unsplit one would differ. The tower crosses every cut, so the split crops it.
fn tower_volume() -> BakedGiVolume {
    let terrain = TerrainLayer {
        origin: [-40.0, -20.0],
        spacing: [2.0, 2.0],
        counts: [41, 21],
        levels: vec![0.5, 1.5, 4.0],
        ground: vec![0.0; 41 * 21],
    };
    let mut layout = VolumeLayout {
        terrain: Some(terrain.clone()),
        bricks: vec![
            DetailBrick {
                origin: [-30.0, -0.5, -6.0],
                spacing: [0.5, 0.5, 0.5],
                counts: [121, 24, 25],
                first_probe: 0,
                flags: 0,
            },
            DetailBrick {
                origin: [-2.0, 1.0, -2.0],
                spacing: [1.0, 1.0, 1.0],
                counts: [5, 4, 5],
                first_probe: 0,
                flags: 0,
            },
            DetailBrick {
                origin: [20.0, 0.5, 10.0],
                spacing: [1.0, 1.0, 1.0],
                counts: [8, 4, 6],
                first_probe: 0,
                flags: regia_asset::baked_gi::BrickFlags::INTERIOR,
            },
        ],
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    let nominal = layout.nominal_positions();
    let dark = [[0.0f32; 4]; 3];
    let small = terrain.probe_count() + layout.bricks[0].probe_count();
    let probes: Vec<ProbeData> = nominal
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let field = 1_200.0 + 60.0 * (p[0] + 40.0) + 45.0 * p[1] + 25.0 * (p[2] + 20.0);
            let sky = if i >= small { 2.0 * field } else { field };
            ProbeData {
                sh: [dark, uniform_sh(sky), dark, dark],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            }
        })
        .collect();
    let lights = BakeLights {
        sky_scale: 1.0,
        ..BakeLights::default()
    };
    BakedGiVolume::from_probes(&layout, lights, BakeInfo::default(), &probes).expect("volume")
}

/// The tower volume split at 70 % of the tower's bytes: no whole-brick split fits, so the tower
/// is cropped at each cut (format version 5).
fn split_cropped(whole: &BakedGiVolume) -> Vec<Arc<BakedGiVolume>> {
    let cap = whole.bricks[0].probe_count() as u64 * 136 * 7 / 10;
    let parts = whole.split_parts(cap).expect("the tower is cropped at the cut");
    assert!(parts.len() >= 2, "{} parts", parts.len());
    let cropped: usize = parts
        .iter()
        .map(|p| p.split.as_ref().map_or(0, |s| s.cropped_bricks(&p.layout())))
        .sum();
    assert!(cropped >= 2, "the tower is cropped in every part it crosses");
    for p in &parts {
        assert!(p.layout().runtime_buffer_bytes() <= cap);
    }
    parts.into_iter().map(Arc::new).collect()
}

/// The CPU mirror across a cropped cut (ADR-3237): along lines crossing every cut, through the
/// tower, the small brick inside it, the terrain beside it and the interior room, the blend of the
/// cropped parts reads as the unsplit volume (same coverage, irradiance within float noise).
#[test]
fn a_cropped_split_blends_to_the_unsplit_lookup_on_the_cpu() {
    let whole = tower_volume();
    let parts = split_cropped(&whole);
    let layers: Vec<BakedGiLayer> = parts.iter().map(|p| BakedGiLayer::new(p.clone())).collect();
    let refs: Vec<&BakedGiLayer> = layers.iter().collect();
    assert!(split_fades(&refs).iter().all(Option::is_some));
    let settings = BakedGiSettings::default();
    let lights = BakedGiFrameLights::default();
    let alone = [BakedGiLayer::new(Arc::new(whole.clone()))];
    // The bands, where two parts blend: every sample inside one is a cut crossing.
    let bands: Vec<(f32, f32)> = parts
        .iter()
        .filter_map(|p| p.split.as_ref())
        .filter(|s| s.neighbor(2).is_some())
        .map(|s| (s.rect[2] - s.band[2], s.rect[2]))
        .collect();
    assert!(!bands.is_empty(), "the split cuts along X");
    let (mut in_band, mut lit) = (0, 0);
    let mut worst = 0.0f32;
    for xi in 0..=680 {
        let x = -34.0 + xi as f32 * 0.1;
        for (y, n) in [
            (0.0, [0.0, 1.0, 0.0]),
            (1.3, [0.6, 0.0, 0.8]),
            (2.6, [-0.7, 0.7, 0.0]),
            (6.1, [0.0, -1.0, 0.0]),
        ] {
            for z in [-5.2, -1.1, 0.0, 0.9, 3.3, 12.0] {
                let p = [x, y, z];
                let v = [0.0, 0.6, 0.8];
                let a = sample_static_gi_blend(&alone, &settings, &lights, p, n, v);
                let b = sample_static_gi_blend(&layers, &settings, &lights, p, n, v);
                if bands.iter().any(|(lo, hi)| (*lo..=*hi).contains(&x)) {
                    in_band += 1;
                }
                if a.coverage > 0.0 {
                    lit += 1;
                }
                assert!(
                    (a.coverage - b.coverage).abs() < 1e-4,
                    "coverage at {p:?}: {} vs {}",
                    a.coverage,
                    b.coverage
                );
                for c in 0..3 {
                    let (u, w) = (a.irradiance_over_pi[c], b.irradiance_over_pi[c]);
                    let rel = (u - w).abs() / u.abs().max(w.abs()).max(1e-3);
                    worst = worst.max(rel);
                    assert!(rel < 1e-4, "irradiance at {p:?} channel {c}: {u} vs {w}");
                }
            }
        }
    }
    assert!(in_band > 100, "{in_band} samples in a band");
    assert!(lit > 10_000, "{lit} lit samples");
    eprintln!("worst relative difference cropped split vs unsplit (CPU): {worst:e}");
}

/// The screen lookup over a floor that crosses a cropped cut equals the unsplit volume's, within
/// the tolerance of the whole-brick split frame above (ADR-3237).
#[test]
fn a_frame_across_a_cropped_cut_equals_the_unsplit_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip baked_gi_split cropped: {err}");
            return;
        }
    };
    let scene = floor_scene();
    let whole = Arc::new(tower_volume());
    let parts = split_cropped(&whole);

    let mut unsplit = FrameRenderer::new();
    unsplit.set_baked_gi(Some(whole.clone()));
    let (hw, hh, a) = lookup(&device, &mut unsplit, &scene);

    let mut split = FrameRenderer::new();
    split.set_baked_gi(Some(parts[0].clone()));
    let rest: Vec<BakedGiLayer> = parts[1..].iter().map(|p| BakedGiLayer::new(p.clone())).collect();
    split.set_baked_gi_layers(&rest);
    let (hw2, hh2, b) = lookup(&device, &mut split, &scene);
    assert_eq!((hw, hh), (hw2, hh2));
    let status = split.baked_gi_status();
    assert_eq!(status.volumes, parts.len());
    assert_eq!(status.volumes_dropped, 0);

    let mut lit = 0;
    let mut worst = 0.0f32;
    for (k, (ta, tb)) in a.iter().zip(&b).enumerate() {
        assert!(
            (ta.coverage - tb.coverage).abs() < 1e-3,
            "texel {k}: coverage {} vs {}",
            ta.coverage,
            tb.coverage
        );
        if ta.coverage <= 0.0 {
            continue;
        }
        lit += 1;
        for (ea, eb) in ta.e.iter().zip(&tb.e) {
            for c in 0..3 {
                let d = (ea[c] - eb[c]).abs();
                let m = ea[c].abs().max(eb[c].abs());
                worst = worst.max(d / m.max(1e-3));
                assert!(
                    d <= 0.01 * m + 1e-3,
                    "texel {k} channel {c}: {} vs {}",
                    ea[c],
                    eb[c]
                );
            }
        }
    }
    assert!(lit > (hw * hh / 3) as usize, "{lit} lit texels of {}", hw * hh);
    eprintln!("worst relative difference cropped split vs unsplit (GPU): {worst:e}");
}
