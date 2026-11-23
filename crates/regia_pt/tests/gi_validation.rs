// SPDX-License-Identifier: MPL-2.0
//! GI validation (ADR-423, `specs/27` § 2.2 GI rows): every real-time diffuse GI technique
//! against the path tracer's indirect light on the standard GI scenes
//! (`regia_test_utils::gi_scenes`: trench, colour room, sealed box).
//!
//! Per scene:
//!
//! - the reference: the path tracer at 1024 spp (pixel centres, 6 bounces), its
//!   `indirect_diffuse + indirect_specular` AOVs (the sky and every bounce after the first hit);
//! - the frame's indirect light: the full frame minus a direct-only frame (sun only: no sky, no
//!   GI, no AO) of the same renderer settings, signed (not clamped);
//! - techniques: `ambient` (the flat sky term with AO, the balanced look in a lab scene), `ssgi`
//!   (screen-space GI), `probes` (GPU-traced irradiance probes after a fixed number of frames),
//!   `probes_ssgi` (both, the Desktop High default), `baked` (a volume baked by `regia_pt::probe`
//!   from the same scene, Desktop High with a baked map); plus `direct`, the direct-only frame
//!   against the tracer's `direct` AOV (the plumbing floor every other row stands on);
//! - per technique ([`regia_test_utils::gi_error`]): RMSE (absolute and relative), the bias in
//!   sun shadow, the energy ratio and mean FLIP, over the pixels that see a surface away from
//!   silhouettes.
//!
//! The table is printed (and written with the images to `$REGIA_GI_TABLE/<scene>.md` when
//! set). The test fails when a technique's error grows past its record in
//! `crates/regia_test_utils/gi_thresholds/<scene>.txt` (`GiTolerance::REFERENCE` on lavapipe,
//! `GiTolerance::OTHER_ADAPTER` on hardware GPUs). `REGIA_GI_RECORD=1` on lavapipe writes the
//! records instead (say why in the commit message, as for re-blessed goldens). The § 2.2 targets
//! (FLIP ≤ 0.08; leak ≤ 0.5 %) are printed as met or not met, not asserted: the records are the
//! regression gate.
//!
//! Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI).

use std::sync::{Arc, OnceLock};

use regia_asset::baked_gi::{BakedGiVolume, VolumeLayout};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume, detail_brick};
use regia_pt::{PathTracer, PtImage, PtScene, PtSettings};
use regia_render::{
    FrameQuality, FrameRenderer, GiMode, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode,
};
use regia_test_utils::gi_error::{
    GI_TABLE_ENV, GiError, GiErrorInput, GiRecord, GiRow, GiTolerance, add_planes, gi_record_mode,
    gi_thresholds_path, improvements, measure, read_records, regressions, render_table, shadow_mask,
    sub_planes, surface_mask, write_records,
};
use regia_test_utils::gi_scenes::{GI_HEIGHT, GI_WIDTH, GiScene, color_room, sealed_box, trench};
use regia_test_utils::golden::{GoldenIcd, golden_icd, goldens_skipped};

/// Frames the dynamic probes run before the capture. They re-trace every frame at hysteresis 0.9; a bounce
/// chain settles by about 64 frames
/// (the color room reads 1.033 at 32, 1.062 at 64, 1.064 at 256; `gi_radiance_cache` `sweep_the_gather`).
const PROBE_FRAMES: u32 = 64;
/// Frames of the screen-space pass (its temporal filter settles in a few).
const SSGI_FRAMES: u32 = 4;
/// Frames of the techniques without history.
const STILL_FRAMES: u32 = 2;
/// § 2.2 GI (diffuse) mean FLIP target.
const FLIP_TARGET: f32 = 0.08;
/// § 2.2 GI (diffuse) p95 FLIP target (ADR-2951).
const FLIP_P95_TARGET: f32 = 0.20;
/// § 2.2 leak target: interior luminance over the lit exterior ground's.
const LEAK_TARGET: f32 = 0.005;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("GI validation needs a GPU; lavapipe is a GPU here"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

/// Desktop High as the lab runs it: native scale without temporal upscaling, no fog or clouds,
/// no reflections (the scenes are rough), the authored vertical field of view (the tracer's
/// camera), and every GI technique off; each technique turns its own on.
fn base_quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    // The probes and the bake reach the shading through the single-sample depth prepass.
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

fn baked_quality() -> FrameQuality {
    FrameQuality {
        baked_gi: true,
        ..probes_quality(true)
    }
}

fn reference(scene: &GiScene) -> (PtScene, PtImage) {
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
    (pt, image)
}

fn bake(scene: &GiScene, pt: &PtScene) -> Arc<BakedGiVolume> {
    let (lo, hi) = scene.bake_bounds;
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![detail_brick(lo, hi, scene.bake_spacing)],
        moment_resolution: 8,
        max_distance: 16.0,
    };
    layout.assign_brick_offsets();
    let settings = GiBakeSettings {
        trace: ProbeTraceSettings {
            rays_per_probe: 128,
            samples: 8,
            max_bounces: 4,
            seed: 7,
            samples_per_submit: 8,
            ..ProbeTraceSettings::default()
        },
        ..GiBakeSettings::default()
    };
    let bake = bake_gi_volume(tracer(), pt, &layout, &settings, |_, _, _| true).expect("bake");
    eprintln!("{}: baked {:?}", scene.name, bake.report);
    assert_eq!(bake.report.rejected_rays, 0);
    Arc::new(bake.volume)
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

/// The indirect light of every technique: `(name, full − direct)`.
fn realtime_indirect(
    scene: &GiScene,
    direct: &[[f32; 3]],
    volume: Arc<BakedGiVolume>,
) -> Vec<(&'static str, Vec<[f32; 3]>)> {
    let lab = &scene.lab;
    let mut out = Vec::new();

    let full = capture(&mut renderer(scene, false), lab, base_quality(), STILL_FRAMES);
    out.push(("ambient", sub_planes(&full, direct)));

    let full = capture(&mut renderer(scene, false), lab, ssgi_quality(), SSGI_FRAMES);
    out.push(("ssgi", sub_planes(&full, direct)));

    // One renderer for both probe rows: the screen-space pass joins probes that have settled.
    let mut probes = renderer(scene, true);
    let full = capture(&mut probes, lab, probes_quality(false), PROBE_FRAMES);
    assert!(probes.ddgi_active(), "{}: the probes lit the frame", scene.name);
    out.push(("probes", sub_planes(&full, direct)));
    let full = capture(&mut probes, lab, probes_quality(true), SSGI_FRAMES);
    assert!(probes.ddgi_active(), "{}: the probes lit the frame", scene.name);
    out.push(("probes_ssgi", sub_planes(&full, direct)));

    let mut baked = renderer(scene, true);
    baked.set_baked_gi(Some(volume));
    let full = capture(&mut baked, lab, baked_quality(), STILL_FRAMES);
    assert!(
        baked.baked_gi_status().active,
        "{}: the bake lit the frame",
        scene.name
    );
    assert!(!baked.ddgi_active(), "{}: no probes on a baked frame", scene.name);
    out.push(("baked", sub_planes(&full, direct)));
    out
}

/// sRGB-encodes `img` at `ev100` + 2 EV (indirect light is dim) for review by eye.
fn encode(img: &[[f32; 3]], ev100: f32) -> Vec<u8> {
    let exposure = 4.0 / (1.2 * 2f32.powf(ev100));
    let e = |v: f32| {
        let v = (v * exposure).clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0 + 0.5) as u8
    };
    img.iter().flat_map(|c| c.map(e)).collect()
}

/// With `$REGIA_GI_TABLE` set: `<scene>.md` (the table) and `<scene>.png` (the reference and
/// every technique side by side, left to right in table order).
fn write_review(scene: &GiScene, table: &str, planes: &[(&str, &[[f32; 3]])]) {
    let Some(dir) = regia_core::brand::env_var_os(GI_TABLE_ENV) else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("{GI_TABLE_ENV}: {e}");
        return;
    }
    let names: Vec<&str> = planes.iter().map(|(n, _)| *n).collect();
    let md = format!(
        "# GI validation: {}\n\nImage `{}.png`, left to right: {}.\n\n{table}",
        scene.name,
        scene.name,
        names.join(", ")
    );
    if let Err(e) = regia_test_utils::write_atomic(&dir.join(format!("{}.md", scene.name)), md.as_bytes()) {
        eprintln!("{GI_TABLE_ENV}: {e}");
    }
    let encoded: Vec<Vec<u8>> = planes
        .iter()
        .map(|(_, p)| encode(p, scene.lab.exposure_ev))
        .collect();
    let width = GI_WIDTH * planes.len() as u32;
    let row = (GI_WIDTH * 3) as usize;
    let mut rgb = Vec::with_capacity(row * planes.len() * GI_HEIGHT as usize);
    for y in 0..GI_HEIGHT as usize {
        for e in &encoded {
            rgb.extend_from_slice(&e[y * row..(y + 1) * row]);
        }
    }
    let mut png_bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut png_bytes, width, GI_HEIGHT);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let written = encoder.write_header().and_then(|mut w| w.write_image_data(&rgb));
    match written {
        Ok(()) => {
            if let Err(e) =
                regia_test_utils::write_atomic(&dir.join(format!("{}.png", scene.name)), &png_bytes)
            {
                eprintln!("{GI_TABLE_ENV}: {e}");
            }
        }
        Err(e) => eprintln!("{GI_TABLE_ENV}: {e}"),
    }
}

fn adapter_label() -> String {
    let info = device().adapter.get_info();
    format!("{} ({}; {})", info.name, info.driver, info.driver_info)
}

/// Prints the § 2.2 target status of each technique row (not asserted).
fn print_targets(scene: &GiScene, rows: &[GiRow]) {
    for r in rows.iter().filter(|r| r.technique != "direct") {
        if scene.scale.is_some() {
            let leak = r.error.mean_test / r.error.scale;
            eprintln!(
                "{} / {}: leak {:.4} % of the lit ground (§ 2.2 target {:.1} %: {})",
                r.scene,
                r.technique,
                leak * 100.0,
                LEAK_TARGET * 100.0,
                if leak <= LEAK_TARGET { "met" } else { "not met" }
            );
        } else {
            eprintln!(
                "{} / {}: FLIP {:.3} (§ 2.2 target {FLIP_TARGET}: {})",
                r.scene,
                r.technique,
                r.error.flip_mean,
                if r.error.flip_mean <= FLIP_TARGET {
                    "met"
                } else {
                    "not met"
                }
            );
            // The § 2.2 p95 row (ADR-2951), harvested by `spec27_calibration`.
            eprintln!(
                "{} / {}: p95 FLIP {:.3} (§ 2.2 target {FLIP_P95_TARGET}: {})",
                r.scene,
                r.technique,
                r.error.flip_p95,
                if r.error.flip_p95 <= FLIP_P95_TARGET {
                    "met"
                } else {
                    "not met"
                }
            );
        }
    }
}

/// Records the rows (record mode, lavapipe only) or checks them against the record. The real-GPU
/// non-golden pass (`REGIA_SKIP_GOLDENS=1` on a hardware GPU) reports regressions without
/// failing: the records are lavapipe records, like the goldens that pass skips.
fn check_or_record(scene: &GiScene, rows: &[GiRow]) {
    let info = device().adapter.get_info();
    let path = gi_thresholds_path(scene.name);
    let report_only = goldens_skipped() && matches!(golden_icd(&info), GoldenIcd::Hardware);
    if gi_record_mode() {
        assert!(
            matches!(golden_icd(&info), GoldenIcd::Lavapipe { .. }),
            "GI records are taken on lavapipe (the gate's reference adapter), not on {}",
            adapter_label()
        );
        let records: Vec<GiRecord> = rows
            .iter()
            .map(|r| GiRecord::from_error(&r.scene, &r.technique, &r.error))
            .collect();
        write_records(&path, &records, &adapter_label()).expect("write the GI record");
        eprintln!("recorded {} GI rows to {}", records.len(), path.display());
        return;
    }
    let records = match read_records(&path) {
        Ok(records) => records,
        Err(e) if report_only => {
            eprintln!(
                "GI record for `{}` not read ({e}); real-GPU non-golden pass",
                scene.name
            );
            return;
        }
        Err(e) => panic!(
            "no GI record for `{}` ({e}): run this test once with REGIA_GI_RECORD=1 on lavapipe and commit {}",
            scene.name,
            path.display()
        ),
    };
    let tolerance = GiTolerance::for_adapter(&info);
    let mut failures = Vec::new();
    for row in rows {
        let Some(record) = records
            .iter()
            .find(|r| r.scene == row.scene && r.technique == row.technique)
        else {
            failures.push(format!(
                "{} / {}: no record in {}; re-record",
                row.scene,
                row.technique,
                path.display()
            ));
            continue;
        };
        failures.extend(regressions(&row.error, record, tolerance));
        for note in improvements(&row.error, record, tolerance) {
            eprintln!("{note}");
        }
    }
    if report_only {
        for f in &failures {
            eprintln!("GI regression (reported, not failed on the real-GPU non-golden pass): {f}");
        }
        return;
    }
    assert!(
        failures.is_empty(),
        "GI error regressed on `{}` ({}, tolerance {tolerance:?}):\n{}",
        scene.name,
        adapter_label(),
        failures.join("\n")
    );
}

/// Scores every technique on `scene`, prints and writes the table, then checks or records it.
/// Returns the rows for the scene's own assertions.
fn validate(scene: &GiScene) -> Vec<GiRow> {
    let (pt, image) = reference(scene);
    let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    let surface_pixels = surface.iter().filter(|s| **s).count();
    assert!(
        surface_pixels * 10 >= surface.len() * 3,
        "{}: at least 30 % of the frame is scored, got {surface_pixels}",
        scene.name
    );
    let reference_indirect = add_planes(&image.indirect_diffuse, &image.indirect_specular);
    let direct = realtime_direct(scene);
    let volume = bake(scene, &pt);
    let techniques = realtime_indirect(scene, &direct, volume);

    let score = |reference: &[[f32; 3]], test: &[[f32; 3]]| -> GiError {
        measure(&GiErrorInput {
            reference,
            test,
            surface: &surface,
            shadow: &shadow,
            width: GI_WIDTH,
            height: GI_HEIGHT,
            ev100: scene.lab.exposure_ev,
            scale: scene.scale,
        })
    };
    let mut rows = vec![GiRow {
        scene: scene.name.to_owned(),
        technique: "direct".to_owned(),
        error: score(image.direct.as_slice(), direct.as_slice()),
    }];
    for (name, plane) in &techniques {
        rows.push(GiRow {
            scene: scene.name.to_owned(),
            technique: (*name).to_owned(),
            error: score(reference_indirect.as_slice(), plane.as_slice()),
        });
    }
    let table = render_table(&rows);
    eprintln!("GI validation on {}:\n{table}", adapter_label());
    print_targets(scene, &rows);
    let mut planes: Vec<(&str, &[[f32; 3]])> = vec![("reference", reference_indirect.as_slice())];
    planes.extend(techniques.iter().map(|(n, p)| (*n, p.as_slice())));
    write_review(scene, &table, &planes);
    for r in &rows {
        assert!(
            r.error.pixels == surface_pixels && r.error.flip_mean.is_finite(),
            "{} / {}: {:?}",
            r.scene,
            r.technique,
            r.error
        );
    }
    check_or_record(scene, &rows);
    rows
}

fn row<'a>(rows: &'a [GiRow], technique: &str) -> &'a GiError {
    &rows
        .iter()
        .find(|r| r.technique == technique)
        .unwrap_or_else(|| panic!("no `{technique}` row"))
        .error
}

#[test]
fn trench_gi_error_stays_within_its_record() {
    let scene = trench();
    let rows = validate(&scene);
    // The sunlit wall bounces onto a floor in shadow: the reference has indirect light there.
    let direct = row(&rows, "direct");
    assert!(direct.shadow_pixels > 500, "{direct:?}");
    let ambient = row(&rows, "ambient");
    assert!(ambient.mean_reference > 0.0 && ambient.energy_ratio.is_some());
}

#[test]
fn color_room_gi_error_stays_within_its_record() {
    let scene = color_room();
    let rows = validate(&scene);
    // The ceiling and the back corners are lit only indirectly.
    let ambient = row(&rows, "ambient");
    assert!(ambient.shadow_pixels > 500, "{ambient:?}");
    assert!(ambient.mean_reference > 0.0 && ambient.shadow_bias.is_some());
}

#[test]
fn sealed_box_leak_stays_within_its_record() {
    let scene = sealed_box();
    let rows = validate(&scene);
    let scale = scene.scale.expect("the leak scene names its scale");
    // The box is closed: the tracer sees no light inside (only its own noise floor).
    let direct = row(&rows, "direct");
    assert!(
        direct.mean_reference <= 1e-4 * scale,
        "the reference sees light inside the sealed box: {direct:?}"
    );
    let ambient = row(&rows, "ambient");
    assert!(
        ambient.mean_reference <= 1e-4 * scale && ambient.energy_ratio.is_none(),
        "the reference sees indirect light inside the sealed box: {ambient:?}"
    );
    // Every pixel inside is in shadow.
    assert_eq!(ambient.shadow_pixels, ambient.pixels);
}

/// The probe rows score the probes only where the camera's pixels read them. A pixel outside every
/// cascade's full-weight window mixes in the flat sky term instead (`exterior_diffuse`), which reads
/// bright under the sky and black under the sun: the plates' first layouts stopped at 34, 57 and 78 %
/// of what the camera sees, and that, not the probes' light, was the 41 and 25 % deficit and the sealed
/// box's 3.9 % leak (ADR-2605, `specs/79` § 12).
#[test]
fn the_plate_layouts_hold_what_the_camera_sees() {
    /// Least mean coverage of a facing's pixels. Not 1: the lookup is half resolution, so a pixel at a
    /// silhouette may read its neighbor's texel.
    const MIN_COVERAGE: f32 = 0.98;
    const FRAMES: u32 = 8;
    const AXES: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let mut failures = Vec::new();
    for scene in [trench(), color_room(), sealed_box()] {
        let frame = scene.lab.as_frame_scene_with_quality(base_quality());
        let pt = from_frame(&frame, &ExtractOptions::default());
        let image = tracer()
            .render(
                &pt,
                &PtSettings {
                    spp: 1,
                    max_bounces: 1,
                    seed: 23,
                    jitter: false,
                    ..PtSettings::default()
                },
            )
            .expect("trace the plate's depth and normals");
        let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
        let mut r = renderer(&scene, true);
        capture(&mut r, &scene.lab, probes_quality(false), FRAMES);
        let rb = r
            .ddgi_read_back(device())
            .expect("read back")
            .expect("the probes ran");
        let (tw, th, texels) = rb.screen_irradiance().expect("the screen lookup ran");
        let (w, h) = (GI_WIDTH as usize, GI_HEIGHT as usize);
        let (tw, th) = (tw as usize, th as usize);
        let coverage_at = |i: usize| {
            let (x, y) = (i % w, i / w);
            texels[(y * th / h).min(th - 1) * tw + (x * tw / w).min(tw - 1)]
                .coverage
                .clamp(0.0, 1.0)
        };
        for facing in 0..6 {
            let axis = AXES[facing / 2];
            let sign = if facing % 2 == 0 { 1.0 } else { -1.0 };
            let pixels: Vec<usize> = (0..w * h)
                .filter(|&i| {
                    let n = image.normal[i];
                    surface[i] && sign * (n[0] * axis[0] + n[1] * axis[1] + n[2] * axis[2]) > 0.9
                })
                .collect();
            if pixels.is_empty() {
                continue;
            }
            let mean = pixels.iter().map(|&i| coverage_at(i)).sum::<f32>() / pixels.len() as f32;
            eprintln!(
                "{} {:?} facing {sign} {axis:?}: {} pixels, coverage {mean:.3}",
                scene.name,
                scene.ddgi.dims,
                pixels.len()
            );
            if mean < MIN_COVERAGE {
                failures.push(format!(
                    "{} facing {sign} {axis:?}: coverage {mean:.3}",
                    scene.name
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "a plate's probe volumes stop short of what its camera sees:\n{}",
        failures.join("\n")
    );
}
