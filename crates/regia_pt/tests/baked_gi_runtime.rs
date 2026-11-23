// SPDX-License-Identifier: MPL-2.0
//! Baked GI at runtime (baked lighting phase 2): a volume baked by the path tracer from the very
//! frame the renderer draws, installed with `FrameRenderer::set_baked_gi`.
//!
//! - The GPU lookup (`baked_gi.wgsl`, read back from the screen pass) matches its CPU mirror
//!   (`regia_render::sample_static_gi`) at floor points the test ray-casts itself, with each
//!   visibility filter (ADR-1105: per tap with the light-bleeding reduction, and the legacy
//!   moments filter).
//! - Per term (sun bounce, sky) the baked lookup at floor receivers against the path tracer's own
//!   probe at the receiver (the same L1 representation, so the difference is the volume's
//!   interpolation and visibility).
//! - The policy: DDGI and SSGI stay off while the bake lights the frame, even when asked for.
//! - A sunlit trench wall bounces onto the shadowed floor (brighter than the analytic terms); a
//!   sealed room under the sun stays dark inside (no leak through the walls).
//! - The baked lab golden (`baked_gi_trench`, lavapipe; bless with `REGIA_BLESS=1`).
//!
//! Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI).

use std::sync::{Arc, OnceLock};

use regia_asset::baked_gi::{BakedGiVolume, GiComponent, TerrainLayer, VolumeLayout, irradiance};
use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume, bake_lights, detail_brick};
use regia_pt::{PathTracer, PtScene};
use regia_render::{
    BakedGiFrameLights, BakedGiSettings, DdgiScreenTexel, DirectionalLightUniform, FrameQuality,
    FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, box_mesh, component_scales, sample_static_gi,
};
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png_on, golden_path, guard_golden,
};

const W: u32 = 160;
const H: u32 = 96;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("baked GI needs a GPU; lavapipe is a GPU here"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

fn block(centre: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

fn lambert(rgb: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        roughness: 0.95,
        ..LabMaterial::default()
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q
}

fn lab(
    eye: [f32; 3],
    target: [f32; 3],
    meshes: Vec<LabMesh>,
    materials: Vec<LabMaterial>,
    sun: DirectionalLightUniform,
) -> LabScene {
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 13.5;
    LabScene {
        width: W,
        height: H,
        eye,
        target,
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
        directional: sun,
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

/// A trench 6 m wide with 4 m walls, long in x, the sun low across it (travelling −z and down
/// at 35°): the far (−z) wall's inner face is sunlit, the whole floor is in the near wall's
/// shadow, and a grey block stands on it.
fn trench() -> LabScene {
    let meshes = vec![
        block([0.0, -0.25, 0.0], [16.0, 0.25, 8.0], 0),
        block([0.0, 2.0, -3.15], [15.0, 2.0, 0.15], 1),
        block([0.0, 2.0, 3.15], [15.0, 2.0, 0.15], 2),
        block([1.5, 0.5, -1.0], [0.5, 0.5, 0.5], 2),
    ];
    lab(
        [-3.0, 1.6, 2.2],
        [2.0, 0.6, -1.5],
        meshes,
        vec![lambert([0.45; 3]), lambert([0.85, 0.5, 0.3]), lambert([0.6; 3])],
        DirectionalLightUniform::new([0.0, -0.574, -0.819], [1.0, 0.97, 0.92], 100_000.0),
    )
}

/// The trench's probes: a flat terrain-following layer over the floor (13 × 5 columns, 2 m ×
/// 1.4 m, four levels) and a 0.5 m detail brick around the block (its inner probes are inside
/// the block: invalid).
fn trench_layout() -> VolumeLayout {
    let counts = [13u32, 5u32];
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-12.0, -2.8],
            spacing: [2.0, 1.4],
            counts,
            levels: vec![0.5, 1.5, 3.0, 4.5],
            ground: vec![0.0; (counts[0] * counts[1]) as usize],
        }),
        bricks: vec![detail_brick([0.5, 0.1, -2.0], [2.5, 1.6, 0.0], 0.5)],
        moment_resolution: 8,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    layout
}

fn bake_settings() -> GiBakeSettings {
    GiBakeSettings {
        trace: ProbeTraceSettings {
            rays_per_probe: 128,
            samples: 8,
            max_bounces: 4,
            seed: 7,
            samples_per_submit: 8,
            ..ProbeTraceSettings::default()
        },
        ..GiBakeSettings::default()
    }
}

fn pt_scene(scene: &LabScene) -> PtScene {
    from_frame(
        &scene.as_frame_scene_with_quality(quality()),
        &ExtractOptions::default(),
    )
}

fn bake(scene: &PtScene, layout: &VolumeLayout) -> Arc<BakedGiVolume> {
    let bake = bake_gi_volume(tracer(), scene, layout, &bake_settings(), |_, _, _| true).expect("bake");
    eprintln!("baked {:?}", bake.report);
    assert_eq!(bake.report.rejected_rays, 0);
    Arc::new(bake.volume)
}

fn trench_volume() -> Arc<BakedGiVolume> {
    static V: OnceLock<Arc<BakedGiVolume>> = OnceLock::new();
    V.get_or_init(|| bake(&pt_scene(&trench()), &trench_layout()))
        .clone()
}

fn render(renderer: &mut FrameRenderer, scene: &LabScene, q: FrameQuality, frames: u32) -> Vec<u8> {
    let mut last = Vec::new();
    for _ in 0..frames {
        last = renderer
            .render_offscreen(device(), &scene.as_frame_scene_with_quality(q))
            .expect("render")
            .pixels;
    }
    last
}

fn linear_luma(pixels: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let lin = |c: u8| {
        let s = f32::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let (mut sum, mut n) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * lin(pixels[i]) + 0.7152 * lin(pixels[i + 1]) + 0.0722 * lin(pixels[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

/// Writes `pixels` to `$BAKED_GI_DUMP/<name>.png` when that variable is set.
fn dump(name: &str, pixels: &[u8]) {
    let Some(dir) = regia_core::brand::env_var_os("BAKED_GI_DUMP") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::File::create(std::path::Path::new(&dir).join(format!("{name}.png"))) else {
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}

fn frame_lights(scene: &LabScene) -> BakedGiFrameLights {
    let d = Vec3::from_array(scene.directional.direction);
    BakedGiFrameLights {
        sun: Some(scene.directional.color.map(|c| c * scene.directional.illuminance)),
        to_sun: Some((-d.normalize()).to_array()),
        fill: None,
    }
}

/// The world point where the view ray through full-resolution pixel `px` (its centre) meets
/// the plane y = 0, for the lab camera.
fn floor_hit(scene: &LabScene, px: [f32; 2]) -> Option<Vec3> {
    let eye = Vec3::from_array(scene.eye);
    let fwd = (Vec3::from_array(scene.target) - eye).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd);
    let aspect = scene.width as f32 / scene.height as f32;
    // The test quality (Desktop High) keeps the horizontal field of view (Hor+).
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
    let dir = (fwd + right * ndc[0] * t * aspect + up * ndc[1] * t).normalize();
    (dir.y < -1e-3).then(|| eye + dir * (-eye.y / dir.y))
}

/// Whether the segment `a`–`b` passes through the box `lo`–`hi` (slab test).
fn ray_hits_box(a: Vec3, b: Vec3, lo: Vec3, hi: Vec3) -> bool {
    let d = b - a;
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    for k in 0..3 {
        if d[k].abs() < 1e-8 {
            if a[k] < lo[k] || a[k] > hi[k] {
                return false;
            }
            continue;
        }
        let (u, w) = ((lo[k] - a[k]) / d[k], (hi[k] - a[k]) / d[k]);
        t0 = t0.max(u.min(w));
        t1 = t1.min(u.max(w));
    }
    t0 <= t1
}

#[test]
fn the_gpu_lookup_matches_its_cpu_mirror_on_the_trench_floor() {
    gpu_matches_cpu_on_the_trench_floor(BakedGiSettings::default());
}

/// The shader's other visibility branch (ADR-1105): the moments filtered first, no
/// light-bleeding reduction (the lookup before ADR-1105).
#[test]
fn the_gpu_lookup_matches_its_cpu_mirror_with_the_legacy_visibility() {
    gpu_matches_cpu_on_the_trench_floor(BakedGiSettings::legacy_visibility());
}

fn gpu_matches_cpu_on_the_trench_floor(settings: BakedGiSettings) {
    let scene = trench();
    let volume = trench_volume();
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(volume.clone()));
    renderer.set_baked_gi_settings(settings);
    render(&mut renderer, &scene, quality(), 2);
    let status = renderer.baked_gi_status();
    assert!(status.active, "the volume lit the frame: {status:?}");
    assert!(status.gpu_bytes > 0);
    let (hw, hh, texels) = renderer
        .baked_gi_read_back(device())
        .expect("read back")
        .expect("the screen lookup ran");
    let scales = component_scales(&volume, &settings, &frame_lights(&scene));
    let mut checked = 0;
    let mut errors = Vec::new();
    for ty in (2..hh - 2).step_by(3) {
        for tx in (2..hw - 2).step_by(3) {
            let Some(p) = floor_hit(&scene, [(tx * 2) as f32, (ty * 2) as f32]) else {
                continue;
            };
            // Open floor between the walls that the camera sees (the block, grown by 0.3 m for
            // the 2×2 depth normal, does not hide it), within 10 m: farther texels are grazing
            // and their depth normal is noisy.
            let eye = Vec3::from_array(scene.eye);
            let hidden = ray_hits_box(eye, p, Vec3::new(0.7, -0.3, -1.8), Vec3::new(2.3, 1.3, -0.2));
            if p.z.abs() > 2.6 || (p - eye).length() > 10.0 || hidden {
                continue;
            }
            let texel: &DdgiScreenTexel = &texels[(ty * hw + tx) as usize];
            let v = (Vec3::from_array(scene.eye) - p).normalize();
            let cpu = sample_static_gi(
                &volume,
                &settings,
                &scales,
                p.to_array(),
                [0.0, 1.0, 0.0],
                v.to_array(),
            );
            let gpu = texel.at([0.0, 1.0, 0.0]);
            assert!(
                (texel.coverage - cpu.coverage).abs() < 0.05,
                "coverage at {p}: gpu {} cpu {}",
                texel.coverage,
                cpu.coverage
            );
            let dist = (Vec3::from_array(scene.eye) - p).length();
            let err = (0..3)
                .map(|c| (gpu[c] - cpu.irradiance_over_pi[c]).abs() / cpu.irradiance_over_pi[c].max(1.0))
                .fold(0.0f32, f32::max);
            // The CPU light over the pixel's footprint (±½ pixel): where on the floor the depth
            // buffer put the sample is known to a pixel, and the field moves ~8 % per 0.3 m beside
            // the block. The GPU must lie inside that range (1 % margin: RGB9E5 and depth).
            let (mut lo, mut hi) = (cpu.irradiance_over_pi, cpu.irradiance_over_pi);
            for (dx, dy) in [(-0.5, -0.5), (0.5, -0.5), (-0.5, 0.5), (0.5, 0.5)] {
                let Some(q) = floor_hit(&scene, [(tx * 2) as f32 + dx, (ty * 2) as f32 + dy]) else {
                    continue;
                };
                let vq = (eye - q).normalize();
                let e = sample_static_gi(
                    &volume,
                    &settings,
                    &scales,
                    q.to_array(),
                    [0.0, 1.0, 0.0],
                    vq.to_array(),
                )
                .irradiance_over_pi;
                for c in 0..3 {
                    lo[c] = lo[c].min(e[c]);
                    hi[c] = hi[c].max(e[c]);
                }
            }
            for c in 0..3 {
                assert!(
                    gpu[c] >= lo[c] * 0.99 - 1.0 && gpu[c] <= hi[c] * 1.01 + 1.0,
                    "texel ({tx}, {ty}) at {p}: gpu {gpu:?} outside the CPU footprint range {lo:?}..{hi:?}"
                );
            }
            errors.push((err, dist, tx, ty, p, gpu, cpu.irradiance_over_pi));
            checked += 1;
        }
    }
    errors.sort_by(|a, b| b.0.total_cmp(&a.0));
    for e in errors.iter().take(8) {
        eprintln!(
            "gpu vs cpu: err {:.4} at {:.1} m, texel ({}, {}) {} gpu {:?} cpu {:?}",
            e.0, e.1, e.2, e.3, e.4, e.5, e.6
        );
    }
    let worst = errors.first().map_or(0.0, |e| e.0);
    eprintln!(
        "gpu vs cpu lookup ({}): {checked} floor texels, worst relative error {worst:.4}",
        settings.visibility_filter.name()
    );
    assert!(checked >= 20, "too few floor texels checked ({checked})");
    assert!(
        worst < 0.01,
        "the GPU lookup departs from its CPU mirror by {worst:.4}"
    );
}

#[test]
fn per_term_the_bake_matches_the_path_tracer_at_floor_receivers() {
    let scene = trench();
    let pt = pt_scene(&scene);
    let volume = trench_volume();
    // The reference below is the L1 SH traced at each receiver, evaluated linearly
    // (`irradiance`): the lookup is compared through the same evaluation. De-ringing (ADR-861)
    // changes the evaluation, not the interpolation this test is about, and is held against
    // exact answers by `baked_gi_quality.rs`.
    let settings = BakedGiSettings {
        deringing: false,
        ..BakedGiSettings::default()
    };
    let lights = bake_lights(&pt);
    // The baked light levels: every factor 1.
    let scales = component_scales(&volume, &settings, &frame_lights(&scene));
    for k in &scales[..2] {
        for c in k {
            assert!((c - 1.0).abs() < 1e-3, "{scales:?} for {lights:?}");
        }
    }
    let receivers = [
        [-6.0, 0.03, -2.0],
        [-6.0, 0.03, 0.0],
        [-6.0, 0.03, 2.0],
        [6.0, 0.03, -2.0],
        [6.0, 0.03, 0.0],
        [6.0, 0.03, 2.0],
        [-1.0, 0.03, -1.0],
    ];
    let reference = tracer()
        .trace_probes(
            &pt,
            &receivers,
            &ProbeTraceSettings {
                rays_per_probe: 256,
                samples: 32,
                max_bounces: 4,
                seed: 11,
                samples_per_submit: 16,
                moment_resolution: 4,
                max_distance: 16.0,
                ..ProbeTraceSettings::default()
            },
            |_, _| true,
        )
        .expect("trace the receivers");
    let up = [0.0, 1.0, 0.0];
    let eye = Vec3::from_array(scene.eye);
    let mut worst = [0.0f32; 2];
    for (r, t) in receivers.iter().zip(&reference) {
        let v = (eye - Vec3::from_array(*r)).normalize().to_array();
        let baked = sample_static_gi(&volume, &settings, &scales, *r, up, v);
        assert!(baked.coverage > 0.99, "{r:?} {baked:?}");
        for (k, comp) in [GiComponent::SunBounce, GiComponent::Sky].into_iter().enumerate() {
            let want = irradiance(&t.sh[comp as usize], up).map(|e| e / std::f32::consts::PI);
            let got = baked.components[comp as usize];
            let lum = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
            let ratio = lum(got) / lum(want).max(1e-3);
            eprintln!(
                "receiver {r:?} {:<10} baked {:>9.2} pt {:>9.2} ratio {ratio:.3}",
                comp.name(),
                lum(got),
                lum(want)
            );
            worst[k] = worst[k].max((ratio - 1.0).abs());
            assert!(
                (0.75..=1.33).contains(&ratio),
                "{} at {r:?}: baked {got:?} vs path tracer {want:?} (×{ratio:.2})",
                comp.name()
            );
        }
    }
    eprintln!(
        "per-term worst |ratio − 1|: sun bounce {:.3}, sky {:.3}",
        worst[0], worst[1]
    );
}

#[test]
fn the_bake_replaces_ddgi_and_ssgi_and_lights_the_shadowed_floor() {
    let scene = trench();
    let volume = trench_volume();
    let mut renderer = FrameRenderer::new();
    // Asked for, but a baked frame never runs the probe update or SSGI.
    renderer.set_ddgi_enabled(Some(true));
    let mut q = quality();
    q.screen_space_gi = true;
    renderer.set_baked_gi(Some(volume));
    let baked = render(&mut renderer, &scene, q, 2);
    assert!(renderer.baked_gi_status().active);
    assert!(!renderer.ddgi_active(), "DDGI is off on a baked frame");
    assert!(
        !renderer.screen_pass_report().ssgi_composite,
        "SSGI is off on a baked frame"
    );
    dump("trench_baked", &baked);
    // The same frame without the bake (analytic terms).
    let mut flat = FrameRenderer::new();
    flat.set_ddgi_enabled(Some(false));
    let analytic = render(&mut flat, &scene, quality(), 2);
    dump("trench_analytic", &analytic);
    // The lab baseline quality never uses the bake.
    renderer.set_ddgi_enabled(Some(false));
    render(
        &mut renderer,
        &scene,
        FrameQuality {
            baked_gi: false,
            ..quality()
        },
        1,
    );
    assert!(!renderer.baked_gi_status().active, "baked_gi off in the quality");
    // The floor in front of the camera (lower middle of the frame).
    let lit = linear_luma(&baked, 50, 70, 110, 92);
    let dark = linear_luma(&analytic, 50, 70, 110, 92);
    eprintln!("shadowed trench floor: baked {lit:.5}, analytic {dark:.5}");
    assert!(
        lit > dark * 1.05,
        "the bake must add the sunlit wall's bounce ({lit} vs {dark})"
    );
    let guard = GoldenGuard {
        min_lum: 10.0,
        max_lum: 220.0,
        max_white_pct: 20.0,
        max_black_pct: 40.0,
        min_object_pct: 60.0,
    };
    guard_golden(&baked, W, H, "baked_gi_trench", guard);
    compare_rgba8_to_png_on(
        &device().adapter.get_info(),
        &baked,
        W,
        H,
        &golden_path("baked_gi_trench"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("golden");
}

#[test]
fn a_sealed_room_lit_by_a_bake_stays_dark_inside() {
    // A 4 × 3 × 4 m room with 0.3 m walls on open ground under a high sun; the camera inside.
    let mut meshes = vec![block([0.0, -0.25, 0.0], [20.0, 0.25, 20.0], 0)];
    meshes.push(block([0.0, 3.15, 0.0], [2.3, 0.15, 2.3], 0));
    meshes.push(block([-2.15, 1.5, 0.0], [0.15, 1.5, 2.3], 0));
    meshes.push(block([2.15, 1.5, 0.0], [0.15, 1.5, 2.3], 0));
    meshes.push(block([0.0, 1.5, -2.15], [2.0, 1.5, 0.15], 0));
    meshes.push(block([0.0, 1.5, 2.15], [2.0, 1.5, 0.15], 0));
    let inside = lab(
        [0.0, 1.5, 1.5],
        [0.0, 0.5, -1.5],
        meshes.clone(),
        vec![lambert([0.6; 3])],
        DirectionalLightUniform::new([-0.3, -0.9, -0.3], [1.0; 3], 100_000.0),
    );
    let outside = lab(
        [0.0, 1.5, 9.0],
        [0.0, 0.0, 5.0],
        meshes,
        vec![lambert([0.6; 3])],
        DirectionalLightUniform::new([-0.3, -0.9, -0.3], [1.0; 3], 100_000.0),
    );
    let pt = pt_scene(&inside);
    let counts = [11u32, 11u32];
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-7.5, -7.5],
            spacing: [1.5, 1.5],
            counts,
            levels: vec![0.75, 2.25, 4.5],
            ground: vec![0.0; (counts[0] * counts[1]) as usize],
        }),
        bricks: Vec::new(),
        moment_resolution: 8,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    let volume = bake(&pt, &layout);
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(volume));
    let in_px = render(&mut renderer, &inside, quality(), 2);
    assert!(renderer.baked_gi_status().active);
    let out_px = render(&mut renderer, &outside, quality(), 2);
    dump("room_inside", &in_px);
    dump("room_outside", &out_px);
    let interior = linear_luma(&in_px, 40, 50, 120, 90);
    let exterior = linear_luma(&out_px, 40, 60, 120, 92);
    eprintln!(
        "sealed room under a bake: interior {interior:.5}, exterior ground {exterior:.4} ({:.2} %)",
        interior / exterior * 100.0
    );
    assert!(
        interior <= 0.01 * exterior,
        "light leaks into the sealed room: interior {interior:.5} vs exterior {exterior:.4}"
    );
}

/// GPU cost of the Depth look (DDGI update + screen lookup, SSGI, the preset's GTAO) against the
/// bake on the same trench frame at 2560 × 1440, Desktop High and Ultra. Continuous rendering:
/// 120 warm-up frames, then `BAKED_GI_BENCH_FRAMES` (default 240) timed frames with no pixel
/// readback (a drain every 4 frames bounds the queue), the camera swaying so neither the
/// static-screen reuse nor DDGI's settling skips work. Real GPUs only: `--ignored`.
#[test]
#[ignore = "GPU timing on a real GPU: cargo test --release -p regia_pt --test baked_gi_runtime -- --ignored bench"]
fn bench_baked_against_depth_at_1440p() {
    let dev = device();
    assert!(dev.capabilities.timestamp_query, "timestamps needed");
    let frames: u32 = regia_core::brand::env_var("BAKED_GI_BENCH_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240);
    let volume = trench_volume();
    eprintln!("bench adapter: {}", dev.capabilities.adapter_name);
    for preset in [
        RenderProfilePreset::DesktopHigh,
        RenderProfilePreset::DesktopUltra,
    ] {
        let q = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
        let mut results = Vec::new();
        for baked in [false, true] {
            let mut scene = trench();
            scene.width = 2560;
            scene.height = 1440;
            let post = PostSettings {
                auto_exposure: false,
                exposure_ev: 13.5,
                ..PostSettings::default()
            };
            scene.post = Some(post.clone());
            let mut renderer = FrameRenderer::new();
            renderer.set_gpu_timing(dev, true);
            if baked {
                renderer.set_baked_gi(Some(volume.clone()));
            } else {
                renderer.set_ddgi_enabled(Some(true));
            }
            let mut sum = regia_render::GpuTimings::default();
            let mut n = 0.0f32;
            let base = scene.eye;
            for i in 0..(120 + frames) {
                let t = i as f32 * 0.02;
                scene.eye = [base[0] + 0.6 * t.sin(), base[1] + 0.1 * (1.3 * t).sin(), base[2]];
                let frame = scene.as_frame_scene_with_quality(q);
                renderer
                    .begin_offscreen(dev, &frame, wgpu::TextureFormat::Rgba8Unorm)
                    .expect("frame");
                if i % 4 == 3 {
                    let _ = dev.device.poll(wgpu::PollType::wait_indefinitely());
                } else {
                    let _ = dev.device.poll(wgpu::PollType::Poll);
                }
                if i >= 120 {
                    let g = renderer.gpu_timings();
                    sum.frame_ms += g.frame_ms;
                    sum.gi_ms += g.gi_ms;
                    sum.ssgi_ms += g.ssgi_ms;
                    sum.ssao_ms += g.ssao_ms;
                    sum.forward_ms += g.forward_ms;
                    sum.scene_ms += g.scene_ms;
                    n += 1.0;
                }
            }
            assert_eq!(renderer.baked_gi_status().active, baked);
            let avg = |v: f32| v / n.max(1.0);
            eprintln!(
                "bench {preset:?} {}: frame {:.3} ms, scene {:.3}, gi (update + lookup) {:.3}, ssgi {:.3}, ssao {:.3}, forward {:.3}",
                if baked { "baked" } else { "depth" },
                avg(sum.frame_ms),
                avg(sum.scene_ms),
                avg(sum.gi_ms),
                avg(sum.ssgi_ms),
                avg(sum.ssao_ms),
                avg(sum.forward_ms),
            );
            results.push(avg(sum.frame_ms));
        }
        eprintln!(
            "bench {preset:?}: baked − depth = {:+.3} ms per frame",
            results[1] - results[0]
        );
    }
}

/// ADR-366 on the GPU: a 10 cm wall across a floor, a volume with bright probes on one side and
/// dark ones on the other, its wall codes from the scene's own triangles, and moments that see
/// nothing (so only the codes keep the sides apart). The brick reaches under the floor slab (its
/// probes there are invalid, as a bake would mark them), so every floor cell is split by the
/// floor and the one at the wall by the wall too. The screen lookup (`bgi_wall_mask`) matches its
/// CPU mirror, and on both sides of the wall, inside the cell it splits, reads its own side.
#[test]
fn the_gpu_wall_lookup_matches_its_cpu_mirror_beside_a_thin_wall() {
    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, GI_COMPONENTS, ProbeData, ProbeFlags, ProbeMeta, SH_Y00,
    };
    use regia_pt::leak::{LeakGeometry, NeighborWalls};

    const WALL_X: f32 = 0.5;
    let (bright, dark) = (400.0f32, 20.0f32);
    let scene = lab(
        [WALL_X, 6.0, 6.0],
        [WALL_X, 0.0, 0.0],
        vec![
            block([0.0, -0.25, 0.0], [16.0, 0.25, 8.0], 0),
            block([WALL_X, 2.0, 0.0], [0.05, 2.0, 8.0], 0),
        ],
        vec![lambert([0.5; 3])],
        DirectionalLightUniform::new([0.0, -0.574, -0.819], [1.0, 0.97, 0.92], 100_000.0),
    );
    let pt = pt_scene(&scene);
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![detail_brick([-2.0, -1.2, -4.0], [2.0, 2.8, 4.0], 1.0)],
        moment_resolution: 4,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    let positions = layout.nominal_positions();
    let walls = NeighborWalls::scan(&layout, &positions, &LeakGeometry::from_scene(&pt));
    assert!(walls.walled_probes() > 0, "the wall splits the brick");
    let k = 4.0 * std::f32::consts::PI * SH_Y00;
    let probes: Vec<ProbeData> = positions
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let l = if p[0] < WALL_X { bright } else { dark };
            let mut sh = [[[0.0f32; 4]; 3]; GI_COMPONENTS];
            sh[GiComponent::Sky as usize] = [[k * l, 0.0, 0.0, 0.0]; 3];
            let validity = if p[1] < 0.0 {
                ProbeFlags::INSIDE_GEOMETRY
            } else {
                ProbeFlags::VALID
            };
            ProbeData {
                sh,
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: validity | walls.probe_flags(i),
                },
            }
        })
        .collect();
    let volume = Arc::new(
        BakedGiVolume::from_probes(
            &layout,
            BakeLights {
                sky_scale: 1.0,
                ..BakeLights::default()
            },
            BakeInfo::default(),
            &probes,
        )
        .expect("volume"),
    );
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(volume.clone()));
    render(&mut renderer, &scene, quality(), 2);
    let status = renderer.baked_gi_status();
    assert!(status.active, "the volume lit the frame: {status:?}");
    assert_eq!(status.wall_probes, walls.walled_probes());
    let (hw, hh, texels) = renderer
        .baked_gi_read_back(device())
        .expect("read back")
        .expect("the screen lookup ran");
    let settings = BakedGiSettings::default();
    let scales = component_scales(&volume, &settings, &frame_lights(&scene));
    let eye = Vec3::from_array(scene.eye);
    let up = [0.0, 1.0, 0.0];
    let mut sides = [0usize; 2];
    let mut checked = 0;
    for ty in 1..hh - 1 {
        for tx in 1..hw - 1 {
            let Some(p) = floor_hit(&scene, [(tx * 2) as f32, (ty * 2) as f32]) else {
                continue;
            };
            // Inside the brick, off its last cell, clear of the wall's foot (the 2×2 depth normal
            // is noisy there) and not behind the wall.
            let hidden = ray_hits_box(
                eye,
                p,
                Vec3::new(WALL_X - 0.05, 0.0, -8.0),
                Vec3::new(WALL_X + 0.05, 4.0, 8.0),
            );
            if p.x.abs() > 1.0 || p.z.abs() > 3.0 || (p.x - WALL_X).abs() < 0.2 || hidden {
                continue;
            }
            let texel: &DdgiScreenTexel = &texels[(ty * hw + tx) as usize];
            let gpu = texel.at(up);
            let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
            for (dx, dy) in [(0.0, 0.0), (-0.5, -0.5), (0.5, -0.5), (-0.5, 0.5), (0.5, 0.5)] {
                let Some(q) = floor_hit(&scene, [(tx * 2) as f32 + dx, (ty * 2) as f32 + dy]) else {
                    continue;
                };
                let v = (eye - q).normalize().to_array();
                let e = sample_static_gi(&volume, &settings, &scales, q.to_array(), up, v).irradiance_over_pi;
                for c in 0..3 {
                    lo[c] = lo[c].min(e[c]);
                    hi[c] = hi[c].max(e[c]);
                }
            }
            for c in 0..3 {
                assert!(
                    gpu[c] >= lo[c] * 0.99 - 1.0 && gpu[c] <= hi[c] * 1.01 + 1.0,
                    "texel ({tx}, {ty}) at {p}: gpu {gpu:?} outside the CPU range {lo:?}..{hi:?}"
                );
            }
            // In the cell the wall splits, each side reads its own probes only.
            if (0.0..1.0).contains(&p.x) {
                let want = if p.x < WALL_X { bright } else { dark };
                assert!(
                    (gpu[0] - want).abs() <= 0.02 * want + 1.0,
                    "texel ({tx}, {ty}) at {p}: gpu {gpu:?}, its side reads {want}"
                );
                sides[usize::from(p.x > WALL_X)] += 1;
            }
            checked += 1;
        }
    }
    eprintln!("wall lookup: {checked} floor texels, split cell {sides:?} (bright, dark side)");
    assert!(checked >= 20, "too few floor texels checked ({checked})");
    assert!(
        sides[0] >= 3 && sides[1] >= 3,
        "both sides of the wall seen: {sides:?}"
    );
}

// Probe GI screen lookup resolution (ADR-1033): the lookup that carries the baked volume (and
// the GPU probes) to the forward and terrain passes is half resolution in every preset.
// `FrameRenderer::set_gi_lookup_resolution` measures it against a full-resolution lookup and a
// quarter-resolution one.

/// Renders `scene` once per lookup resolution with a fresh renderer (the same frame sequence,
/// so the same depth buffer) and returns the lookup read back and the frame.
fn render_at_lookup(
    scene: &LabScene,
    volume: &Arc<BakedGiVolume>,
    resolution: regia_render::GiLookupResolution,
) -> ((u32, u32, Vec<DdgiScreenTexel>), Vec<u8>) {
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(volume.clone()));
    renderer.set_gi_lookup_resolution(resolution);
    assert_eq!(renderer.gi_lookup_resolution(), resolution);
    let pixels = render(&mut renderer, scene, quality(), 2);
    assert!(renderer.baked_gi_status().active, "the volume lit the frame");
    let lookup = renderer
        .baked_gi_read_back(device())
        .expect("read back")
        .expect("the screen lookup ran");
    (lookup, pixels)
}

/// Every lookup resolution stores, per texel, exactly what the full-resolution lookup stores
/// for the pixel under the texel's top-left corner: the step only chooses which pixels the
/// lookup visits. A wrong step in either shader (lookup or upsample) or in the buffer sizing
/// would read other pixels or leave texels unwritten.
#[test]
fn every_lookup_resolution_stores_the_pixel_under_its_texel() {
    use regia_render::GiLookupResolution;
    let scene = trench();
    let volume = trench_volume();
    let ((fw, fh, full), _) = render_at_lookup(&scene, &volume, GiLookupResolution::Full);
    assert_eq!(
        (fw, fh),
        (W, H),
        "a full-resolution lookup has one texel per pixel"
    );
    // Enough covered texels that equal lookups are not trivially all zero (sky, or floor past
    // the layer's ends, is uncovered).
    let full_covered = full.iter().filter(|t| t.coverage > 0.0).count();
    assert!(
        full_covered > full.len() / 8,
        "too little of the trench frame is covered by the bake ({full_covered} of {})",
        full.len()
    );
    for resolution in [GiLookupResolution::Half, GiLookupResolution::Quarter] {
        let step = resolution.step();
        let ((w, h, texels), _) = render_at_lookup(&scene, &volume, resolution);
        assert_eq!((w, h), resolution.lookup_size(W, H), "{resolution:?} lookup size");
        assert_eq!(texels.len(), (w * h) as usize);
        let mut covered = 0;
        for ty in 0..h {
            for tx in 0..w {
                let got = &texels[(ty * w + tx) as usize];
                let want = &full[(ty * step * fw + tx * step) as usize];
                assert_eq!(
                    got,
                    want,
                    "{resolution:?} texel ({tx}, {ty}) differs from the full lookup at pixel ({}, {})",
                    tx * step,
                    ty * step
                );
                covered += usize::from(got.coverage > 0.0);
            }
        }
        assert!(
            covered > texels.len() / 8,
            "{resolution:?}: {covered} covered texels"
        );
    }
}

/// Display-encoded RGB of an RGBA8 frame (`regia_render::compare::ssim`'s input).
fn display_rgb(pixels: &[u8]) -> Vec<[f32; 3]> {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| [p[0], p[1], p[2]].map(|c| f32::from(c) / 255.0))
        .collect()
}

/// Image error of `test` against `reference` (RGBA8, `w` × `h`).
#[derive(Debug)]
struct LookupError {
    /// Mean SSIM of the luma (11 × 11 Gaussian window).
    ssim: f32,
    /// Largest channel difference (/255) on edge pixels: the reference's luma steps by more
    /// than 0.08 to a 4-neighbour (silhouettes, creases, shadow edges).
    max_edge: u8,
    /// Largest channel difference (/255) elsewhere.
    max_flat: u8,
    /// Mean of the per-pixel largest channel difference (/255).
    mean: f32,
    /// 99.9th percentile of the per-pixel largest channel difference (/255).
    p999: u8,
    /// Edge pixels.
    edge_pixels: usize,
}

fn lookup_error(reference: &[u8], test: &[u8], w: u32, h: u32) -> LookupError {
    let a = display_rgb(reference);
    let b = display_rgb(test);
    let ssim = regia_render::compare::ssim(&a, &b, w, h);
    let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let (wu, hu) = (w as usize, h as usize);
    let mut diffs = Vec::with_capacity(wu * hu);
    let (mut max_edge, mut max_flat, mut edge_pixels) = (0u8, 0u8, 0usize);
    for y in 0..hu {
        for x in 0..wu {
            let i = y * wu + x;
            let d = (0..3)
                .map(|c| reference[4 * i + c].abs_diff(test[4 * i + c]))
                .max()
                .unwrap_or(0);
            let l = luma(a[i]);
            let edge = [
                (x.wrapping_sub(1), y),
                (x + 1, y),
                (x, y.wrapping_sub(1)),
                (x, y + 1),
            ]
            .into_iter()
            .any(|(nx, ny)| nx < wu && ny < hu && (luma(a[ny * wu + nx]) - l).abs() > 0.08);
            if edge {
                edge_pixels += 1;
                max_edge = max_edge.max(d);
            } else {
                max_flat = max_flat.max(d);
            }
            diffs.push(d);
        }
    }
    let mean = diffs.iter().map(|&d| f32::from(d)).sum::<f32>() / diffs.len().max(1) as f32;
    diffs.sort_unstable();
    let p999 = diffs
        .get((diffs.len() * 999 / 1000).min(diffs.len().saturating_sub(1)))
        .copied()
        .unwrap_or(0);
    LookupError {
        ssim,
        max_edge,
        max_flat,
        mean,
        p999,
        edge_pixels,
    }
}

#[test]
fn lookup_error_measures_differences_on_and_off_edges() {
    // A 4 × 1 frame: black | black | white | white. The middle two pixels are edge pixels.
    let reference: Vec<u8> = [0u8, 0, 255, 255].iter().flat_map(|&v| [v, v, v, 255]).collect();
    let same = lookup_error(&reference, &reference, 4, 1);
    assert_eq!((same.max_edge, same.max_flat, same.p999), (0, 0, 0));
    assert_eq!(same.edge_pixels, 2);
    assert!((same.ssim - 1.0).abs() < 1e-4, "{same:?}");
    let mut test = reference.clone();
    test[4] = 40; // pixel 1 (an edge pixel), red channel
    test[12] = 245; // pixel 3 (flat), red channel
    let e = lookup_error(&reference, &test, 4, 1);
    assert_eq!((e.max_edge, e.max_flat), (40, 10), "{e:?}");
    assert!((e.mean - 12.5).abs() < 1e-4, "{e:?}");
}

/// The trench seen down its length past six thin posts (0.1 m square, 2 m tall) at 4–19 m:
/// one to three pixels wide at 480 × 288, thinner than a half-resolution lookup texel far off,
/// where the upsample has no texel on the post and takes the nearest in depth.
fn trench_posts_along() -> LabScene {
    let mut scene = trench();
    for x in [-6.0, -3.0, 0.0, 3.0, 6.0, 9.0] {
        scene.meshes.push(block([x, 1.0, -1.8], [0.05, 1.0, 0.05], 2));
    }
    scene.eye = [-10.0, 1.7, 0.4];
    scene.target = [4.0, 0.6, -0.8];
    scene
}

/// The frame size of the image-error measurement: `GI_LOOKUP_ERROR_SIZE` (`WxH`), default
/// 480 × 288 (lavapipe renders it in seconds).
fn error_size() -> (u32, u32) {
    regia_core::brand::env_var("GI_LOOKUP_ERROR_SIZE")
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .filter(|&(w, h): &(u32, u32)| w > 0 && h > 0)
        .unwrap_or((480, 288))
}

/// Image error of the half- and quarter-resolution lookups against a full-resolution one, per
/// scene: the trench lit by its bake from the golden's camera, from one looking down the
/// trench past the block, and past thin posts (`trench_posts_along`), and the trench under the
/// GPU probes (no bake). Prints SSIM and the
/// largest differences on and off edges; asserts nothing beyond the frames rendering (it
/// measures, ADR-1033). Any adapter: `GI_LOOKUP_ERROR_SIZE=1920x1080` on a GPU (a full-
/// resolution probe lookup above 1920 × 1080 falls back to half; the printout says so).
#[test]
#[ignore = "measurement: cargo test --release -p regia_pt --test baked_gi_runtime -- --ignored --nocapture measure_gi_lookup"]
fn measure_gi_lookup_resolution_error() {
    use regia_render::GiLookupResolution;
    let (w, h) = error_size();
    let volume = trench_volume();
    let mut along = trench();
    along.eye = [-10.0, 1.7, 0.4];
    along.target = [4.0, 0.6, -0.8];
    let scenes = [
        ("trench_baked", trench(), true),
        ("trench_along_baked", along, true),
        ("trench_posts_baked", trench_posts_along(), true),
        ("trench_probes", trench(), false),
    ];
    for (name, mut scene, baked) in scenes {
        scene.width = w;
        scene.height = h;
        let mut frames = Vec::new();
        for resolution in [
            GiLookupResolution::Full,
            GiLookupResolution::Half,
            GiLookupResolution::Quarter,
        ] {
            let mut renderer = FrameRenderer::new();
            renderer.set_gi_lookup_resolution(resolution);
            let pixels = if baked {
                renderer.set_baked_gi(Some(volume.clone()));
                render(&mut renderer, &scene, quality(), 2)
            } else {
                // The probes converge over their first updates (every frame for eight).
                renderer.set_ddgi_enabled(Some(true));
                render(&mut renderer, &scene, quality(), 24)
            };
            assert_eq!(pixels.len(), (w * h * 4) as usize);
            let used = if baked {
                renderer
                    .baked_gi_read_back(device())
                    .ok()
                    .flatten()
                    .map(|(lw, lh, _)| (lw, lh))
            } else {
                renderer
                    .ddgi_read_back(device())
                    .ok()
                    .flatten()
                    .map(|r| (r.screen[1], r.screen[2]))
            };
            frames.push((resolution, used, pixels));
        }
        let (_, full_used, reference) = &frames[0];
        eprintln!("gi lookup error {name} {w}x{h}: Full lookup {full_used:?}");
        for (resolution, used, pixels) in &frames[1..] {
            let e = lookup_error(reference, pixels, w, h);
            eprintln!(
                "gi lookup error {name} {w}x{h} {resolution:?} (lookup {used:?}) vs Full: ssim {:.5}, max edge {}/255, max elsewhere {}/255, mean {:.3}/255, p99.9 {}/255 ({} edge pixels)",
                e.ssim, e.max_edge, e.max_flat, e.mean, e.p999, e.edge_pixels
            );
            dump_sized(&format!("lookup_{name}_{resolution:?}"), pixels, w, h);
        }
        dump_sized(&format!("lookup_{name}_Full"), reference, w, h);
    }
}

/// [`dump`] for a frame of any size.
fn dump_sized(name: &str, pixels: &[u8], w: u32, h: u32) {
    let Some(dir) = regia_core::brand::env_var_os("BAKED_GI_DUMP") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::File::create(std::path::Path::new(&dir).join(format!("{name}.png"))) else {
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}

/// GPU cost of the probe GI screen lookup at full, half (the presets') and quarter resolution,
/// at 1920 × 1080 and 2560 × 1440, Desktop High, for the trench lit by its bake and by the GPU
/// probes. Continuous rendering as `bench_baked_against_depth_at_1440p` (camera swaying, no
/// readback, a drain every 4 frames): `GI_LOOKUP_BENCH_WARM` (default 120) warm-up frames, then
/// `GI_LOOKUP_BENCH_FRAMES` (default 240) timed ones. Prints the frame, the GI total, the
/// lookup pass and the forward pass, and the half − quarter saving that ADR-1033's bar
/// (≥ 0.3 ms at 1440p) is judged on.
#[test]
#[ignore = "GPU timing: cargo test --release -p regia_pt --test baked_gi_runtime -- --ignored --nocapture bench_gi_lookup"]
fn bench_gi_lookup_resolution() {
    use regia_render::GiLookupResolution;
    let dev = device();
    assert!(dev.capabilities.timestamp_query, "timestamps needed");
    let env = |key: &str, default: u32| {
        regia_core::brand::env_var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let warm = env("GI_LOOKUP_BENCH_WARM", 120);
    let frames = env("GI_LOOKUP_BENCH_FRAMES", 240).max(1);
    let volume = trench_volume();
    eprintln!("bench adapter: {}", dev.capabilities.adapter_name);
    let q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    for (w, h) in [(1920u32, 1080u32), (2560, 1440)] {
        for baked in [true, false] {
            let source = if baked { "baked" } else { "probes" };
            let mut lookup_ms = Vec::new();
            let mut frame_ms = Vec::new();
            for resolution in [
                GiLookupResolution::Full,
                GiLookupResolution::Half,
                GiLookupResolution::Quarter,
            ] {
                let mut scene = trench();
                scene.width = w;
                scene.height = h;
                scene.post = Some(PostSettings {
                    auto_exposure: false,
                    exposure_ev: 13.5,
                    ..PostSettings::default()
                });
                let mut renderer = FrameRenderer::new();
                renderer.set_gpu_timing(dev, true);
                renderer.set_gi_lookup_resolution(resolution);
                if baked {
                    renderer.set_baked_gi(Some(volume.clone()));
                } else {
                    renderer.set_ddgi_enabled(Some(true));
                }
                let mut sum = regia_render::GpuTimings::default();
                let mut n = 0.0f32;
                let base = scene.eye;
                for i in 0..(warm + frames) {
                    let t = i as f32 * 0.02;
                    scene.eye = [base[0] + 0.6 * t.sin(), base[1] + 0.1 * (1.3 * t).sin(), base[2]];
                    let frame = scene.as_frame_scene_with_quality(q);
                    renderer
                        .begin_offscreen(dev, &frame, wgpu::TextureFormat::Rgba8Unorm)
                        .expect("frame");
                    if i % 4 == 3 {
                        let _ = dev.device.poll(wgpu::PollType::wait_indefinitely());
                    } else {
                        let _ = dev.device.poll(wgpu::PollType::Poll);
                    }
                    if i >= warm {
                        let g = renderer.gpu_timings();
                        sum.frame_ms += g.frame_ms;
                        sum.scene_ms += g.scene_ms;
                        sum.gi_ms += g.gi_ms;
                        sum.gi_stages_ms[4] += g.gi_stages_ms[4];
                        sum.forward_ms += g.forward_ms;
                        n += 1.0;
                    }
                }
                assert_eq!(renderer.baked_gi_status().active, baked);
                // The lookup the frames used (a full-resolution probe lookup above 1920 × 1080
                // falls back to half).
                let used = if baked {
                    renderer
                        .baked_gi_read_back(dev)
                        .ok()
                        .flatten()
                        .map(|(lw, lh, _)| (lw, lh))
                } else {
                    renderer
                        .ddgi_read_back(dev)
                        .ok()
                        .flatten()
                        .map(|r| (r.screen[1], r.screen[2]))
                };
                let avg = |v: f32| v / n.max(1.0);
                eprintln!(
                    "bench {w}x{h} {source} {resolution:?} (lookup {used:?}): frame {:.3} ms, scene {:.3}, gi {:.3}, lookup {:.3}, forward {:.3}",
                    avg(sum.frame_ms),
                    avg(sum.scene_ms),
                    avg(sum.gi_ms),
                    avg(sum.gi_stages_ms[4]),
                    avg(sum.forward_ms),
                );
                lookup_ms.push(avg(sum.gi_stages_ms[4]));
                frame_ms.push(avg(sum.frame_ms));
            }
            eprintln!(
                "bench {w}x{h} {source}: full − half = {:+.3} ms lookup, {:+.3} ms frame; half − quarter = {:+.3} ms lookup, {:+.3} ms frame",
                lookup_ms[0] - lookup_ms[1],
                frame_ms[0] - frame_ms[1],
                lookup_ms[1] - lookup_ms[2],
                frame_ms[1] - frame_ms[2],
            );
        }
    }
}

/// Image error of screen-space GI gathered at half resolution (with the joint-bilateral
/// upsample) against the full gather that native Desktop High and Ultra run, on maps without a
/// bake (the bake turns SSGI off): the trench from the golden's camera, down its length and
/// past thin posts, under the GPU probes (as the presets run it), and with the probes off (SSGI
/// alone). Prints
/// as `measure_gi_lookup_resolution_error`; asserts nothing beyond the frames rendering.
#[test]
#[ignore = "measurement: cargo test --release -p regia_pt --test baked_gi_runtime -- --ignored --nocapture measure_ssgi"]
fn measure_ssgi_half_res_error() {
    let (w, h) = error_size();
    let mut along = trench();
    along.eye = [-10.0, 1.7, 0.4];
    along.target = [4.0, 0.6, -0.8];
    let scenes = [
        ("trench_ssgi_probes", trench(), true),
        ("trench_along_ssgi_probes", along.clone(), true),
        ("trench_posts_ssgi_probes", trench_posts_along(), true),
        ("trench_ssgi_alone", trench(), false),
        ("trench_along_ssgi_alone", along, false),
    ];
    assert!(quality().screen_space_gi, "the test quality runs SSGI");
    for (name, mut scene, probes) in scenes {
        scene.width = w;
        scene.height = h;
        // Full gather, then half with the shipped upsample, then half with the relative-depth
        // variant (ADR-1033).
        let variants = [
            ("full", false, false),
            ("half", true, false),
            ("half_reldepth", true, true),
        ];
        let mut frames = Vec::new();
        for (_, half, relative) in variants {
            let mut renderer = FrameRenderer::new();
            renderer.set_ssgi_half_res(Some(half));
            renderer.set_ssgi_upsample_relative_depth(relative);
            renderer.set_ddgi_enabled(Some(probes));
            let pixels = render(&mut renderer, &scene, quality(), if probes { 24 } else { 2 });
            assert_eq!(pixels.len(), (w * h * 4) as usize);
            frames.push(pixels);
        }
        for (k, (label, _, _)) in variants.iter().enumerate().skip(1) {
            let e = lookup_error(&frames[0], &frames[k], w, h);
            eprintln!(
                "ssgi error {name} {w}x{h} {label} vs full gather: ssim {:.5}, max edge {}/255, max elsewhere {}/255, mean {:.3}/255, p99.9 {}/255 ({} edge pixels)",
                e.ssim, e.max_edge, e.max_flat, e.mean, e.p999, e.edge_pixels
            );
        }
        for ((label, _, _), pixels) in variants.iter().zip(&frames) {
            dump_sized(&format!("ssgi_{name}_{label}"), pixels, w, h);
        }
    }
}

/// GPU cost of screen-space GI gathered at full resolution (native Desktop High and Ultra) and
/// at half resolution with the joint-bilateral upsample, at 1920 × 1080 and 2560 × 1440, on the
/// trench without a bake, under the GPU probes (as the presets run it) and without them.
/// Desktop High and Ultra; continuous rendering as `bench_gi_lookup_resolution` (same
/// `GI_LOOKUP_BENCH_*` variables). Prints the frame, SSGI and forward times and the full − half
/// saving that ADR-1033's bar (≥ 0.3 ms at 1440p) is judged on.
#[test]
#[ignore = "GPU timing: cargo test --release -p regia_pt --test baked_gi_runtime -- --ignored --nocapture bench_ssgi"]
fn bench_ssgi_half_res() {
    let dev = device();
    assert!(dev.capabilities.timestamp_query, "timestamps needed");
    let env = |key: &str, default: u32| {
        regia_core::brand::env_var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let warm = env("GI_LOOKUP_BENCH_WARM", 120);
    let frames = env("GI_LOOKUP_BENCH_FRAMES", 240).max(1);
    eprintln!("bench adapter: {}", dev.capabilities.adapter_name);
    for preset in [
        RenderProfilePreset::DesktopHigh,
        RenderProfilePreset::DesktopUltra,
    ] {
        let q = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
        assert!(q.screen_space_gi, "{preset:?} runs SSGI");
        for (w, h) in [(1920u32, 1080u32), (2560, 1440)] {
            for probes in [true, false] {
                let mut ssgi_ms = Vec::new();
                let mut frame_ms = Vec::new();
                // Full gather, half with the shipped upsample, half with the relative-depth one.
                for (label, half, relative) in [
                    ("full", false, false),
                    ("half", true, false),
                    ("half_reldepth", true, true),
                ] {
                    let mut scene = trench();
                    scene.width = w;
                    scene.height = h;
                    scene.post = Some(PostSettings {
                        auto_exposure: false,
                        exposure_ev: 13.5,
                        ..PostSettings::default()
                    });
                    let mut renderer = FrameRenderer::new();
                    renderer.set_gpu_timing(dev, true);
                    renderer.set_ssgi_half_res(Some(half));
                    renderer.set_ssgi_upsample_relative_depth(relative);
                    renderer.set_ddgi_enabled(Some(probes));
                    let mut sum = regia_render::GpuTimings::default();
                    let mut n = 0.0f32;
                    let base = scene.eye;
                    for i in 0..(warm + frames) {
                        let t = i as f32 * 0.02;
                        scene.eye = [base[0] + 0.6 * t.sin(), base[1] + 0.1 * (1.3 * t).sin(), base[2]];
                        let frame = scene.as_frame_scene_with_quality(q);
                        renderer
                            .begin_offscreen(dev, &frame, wgpu::TextureFormat::Rgba8Unorm)
                            .expect("frame");
                        if i % 4 == 3 {
                            let _ = dev.device.poll(wgpu::PollType::wait_indefinitely());
                        } else {
                            let _ = dev.device.poll(wgpu::PollType::Poll);
                        }
                        if i >= warm {
                            let g = renderer.gpu_timings();
                            sum.frame_ms += g.frame_ms;
                            sum.scene_ms += g.scene_ms;
                            sum.gi_ms += g.gi_ms;
                            sum.ssgi_ms += g.ssgi_ms;
                            sum.forward_ms += g.forward_ms;
                            n += 1.0;
                        }
                    }
                    let avg = |v: f32| v / n.max(1.0);
                    eprintln!(
                        "bench {preset:?} {w}x{h} probes {probes} ssgi {label}: frame {:.3} ms, scene {:.3}, gi {:.3}, ssgi {:.3}, forward {:.3}",
                        avg(sum.frame_ms),
                        avg(sum.scene_ms),
                        avg(sum.gi_ms),
                        avg(sum.ssgi_ms),
                        avg(sum.forward_ms),
                    );
                    ssgi_ms.push(avg(sum.ssgi_ms));
                    frame_ms.push(avg(sum.frame_ms));
                }
                eprintln!(
                    "bench {preset:?} {w}x{h} probes {probes}: ssgi full − half = {:+.3} ms ssgi, {:+.3} ms frame; full − half_reldepth = {:+.3} ms ssgi, {:+.3} ms frame",
                    ssgi_ms[0] - ssgi_ms[1],
                    frame_ms[0] - frame_ms[1],
                    ssgi_ms[0] - ssgi_ms[2],
                    frame_ms[0] - frame_ms[2],
                );
            }
        }
    }
}

/// The SSGI override reaches the gather: at native scale the default (`None`) is the full
/// gather, byte for byte, and `Some(true)` changes the frame (the half-resolution gather and
/// its upsample). The probes stay off so the frames depend on SSGI alone.
#[test]
fn the_ssgi_override_switches_the_gather_resolution() {
    let scene = trench();
    assert!(quality().screen_space_gi && !quality().ssgi_half_res());
    let frame = |half: Option<bool>| {
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_enabled(Some(false));
        renderer.set_ssgi_half_res(half);
        assert_eq!(renderer.ssgi_half_res(), half);
        // One frame: the first always gathers (a held second frame may reuse it).
        let pixels = render(&mut renderer, &scene, quality(), 1);
        assert!(renderer.screen_pass_report().ssgi_composite, "SSGI ran");
        pixels
    };
    let default = frame(None);
    let full = frame(Some(false));
    let half = frame(Some(true));
    assert!(default == full, "the default native gather is the full one");
    // The upsample's relative-depth variant touches only the half-resolution upsample: the full
    // gather does not run it.
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_ssgi_half_res(Some(false));
    renderer.set_ssgi_upsample_relative_depth(true);
    assert!(renderer.ssgi_upsample_relative_depth());
    assert!(
        render(&mut renderer, &scene, quality(), 1) == full,
        "the relative-depth upsample changed a full-resolution gather"
    );
    let changed = full
        .as_chunks::<4>()
        .0
        .iter()
        .zip(half.as_chunks::<4>().0)
        .filter(|(a, b)| a != b)
        .count();
    assert!(changed > 0, "the half-resolution gather changed no pixel");
}
