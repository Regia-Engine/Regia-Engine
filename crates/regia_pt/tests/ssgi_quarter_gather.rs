// SPDX-License-Identifier: MPL-2.0
//! Quarter-resolution screen-space GI (ADR-3964), scored on lavapipe against the path tracer.
//!
//! The half-resolution gather (texel step 2, today's Desktop High 0.67 path) must stay inside
//! the existing spec 27 records (`gi_thresholds`, `GiTolerance::for_adapter`), and must match
//! the original `max(noise.y, 1)` shader calculation pixel for pixel. The quarter gather
//! (step 4) is printed: GI-in-shadow ratio on the open scenes, leak on the sealed box. A miss does
//! not fail the test and does not turn the default on. No wall-clock assert. No golden image.
//!
//! One test, so the process-wide gather setting cannot race another test in this binary.

use std::sync::OnceLock;

use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtImage, PtSettings};
use regia_render::{
    FrameQuality, FrameRenderer, GiMode, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode,
    SsgiGatherGuard, ssgi_gather_step_used, ssgi_quarter_gather,
};
use regia_test_utils::gi_error::{
    GiError, GiErrorInput, GiRecord, GiTolerance, add_planes, gi_thresholds_path, measure, read_records,
    regressions, shadow_mask, sub_planes, surface_mask,
};
use regia_test_utils::gi_scenes::{GI_HEIGHT, GI_WIDTH, GiScene, color_room, sealed_box, trench};
use regia_test_utils::golden::{GoldenIcd, golden_icd, goldens_skipped};

type GatherCapture = (Vec<[f32; 3]>, u32);

/// Frames the dynamic probes run before the screen-space rows. Matches `gi_validation`.
const PROBE_FRAMES: u32 = 64;
/// Frames of the screen-space pass.
const SSGI_FRAMES: u32 = 4;
/// Frames of the direct-only capture.
const STILL_FRAMES: u32 = 2;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("quarter gather measurement needs lavapipe"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
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
    q
}

fn ssgi_quality() -> FrameQuality {
    FrameQuality {
        gi: GiMode::ScreenSpace,
        screen_space_gi: true,
        ..base_quality()
    }
}

fn probes_quality(ssgi: bool) -> FrameQuality {
    FrameQuality {
        gi: GiMode::Probes,
        gi_probes: true,
        screen_space_gi: ssgi,
        ..base_quality()
    }
}

fn renderer(scene: &GiScene, probes: bool) -> FrameRenderer {
    let mut r = FrameRenderer::new();
    r.set_ddgi_settings(scene.ddgi.clone());
    r.set_ddgi_enabled(Some(probes));
    // Step 2 unless the quarter guard is on. Native scale would otherwise gather at step 1.
    r.set_ssgi_half_res(Some(true));
    r
}

fn capture(
    renderer: &mut FrameRenderer,
    lab: &regia_render::LabScene,
    quality: FrameQuality,
    frames: u32,
) -> Vec<[f32; 3]> {
    let frame = lab.as_frame_scene_with_quality(quality);
    for i in 0..frames {
        if i + 1 == frames {
            renderer.request_hdr_capture();
        }
        renderer.render_offscreen(device(), &frame).expect("frame");
    }
    let hdr = renderer
        .take_hdr_capture(device())
        .expect("read back")
        .expect("the frame recorded its HDR copy");
    assert_eq!((hdr.width, hdr.height), (GI_WIDTH, GI_HEIGHT));
    hdr.rgb
}

fn reference(scene: &GiScene) -> PtImage {
    let frame = scene.lab.as_frame_scene_with_quality(base_quality());
    let pt = from_frame(&frame, &ExtractOptions::default());
    let image = tracer()
        .render(
            &pt,
            &PtSettings {
                spp: 1024,
                max_bounces: 6,
                seed: 23,
                jitter: false,
                samples_per_submit: 8,
                ..PtSettings::default()
            },
        )
        .expect("trace the reference");
    assert_eq!((image.width, image.height), (GI_WIDTH, GI_HEIGHT));
    image
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

/// Indirect light at `quarter`, and the texel step the gather wrote.
fn ssgi_indirect(scene: &GiScene, direct: &[[f32; 3]], quarter: bool) -> GatherCapture {
    let _guard = SsgiGatherGuard::quarter(quarter);
    let full = capture(
        &mut renderer(scene, false),
        &scene.lab,
        ssgi_quality(),
        SSGI_FRAMES,
    );
    (sub_planes(&full, direct), ssgi_gather_step_used())
}

/// Each scale sees the same fixed number of probe updates, independently of measurement order.
fn probes_ssgi_indirect(scene: &GiScene, direct: &[[f32; 3]], quarter: bool) -> GatherCapture {
    let _guard = SsgiGatherGuard::quarter(quarter);
    let mut probes = renderer(scene, true);
    let _settled = capture(&mut probes, &scene.lab, probes_quality(false), PROBE_FRAMES);
    assert!(probes.ddgi_active(), "{}: the probes lit the frame", scene.name);
    let full = capture(&mut probes, &scene.lab, probes_quality(true), SSGI_FRAMES);
    assert!(probes.ddgi_active(), "{}: the probes stayed on", scene.name);
    (sub_planes(&full, direct), ssgi_gather_step_used())
}

/// Replays the original scale expression through the existing shader-reload API.
/// No stored image or additional shader fixture: only the changed scale selection is reverted.
fn original_half_captures(scene: &GiScene, direct: &[[f32; 3]]) -> (GatherCapture, GatherCapture) {
    let source = include_str!("../../regia_render/shaders/ssgi.wgsl");
    let current = "let scale = gather_scale();";
    assert_eq!(
        source.matches(current).count(),
        2,
        "gather and upsample scale selections"
    );
    let original = source.replace(current, "let scale = max(params.noise.y, 1.0);");
    regia_render::shader_reload::stage_shader_edit("ssgi.wgsl", &original)
        .expect("validate the original scale calculation");
    let plain = ssgi_indirect(scene, direct, false);
    let probes = probes_ssgi_indirect(scene, direct, false);
    regia_render::shader_reload::clear_shader_overrides();
    (plain, probes)
}

/// A reused half gather must be replaced on each change of step, even with the camera held.
fn held_view_switches_step(scene: &GiScene) {
    let mut view = renderer(scene, false);
    let half = {
        let _guard = SsgiGatherGuard::quarter(false);
        capture(&mut view, &scene.lab, ssgi_quality(), SSGI_FRAMES)
    };
    let quarter = {
        let _guard = SsgiGatherGuard::quarter(true);
        let rgb = capture(&mut view, &scene.lab, ssgi_quality(), 1);
        assert_eq!(ssgi_gather_step_used(), 4);
        assert!(
            view.screen_pass_report().ssgi_gather,
            "a step change gathers again"
        );
        rgb
    };
    assert!(mean_abs(&half, &quarter) > 1.0e-4, "held view must show quarter");
    let restored = {
        let _guard = SsgiGatherGuard::quarter(false);
        let rgb = capture(&mut view, &scene.lab, ssgi_quality(), 1);
        assert_eq!(ssgi_gather_step_used(), 2);
        assert!(
            view.screen_pass_report().ssgi_gather,
            "clearing quarter gathers again"
        );
        rgb
    };
    assert_eq!(half, restored, "clearing quarter restores the held half image");
}

fn score(scene: &GiScene, image: &PtImage, reference_indirect: &[[f32; 3]], test: &[[f32; 3]]) -> GiError {
    let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    measure(&GiErrorInput {
        reference: reference_indirect,
        test,
        surface: &surface,
        shadow: &shadow,
        width: GI_WIDTH,
        height: GI_HEIGHT,
        ev100: scene.lab.exposure_ev,
        scale: scene.scale,
    })
}

fn fmt_opt(v: Option<f32>) -> String {
    match v {
        Some(v) => format!("{v:.5}"),
        None => "-".to_owned(),
    }
}

fn within_record(scene: &str, technique: &str, error: &GiError) -> bool {
    let path = gi_thresholds_path(scene);
    let records = read_records(&path).unwrap_or_else(|e| {
        panic!(
            "no GI record for `{scene}` ({e}): the records live in {}",
            path.display()
        )
    });
    let record = records
        .iter()
        .find(|r| r.scene == scene && r.technique == technique)
        .unwrap_or_else(|| panic!("no `{technique}` record for `{scene}` in {}", path.display()));
    let info = device().adapter.get_info();
    regressions(error, record, GiTolerance::for_adapter(&info)).is_empty()
}

fn report(scene: &GiScene, technique: &str, step: u32, error: &GiError, within: bool) {
    // Open scenes: shadow_bias is (shadow_test − shadow_ref) / shadow_ref, so the GI-in-shadow
    // ratio is 1 + shadow_bias. The sealed box divides by the lit exterior, so the leak is
    // mean_test / scale and the bias is not that ratio.
    let gi_in_shadow = if scene.scale.is_none() {
        fmt_opt(error.shadow_bias.map(|b| 1.0 + b))
    } else {
        "-".to_owned()
    };
    let leak = if scene.scale.is_some() {
        format!("{:.5}", error.mean_test / error.scale)
    } else {
        "-".to_owned()
    };
    eprintln!(
        "GI_QUARTER scene={} technique={technique} step={step} rel_rmse={:.5} shadow_bias={} energy_ratio={} flip={:.5} gi_in_shadow={gi_in_shadow} leak={leak} mean_test={:.5} within={within}",
        scene.name,
        error.rel_rmse,
        fmt_opt(error.shadow_bias),
        fmt_opt(error.energy_ratio),
        error.flip_mean,
        error.mean_test,
    );
}

fn pin_half(scene: &str, technique: &str, error: &GiError) {
    let path = gi_thresholds_path(scene);
    let records = read_records(&path).expect("GI records");
    let record = records
        .iter()
        .find(|r| r.scene == scene && r.technique == technique)
        .unwrap_or_else(|| panic!("no `{technique}` record for `{scene}`"));
    let info = device().adapter.get_info();
    let failures = regressions(error, record, GiTolerance::for_adapter(&info));
    let report_only = goldens_skipped() && matches!(golden_icd(&info), GoldenIcd::Hardware);
    if report_only {
        for failure in &failures {
            eprintln!("GI_QUARTER half regression (reported, not failed off lavapipe): {failure}");
        }
        return;
    }
    assert!(
        failures.is_empty(),
        "half-resolution `{technique}` on `{scene}` left the spec 27 record ({:?}):\n{}",
        GiTolerance::for_adapter(&info),
        failures.join("\n")
    );
}

fn mean_abs(a: &[[f32; 3]], b: &[[f32; 3]]) -> f32 {
    let (sum, n) = a.iter().zip(b).fold((0.0f64, 0usize), |(sum, n), (x, y)| {
        let d = (x[0] - y[0]).abs() + (x[1] - y[1]).abs() + (x[2] - y[2]).abs();
        (sum + f64::from(d), n + 1)
    });
    (sum / n.max(1) as f64) as f32
}

fn check_row(
    scene: &GiScene,
    image: &PtImage,
    reference_indirect: &[[f32; 3]],
    technique: &str,
    half: (&[[f32; 3]], u32),
    quarter: (&[[f32; 3]], u32),
) {
    assert_eq!(half.1, 2, "{technique}: the half gather must write step 2");
    assert_eq!(quarter.1, 4, "{technique}: the quarter gather must write step 4");
    let half_error = score(scene, image, reference_indirect, half.0);
    let quarter_error = score(scene, image, reference_indirect, quarter.0);
    assert!(
        half_error.pixels > 0 && half_error.flip_mean.is_finite() && quarter_error.flip_mean.is_finite(),
        "{technique}: {:?}",
        (half_error, quarter_error)
    );
    let gap = mean_abs(half.0, quarter.0);
    assert!(
        gap > 1.0e-4,
        "{technique}: quarter matched half (mean abs {gap}); the step-4 gather did not run"
    );
    let half_within = within_record(scene.name, technique, &half_error);
    let quarter_within = within_record(scene.name, technique, &quarter_error);
    report(scene, technique, 2, &half_error, half_within);
    report(scene, technique, 4, &quarter_error, quarter_within);
    let half_record = GiRecord::from_error(scene.name, technique, &half_error);
    let quarter_regressions = regressions(
        &quarter_error,
        &half_record,
        GiTolerance::for_adapter(&device().adapter.get_info()),
    );
    eprintln!(
        "GI_QUARTER_COMPARE scene={} technique={technique} within_half={} flip_delta={:.5}",
        scene.name,
        quarter_regressions.is_empty(),
        quarter_error.flip_mean - half_error.flip_mean,
    );
    for regression in quarter_regressions {
        eprintln!("GI_QUARTER_MISS {regression}");
    }
    pin_half(scene.name, technique, &half_error);
}

/// Half-resolution SSGI stays inside the spec 27 records. Quarter is measured and left off.
#[test]
fn quarter_gather_holds_or_misses_the_spec27_rows_and_stays_off() {
    assert!(!ssgi_quarter_gather(), "quarter gather defaults off");
    let info = device().adapter.get_info();
    eprintln!(
        "GI_QUARTER adapter {} ({}; {})",
        info.name, info.driver, info.driver_info
    );
    for scene in [trench(), color_room(), sealed_box()] {
        let image = reference(&scene);
        let reference_indirect = add_planes(&image.indirect_diffuse, &image.indirect_specular);
        let direct = realtime_direct(&scene);
        let (half, half_step) = ssgi_indirect(&scene, &direct, false);
        let (quarter, quarter_step) = ssgi_indirect(&scene, &direct, true);
        check_row(
            &scene,
            &image,
            &reference_indirect,
            "ssgi",
            (&half, half_step),
            (&quarter, quarter_step),
        );
        let (ph, ph_step) = probes_ssgi_indirect(&scene, &direct, false);
        let (pq, pq_step) = probes_ssgi_indirect(&scene, &direct, true);
        check_row(
            &scene,
            &image,
            &reference_indirect,
            "probes_ssgi",
            (&ph, ph_step),
            (&pq, pq_step),
        );
        let (original, original_probes) = original_half_captures(&scene, &direct);
        assert_eq!(original.1, 2);
        assert_eq!(original_probes.1, 2);
        assert_eq!(half, original.0, "{}: half SSGI pixels changed", scene.name);
        assert_eq!(
            ph, original_probes.0,
            "{}: half probes+SSGI pixels changed",
            scene.name
        );
        eprintln!(
            "GI_HALF_PIN scene={} ssgi=identical probes_ssgi=identical",
            scene.name
        );
        assert!(
            !ssgi_quarter_gather(),
            "{}: the guard restored the default",
            scene.name
        );
    }
    held_view_switches_step(&trench());
    assert!(
        !ssgi_quarter_gather(),
        "quarter gather is still off after the measurement"
    );
}
