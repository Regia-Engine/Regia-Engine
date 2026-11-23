// SPDX-License-Identifier: MPL-2.0
//! ReSTIR GI against the path tracer and the same harness's DDGI rows (ADR-3349).
//! Small fixed-seed lavapipe scenes; no wall-clock assertions and no golden images.
//!
//! Run explicitly with `--ignored --nocapture --test-threads=1` on a device with ray queries and
//! hit-vertex return. The fallback test separately uses an ordinary device without those features.

use std::sync::OnceLock;

use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtImage, PtSettings};
use regia_render::restir_gi::RestirGiFallback;
use regia_render::{
    FrameQuality, FrameRenderer, GiMode, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode,
    frame_path_counters,
};
use regia_test_utils::gi_error::{
    GI_TABLE_ENV, GiError, GiErrorInput, GiRow, measure, quantile, render_table, shadow_mask, sub_planes,
    surface_mask,
};
use regia_test_utils::gi_scenes::{GiScene, color_room, sealed_box, trench};

const W: u32 = 80;
const H: u32 = 48;
const REFERENCE_SPP: u32 = 128;
const PROBE_FRAMES: u32 = 64;
const RESTIR_FRAMES: u32 = 64;
const STILL_FRAMES: u32 = 4;
const STABILITY_FRAMES: u32 = 32;
const LEAK_TARGET: f32 = 0.005;
const STABILITY_TARGET: f32 = 0.01;

fn ray_device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| {
        let device = RenderDevice::headless_with_ray_queries().expect("ray-query headless device");
        let features = device.device.features();
        assert!(features.contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY));
        assert!(features.contains(wgpu::Features::EXPERIMENTAL_RAY_HIT_VERTEX_RETURN));
        device
    })
}

fn plain_device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| {
        regia_render::hwrt::set_device_request(regia_render::hwrt::HwrtRequest::Off);
        RenderDevice::headless().expect("ordinary headless device")
    })
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(ray_device()).expect("compile the path tracer"))
}

fn base_quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Ambient;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q.paged_sun_shadows = false;
    q
}

fn probes_quality(ssgi: bool) -> FrameQuality {
    FrameQuality {
        gi: GiMode::Probes,
        gi_probes: true,
        screen_space_gi: ssgi,
        ..base_quality()
    }
}

fn restir_quality() -> FrameQuality {
    FrameQuality {
        restir_gi: true,
        ..probes_quality(false)
    }
}

fn small(mut scene: GiScene) -> GiScene {
    scene.lab.width = W;
    scene.lab.height = H;
    scene.ddgi.screen_max = [W / 2, H / 2];
    scene
}

fn reference(scene: &GiScene) -> PtImage {
    let frame = scene.lab.as_frame_scene_with_quality(base_quality());
    let pt = from_frame(&frame, &ExtractOptions::default());
    let image = tracer()
        .render(
            &pt,
            &PtSettings {
                spp: REFERENCE_SPP,
                max_bounces: 6,
                seed: 23,
                jitter: false,
                samples_per_submit: 8,
                ..PtSettings::default()
            },
        )
        .expect("trace the reference");
    assert_eq!((image.width, image.height), (W, H));
    image
}

fn renderer(scene: &GiScene, probes: bool) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_settings(scene.ddgi.clone());
    renderer.set_ddgi_enabled(Some(probes));
    renderer
}

fn capture_on(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    lab: &regia_render::LabScene,
    quality: FrameQuality,
    frames: u32,
) -> Vec<[f32; 3]> {
    let frame = lab.as_frame_scene_with_quality(quality);
    for index in 0..frames {
        if index + 1 == frames {
            renderer.request_hdr_capture();
        }
        renderer.render_offscreen(device, &frame).expect("frame");
    }
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("read back")
        .expect("the frame recorded its HDR copy");
    assert_eq!((hdr.width, hdr.height), (W, H));
    hdr.rgb
}

fn capture(
    renderer: &mut FrameRenderer,
    lab: &regia_render::LabScene,
    quality: FrameQuality,
    frames: u32,
) -> Vec<[f32; 3]> {
    capture_on(ray_device(), renderer, lab, quality, frames)
}

fn realtime_direct(scene: &GiScene) -> Vec<[f32; 3]> {
    let mut lab = scene.lab.clone();
    lab.ambient_lux = 0.0;
    let quality = FrameQuality {
        ssao: SsaoMode::Off,
        ..base_quality()
    };
    capture(&mut renderer(scene, false), &lab, quality, STILL_FRAMES)
}

fn flip_p95(reference: &[[f32; 3]], test: &[[f32; 3]], mask: &[bool], ev100: f32) -> f32 {
    let blended: Vec<[f32; 3]> = test
        .iter()
        .zip(reference)
        .zip(mask)
        .map(|((t, r), m)| if *m { *t } else { *r })
        .collect();
    let (_, flip) = regia_render::compare::compare_linear(
        reference,
        &blended,
        W,
        H,
        ev100,
        regia_render::compare::DEFAULT_PPD,
    );
    let values: Vec<f32> = flip
        .map
        .iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .map(|(v, _)| *v)
        .collect();
    quantile(&values, 0.95)
}

fn row<'a>(rows: &'a [GiRow], technique: &str) -> &'a GiError {
    &rows
        .iter()
        .find(|r| r.technique == technique)
        .unwrap_or_else(|| panic!("no `{technique}` row"))
        .error
}

fn validate(scene: &GiScene) -> Vec<GiRow> {
    let image = reference(scene);
    let surface = surface_mask(&image.depth, W, H, 0.05);
    assert!(
        surface.iter().filter(|v| **v).count() > 100,
        "{}: too few surface pixels",
        scene.name
    );
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    let direct = realtime_direct(scene);
    let score = |test: &[[f32; 3]]| {
        measure(&GiErrorInput {
            reference: &image.indirect_diffuse,
            test,
            surface: &surface,
            shadow: &shadow,
            width: W,
            height: H,
            ev100: scene.lab.exposure_ev,
            scale: scene.scale,
        })
    };
    let p95 = |test: &[[f32; 3]]| flip_p95(&image.indirect_diffuse, test, &surface, scene.lab.exposure_ev);
    let lab = &scene.lab;
    let mut rows = Vec::new();
    let mut push = |name: &str, plane: &[[f32; 3]]| {
        let error = score(plane);
        let p95 = p95(plane);
        eprintln!(
            "{} {name}: indirect_diffuse FLIP mean {:.6}, p95 {p95:.6}",
            scene.name, error.flip_mean
        );
        rows.push(GiRow {
            scene: scene.name.to_owned(),
            technique: name.to_owned(),
            error,
        });
    };

    let mut ddgi = renderer(scene, true);
    let full = capture(&mut ddgi, lab, probes_quality(false), PROBE_FRAMES);
    assert!(ddgi.ddgi_active());
    push("probes", &sub_planes(&full, &direct));
    let full = capture(&mut ddgi, lab, probes_quality(true), STILL_FRAMES);
    push("probes_ssgi", &sub_planes(&full, &direct));

    let mut restir = renderer(scene, true);
    let full = capture(&mut restir, lab, restir_quality(), RESTIR_FRAMES);
    let report = restir.restir_gi_report();
    assert!(report.active, "{}: ReSTIR GI did not run: {report:?}", scene.name);
    assert!(report.fallback.is_none(), "{report:?}");
    assert!(restir.ddgi_active(), "the probes remain the hit-radiance source");
    eprintln!("{}: ReSTIR GI work {:?}", scene.name, report.total_work);
    assert!(
        report.total_work.rays > 0,
        "no ray counters completed: {report:?}"
    );
    let restir_plane = sub_planes(&full, &direct);
    push("restir_gi", &restir_plane);

    // Warm both parities, then require no new GPU objects or traced-scene builds.
    let counters = frame_path_counters();
    let transients = restir.graph_report().graph.transients.created;
    let builds = restir.sun_trace_report();
    let frames = restir.restir_gi_report().frames;
    let frame = lab.as_frame_scene_with_quality(restir_quality());
    for _ in 0..4 {
        restir
            .render_offscreen(ray_device(), &frame)
            .expect("steady frame");
    }
    let after = frame_path_counters().since(counters);
    assert_eq!(
        (after.pipelines, after.bind_groups, after.buffers, after.textures),
        (0, 0, 0, 0),
        "{}: steady creations {after:?}",
        scene.name
    );
    assert_eq!(restir.graph_report().graph.transients.created, transients);
    let after_builds = restir.sun_trace_report();
    assert_eq!(after_builds.scene_builds, builds.scene_builds);
    assert_eq!(after_builds.tlas_refits, builds.tlas_refits);
    assert_eq!(after_builds.accel.blas_built, builds.accel.blas_built);
    assert!(restir.restir_gi_report().frames > frames);

    eprintln!("ReSTIR GI on {}:\n{}", scene.name, render_table(&rows));
    if let Some(dir) = regia_core::brand::env_var_os(GI_TABLE_ENV) {
        let dir = std::path::PathBuf::from(dir);
        let path = dir.join(format!("{}_restir_gi.md", scene.name));
        if let Err(error) = std::fs::create_dir_all(&dir)
            .and_then(|()| regia_test_utils::write_atomic(&path, render_table(&rows).as_bytes()))
        {
            eprintln!("{GI_TABLE_ENV}: {error}");
        }
    }
    let ddgi_mean = row(&rows, "probes_ssgi").flip_mean;
    let restir_mean = row(&rows, "restir_gi").flip_mean;
    if restir_mean >= ddgi_mean {
        eprintln!(
            "{}: plate improvement missed: ReSTIR GI mean FLIP {restir_mean:.6} vs DDGI+SSGI {ddgi_mean:.6}; \
             ReSTIR GI relative RMSE {:.6} vs {:.6}",
            scene.name,
            row(&rows, "restir_gi").rel_rmse,
            row(&rows, "probes_ssgi").rel_rmse
        );
    }
    rows
}

#[test]
#[ignore = "needs ray queries and hit-vertex return; run explicitly on lavapipe"]
fn trench_reports_flip_next_to_ddgi() {
    let rows = validate(&small(trench()));
    let restir = row(&rows, "restir_gi");
    assert!(restir.flip_mean.is_finite() && restir.rel_rmse.is_finite());
}

#[test]
#[ignore = "needs ray queries and hit-vertex return; run explicitly on lavapipe"]
fn color_room_reports_flip_next_to_ddgi() {
    let rows = validate(&small(color_room()));
    let restir = row(&rows, "restir_gi");
    assert!(restir.flip_mean.is_finite() && restir.rel_rmse.is_finite());
}

#[test]
#[ignore = "needs ray queries and hit-vertex return; run explicitly on lavapipe"]
fn sealed_box_leak_stays_within_the_spec27_bound() {
    let scene = small(sealed_box());
    let rows = validate(&scene);
    let scale = scene.scale.expect("the leak scene names its scale");
    let leak = row(&rows, "restir_gi").mean_test / scale;
    let plain = row(&rows, "probes_ssgi").mean_test / scale;
    eprintln!(
        "sealed_box leak: ReSTIR GI {:.6} %, DDGI+SSGI {:.6} %",
        leak * 100.0,
        plain * 100.0
    );
    assert!(
        leak <= LEAK_TARGET,
        "ReSTIR GI leak {:.6} % exceeds {:.1} %",
        leak * 100.0,
        LEAK_TARGET * 100.0
    );
}

#[test]
#[ignore = "needs ray queries and hit-vertex return; run explicitly on lavapipe"]
fn held_view_stays_within_the_temporal_bound_after_convergence() {
    let scene = small(trench());
    let direct = realtime_direct(&scene);
    let mut renderer = renderer(&scene, true);
    let frame = scene.lab.as_frame_scene_with_quality(restir_quality());
    let mut previous = Vec::new();
    let mut worst = 0.0f32;
    let mask = vec![true; (W * H) as usize];
    for index in 0..RESTIR_FRAMES + STABILITY_FRAMES {
        if index + 1 >= RESTIR_FRAMES {
            renderer.request_hdr_capture();
        }
        renderer
            .render_offscreen(ray_device(), &frame)
            .expect("held frame");
        if index + 1 >= RESTIR_FRAMES {
            let hdr = renderer
                .take_hdr_capture(ray_device())
                .expect("read back")
                .expect("HDR capture");
            let current = sub_planes(&hdr.rgb, &direct);
            if !previous.is_empty() {
                let (_, flip) = regia_render::compare::compare_linear(
                    &previous,
                    &current,
                    W,
                    H,
                    scene.lab.exposure_ev,
                    regia_render::compare::DEFAULT_PPD,
                );
                let mean = flip
                    .map
                    .iter()
                    .zip(&mask)
                    .filter(|(_, m)| **m)
                    .map(|(v, _)| *v)
                    .sum::<f32>()
                    / flip.map.len().max(1) as f32;
                worst = worst.max(mean);
            }
            previous = current;
        }
    }
    eprintln!("held view: worst frame-to-frame FLIP over {STABILITY_FRAMES} frames = {worst:.6}");
    assert!(
        worst <= STABILITY_TARGET,
        "held-view FLIP {worst:.6} exceeds {STABILITY_TARGET}"
    );
}

#[test]
fn a_device_without_ray_queries_draws_the_ddgi_frame_bit_identically() {
    let scene = small(trench());
    let lab = &scene.lab;
    let quality = restir_quality();
    let mut fallback = renderer(&scene, true);
    let mut plain = renderer(&scene, true);
    let with_request = capture_on(plain_device(), &mut fallback, lab, quality, 4);
    let without_request = capture_on(
        plain_device(),
        &mut plain,
        lab,
        FrameQuality {
            restir_gi: false,
            ..quality
        },
        4,
    );
    assert_eq!(with_request, without_request, "the fallback moved one channel");
    let report = fallback.restir_gi_report();
    assert!(!report.active);
    assert_eq!(report.fallback, Some(RestirGiFallback::NoRayQuery));
}
