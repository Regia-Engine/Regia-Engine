// SPDX-License-Identifier: MPL-2.0
//! Attribution of the § 2.2 GI (diffuse) plate misses (lane sprint-gi-leak-plates-p95,
//! ADR-3701): which term of the real-time indirect light carries the error the
//! `gi_validation` plates score against the path tracer.
//!
//! Every row is one toggle on the same renderer at the same settled state (the
//! `gi_radiance_cache` pattern): the renderer settles the probes once at the baseline row's
//! quality, then each row flips one `FrameQuality` field (or one renderer switch) and reads the
//! same number of frames. The probe hysteresis, the SSGI temporal state and the static-screen
//! cache therefore start every row from the same settled renderer, and the rows differ only in
//! the switched term — a fresh renderer per row would leave each row's state at a different age,
//! which is the confound that made the gather's sign flip across renderers (the 2026-11-07 lane
//! report). Every row is scored with `gi_error::measure` exactly as `gi_validation` scores its
//! rows: relative RMSE, shadow bias, energy ratio, mean FLIP and p95 FLIP over the pixels that
//! see a surface away from silhouettes. Switching one term off at a time at that settled state
//! attributes the miss:
//!
//! | row | probes | SSGI | SSAO | static-screen reuse | radiance cache |
//! |---|---|---|---|---|---|
//! | `probes_ssgi` | on | on | on (Desktop High) | on | off |
//! | `probes` | on | off | on | off (no screen pass) | off |
//! | `ssgi` | off | on | on | off (fresh frames) | off |
//! | `probes_ssgi_no_ao` | on | on | off | on | off |
//! | `probes_ssgi_no_reuse` | on | on | on | off | off |
//! | `probes_ssgi_cache` | on | on | on | on | on (hits only, the default) |
//!
//! Two rows turn reuse off where the baseline holds it, and both say so. The `probes` row has
//! no screen pass, so there is nothing to reuse. The `ssgi` row flips the probes off, which the
//! screen cache key does not cover (it never keyed the probe state), so holding the baseline's
//! composite would shade probes-on light: it renders fresh frames instead. Every other toggle
//! changes the cache key (the AO flag, `paged_sun_shadows`, the radiance cache), so the held
//! view serves the toggled term's own composite after its first frame.
//!
//! `ambient` (the flat sky term) is the floor every probe row must beat. The table is printed
//! (and written with `$REGIA_GI_TABLE` like `gi_validation`). This is a measurement test
//! (`--ignored`): it asserts only what the attribution itself proves — every probe row beats
//! the flat sky term, and the screen gather and the AO terms move the score by the small
//! amounts this lane measured (a large move fails: the attribution changed).

use std::sync::OnceLock;

use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtImage, PtSettings};
use regia_render::{
    FrameQuality, FrameRenderer, GiMode, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode,
};
use regia_test_utils::gi_error::{
    GI_TABLE_ENV, GiError, GiErrorInput, GiRow, add_planes, measure, render_table, shadow_mask, sub_planes,
    surface_mask,
};
use regia_test_utils::gi_scenes::{GI_HEIGHT, GI_WIDTH, GiScene, color_room, sealed_box, trench};
use regia_test_utils::write_atomic;

/// Frames before the rows are read; the plates' probes re-trace every frame at hysteresis
/// 0.9, so a bounce chain settles by about 64 (see `gi_validation`).
const PROBE_FRAMES: u32 = 64;
/// Frames the screen pass's temporal filter needs before a row reads it ("a few").
const SSGI_FRAMES: u32 = 4;
/// Frames every switch-off row reads after its toggle: an identical count per row is part of
/// the attribution (a row that read more frames would read a more settled state).
const ROW_FRAMES: u32 = SSGI_FRAMES;
/// Frames of the techniques without history.
const STILL_FRAMES: u32 = 2;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("this test needs a GPU; lavapipe is a GPU here"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

/// Desktop High as the lab runs it: native scale without temporal upscaling, no fog or clouds,
/// no reflections, and every GI technique off; each row turns its own on. Identical to
/// `gi_validation`'s base so the rows read the same frame the plates score.
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

fn reference(scene: &GiScene) -> PtImage {
    let frame = scene.lab.as_frame_scene_with_quality(base_quality());
    let pt = from_frame(&frame, &ExtractOptions::default());
    tracer()
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
        .expect("trace the reference")
}

/// A renderer with the scene's probe settings, probes on or off.
fn renderer(scene: &GiScene, probes: bool) -> FrameRenderer {
    let mut r = FrameRenderer::new();
    r.set_ddgi_settings(scene.ddgi.clone());
    r.set_ddgi_enabled(Some(probes));
    r
}

/// Renders `frames` frames of `lab` at `quality` and returns the last one's linear HDR.
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

/// The frame's direct light alone: the sun with its shadows, no sky, no GI, no AO.
fn realtime_direct(scene: &GiScene) -> Vec<[f32; 3]> {
    let mut lab = scene.lab.clone();
    lab.ambient_lux = 0.0;
    let quality = FrameQuality {
        ssao: SsaoMode::Off,
        ..base_quality()
    };
    capture(&mut renderer(scene, false), &lab, quality, STILL_FRAMES)
}

/// The attribution rows of `scene`, scored against the path tracer's indirect light.
fn attribute(scene: &GiScene) -> Vec<GiRow> {
    let image = reference(scene);
    let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    let reference_indirect = add_planes(&image.indirect_diffuse, &image.indirect_specular);
    let direct = realtime_direct(scene);
    let score = |test: &[[f32; 3]]| -> GiError {
        measure(&GiErrorInput {
            reference: reference_indirect.as_slice(),
            test,
            surface: &surface,
            shadow: &shadow,
            width: GI_WIDTH,
            height: GI_HEIGHT,
            ev100: scene.lab.exposure_ev,
            scale: scene.scale,
        })
    };
    let lab = &scene.lab;
    let mut rows = Vec::new();
    let mut push = |name: &str, plane: &[[f32; 3]]| {
        rows.push(GiRow {
            scene: scene.name.to_owned(),
            technique: name.to_owned(),
            error: score(plane),
        });
    };

    // One renderer for every row: settle the probes once at the baseline quality, then flip one
    // term per row and read the same number of frames at that settled state. The settle's
    // frames are not scored; they leave the renderer the state a held Desktop High view has
    // (the static-screen cache holding its early composite among it).
    let baseline = FrameQuality {
        gi: GiMode::Probes,
        gi_probes: true,
        screen_space_gi: true,
        ..base_quality()
    };
    let mut r = renderer(scene, true);
    let _ = capture(&mut r, lab, baseline, PROBE_FRAMES);

    // The baseline, the Desktop High default: probes + SSGI, AO on, static-screen reuse on.
    let full = capture(&mut r, lab, baseline, ROW_FRAMES);
    assert!(r.ddgi_active(), "{}: the probes lit the frame", scene.name);
    push("probes_ssgi", &sub_planes(&full, &direct));

    // Probes alone: the probe irradiance and lookup carry the whole row. No screen pass, so
    // nothing reuses.
    let probes_only = FrameQuality {
        screen_space_gi: false,
        ..baseline
    };
    let full = capture(&mut r, lab, probes_only, ROW_FRAMES);
    assert!(r.ddgi_active(), "{}: the probes lit the frame", scene.name);
    push("probes", &sub_planes(&full, &direct));

    // AO off: what the AO term (and its interaction with the probe light) adds. The AO flag is
    // part of the screen cache key, so the held view serves the AO-off composite after the
    // toggle's first frame.
    let no_ao = FrameQuality {
        ssao: SsaoMode::Off,
        ..baseline
    };
    let full = capture(&mut r, lab, no_ao, ROW_FRAMES);
    push("probes_ssgi_no_ao", &sub_planes(&full, &direct));

    // Static-screen reuse off (paged sun shadows off): the row every settled measurement
    // must be able to read without a held composite. Every frame renders in full.
    let no_reuse = FrameQuality {
        paged_sun_shadows: false,
        ..baseline
    };
    let full = capture(&mut r, lab, no_reuse, ROW_FRAMES);
    push("probes_ssgi_no_reuse", &sub_planes(&full, &direct));

    // The cached gather over the same settled probes (hits only, the default). The cache joins
    // the screen key, so the held view serves the cache's composite after the first frame.
    r.set_radiance_cache(true);
    let full = capture(&mut r, lab, baseline, ROW_FRAMES);
    let status = r.radiance_cache_status();
    assert!(
        status.active,
        "{}: the gather read the cache: {status:?}",
        scene.name
    );
    r.set_radiance_cache(false);
    push("probes_ssgi_cache", &sub_planes(&full, &direct));

    // The flat sky term: the floor every probe row must beat. Probes off from here on; no row
    // below needs the settled probe state again.
    r.set_ddgi_enabled(Some(false));
    let ambient_q = FrameQuality {
        gi: GiMode::Ambient,
        gi_probes: false,
        ..base_quality()
    };
    let full = capture(&mut r, lab, ambient_q, ROW_FRAMES);
    push("ambient", &sub_planes(&full, &direct));

    // The screen gather alone over the flat sky term. Flipping the probes off is not part of
    // the screen cache key, so reuse off renders this row's own gather on fresh frames
    // instead of serving the baseline's held composite.
    let ssgi_only = FrameQuality {
        gi: GiMode::ScreenSpace,
        gi_probes: false,
        paged_sun_shadows: false,
        ..baseline
    };
    let full = capture(&mut r, lab, ssgi_only, ROW_FRAMES);
    push("ssgi", &sub_planes(&full, &direct));

    let table = render_table(&rows);
    eprintln!("GI attribution on {}:\n{table}", scene.name);
    if let Some(dir) = regia_core::brand::env_var_os(GI_TABLE_ENV) {
        let dir = std::path::PathBuf::from(dir);
        let path = dir.join(format!("{}_attribution.md", scene.name));
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| write_atomic(&path, table.as_bytes())) {
            eprintln!("{GI_TABLE_ENV}: {e}");
        }
    }
    rows
}

fn row<'a>(rows: &'a [GiRow], technique: &str) -> &'a GiError {
    &rows
        .iter()
        .find(|r| r.technique == technique)
        .unwrap_or_else(|| panic!("no `{technique}` row"))
        .error
}

/// The attribution itself: every probe row must beat the flat sky term, and the switch-off
/// rows must stay close to the baseline (a large move means the attribution found the term;
/// the lane records it in the report and tightens these bounds to what it measured).
fn assert_attribution_holds(rows: &[GiRow], scene: &str) {
    let ambient = row(rows, "ambient");
    for technique in ["probes_ssgi", "probes", "probes_ssgi_cache"] {
        let r = row(rows, technique);
        assert!(
            r.flip_mean < ambient.flip_mean,
            "{scene}: {technique} (FLIP {}) must beat the flat sky term ({})",
            r.flip_mean,
            ambient.flip_mean
        );
    }
    // The switch-off rows must stay close to the baseline: a large move means the switch-off
    // found the term, so the attribution this harness reports changed. The bound sits just
    // above the largest move the settled rows measured on the reference lavapipe (the colour
    // room's AO row, 0.047 FLIP over the held-composite baseline; most rows move less than
    // 0.035, and the settled-vs-settled term effects are at most 0.012). The colour-room moves
    // against the held baseline are the static-screen cache artifact the harness doc records,
    // not term effects. See the table the test prints.
    let base = row(rows, "probes_ssgi");
    for technique in [
        "probes",
        "probes_ssgi_no_ao",
        "probes_ssgi_no_reuse",
        "probes_ssgi_cache",
    ] {
        let r = row(rows, technique);
        assert!(
            (r.flip_mean - base.flip_mean).abs() <= 0.05,
            "{scene}: {technique} moves the score by {} (baseline {}): the attribution changed",
            r.flip_mean,
            base.flip_mean
        );
    }
}

/// One harness truth for the sealed box (`specs/27` § 2.2): the leak row every harness
/// reports, with the static-screen cache on (the Desktop High default) and off, at the
/// settled frame count `gi_validation` reads. The spec 27 suite harvests its leak row from
/// `gi_validation`'s log, so the two harnesses can only disagree through settings, frame
/// count or the held-screen cache; this pins all three and fails above the row's own bound
/// (0.5 % of the lit ground; the pre-layout layouts read 3.87 %, so a regression halfway
/// back must not pass).
#[test]
#[ignore = "measurement: prints the sealed-box leak with the cache on and off"]
fn sealed_box_leak_with_and_without_the_static_screen_cache() {
    let scene = sealed_box();
    let scale = scene.scale.expect("the leak scene names its scale");
    let image = reference(&scene);
    let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    let reference_indirect = add_planes(&image.indirect_diffuse, &image.indirect_specular);
    let direct = realtime_direct(&scene);
    let score = |test: &[[f32; 3]]| -> GiError {
        measure(&GiErrorInput {
            reference: reference_indirect.as_slice(),
            test,
            surface: &surface,
            shadow: &shadow,
            width: GI_WIDTH,
            height: GI_HEIGHT,
            ev100: scene.lab.exposure_ev,
            scale: scene.scale,
        })
    };
    let quality = FrameQuality {
        gi: GiMode::Probes,
        gi_probes: true,
        screen_space_gi: true,
        ..base_quality()
    };
    let leak = |name: &str, plane: &[[f32; 3]]| -> f32 {
        let error = score(plane);
        let leak = error.mean_test / scale;
        eprintln!(
            "sealed_box / {name}: leak {:.4} % of the lit ground (§ 2.2 bound 0.5 %: {})",
            leak * 100.0,
            if leak <= 0.005 { "met" } else { "not met" }
        );
        leak
    };
    // Cache on (paged sun shadows, the Desktop High default).
    let mut r = renderer(&scene, true);
    let full = capture(&mut r, &scene.lab, quality, PROBE_FRAMES + SSGI_FRAMES);
    let on = leak("probes_ssgi", &sub_planes(&full, &direct));
    // Cache off: every frame renders in full.
    let no_reuse = FrameQuality {
        paged_sun_shadows: false,
        ..quality
    };
    let mut r = renderer(&scene, true);
    let full = capture(&mut r, &scene.lab, no_reuse, PROBE_FRAMES + SSGI_FRAMES);
    let off = leak("probes_ssgi_no_reuse", &sub_planes(&full, &direct));
    // Probes alone settle the same way without the screen pass.
    let probes_only = FrameQuality {
        screen_space_gi: false,
        ..quality
    };
    let mut r = renderer(&scene, true);
    let full = capture(&mut r, &scene.lab, probes_only, PROBE_FRAMES + STILL_FRAMES);
    let probes = leak("probes", &sub_planes(&full, &direct));
    for (name, leak) in [("cache on", on), ("cache off", off), ("probes", probes)] {
        assert!(
            leak <= 0.005,
            "sealed_box: {name} leaks {:.4} % of the lit ground, above the § 2.2 bound (0.5 %)",
            leak * 100.0
        );
    }
}

#[test]
#[ignore = "measurement: prints the § 2.2 GI plate attribution table"]
fn attribute_the_trench_plate_miss() {
    let rows = attribute(&trench());
    assert_attribution_holds(&rows, "trench");
}

#[test]
#[ignore = "measurement: prints the § 2.2 GI plate attribution table"]
fn attribute_the_color_room_plate_miss() {
    let rows = attribute(&color_room());
    assert_attribution_holds(&rows, "color_room");
}
