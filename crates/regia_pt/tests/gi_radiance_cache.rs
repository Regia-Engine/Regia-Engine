// SPDX-License-Identifier: MPL-2.0
//! The radiance cache against the path tracer (ADR-2085, `specs/79` § 12): the plates of
//! `gi_validation.rs` with the cache on, next to the frames it stands beside.
//!
//! On the standard scenes the cached gather must stay within a tolerance of the path tracer as measured
//! for the plain gather over the same probes (`probes_ssgi`), and it must not bring light into the
//! sealed box: this measures one more technique live, without a record of its own in `gi_thresholds`
//! (those are the lead's to re-record). So must the economy gather, under temporal AA, which it relies on.
//!
//! `REGIA_GI_TABLE=<dir>` also writes `<scene>_radiance_cache.md` for review; the table is printed.

use std::sync::OnceLock;

use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtImage, PtSettings};
use regia_render::{
    FrameQuality, FrameRenderer, GiMode, RadianceCacheSettings, RadianceCacheTerm, RenderDevice,
    RenderProfile, RenderProfilePreset, SsaoMode,
};
use regia_test_utils::gi_error::{
    GI_TABLE_ENV, GiError, GiErrorInput, GiRow, add_planes, luminance, measure, render_table, shadow_mask,
    sub_planes, surface_mask,
};
use regia_test_utils::gi_scenes::{GI_HEIGHT, GI_WIDTH, GiScene, color_room, sealed_box, trench};
use regia_test_utils::golden::{GoldenIcd, golden_icd};

/// Frames before the first row is read. The plates' probes re-trace every frame at hysteresis 0.9: every row
/// must read the same probes, or a later row scores the probes' age and not the gather. Until ADR-5506 a
/// bounce chain settled by about 64 (on the color room 1.033 at 32, 1.062 at 64, 1.064 at 256). Since the
/// plates have the tiers' lookup, a probe's surface texels lean on their surface from its 16th to its 32nd
/// update and the bounces follow that: on lavapipe the trench reads FLIP 0.066 at 64 frames, 0.072 at 128,
/// 0.068 at 160 and 0.071 at 192 and 256 (energy 0.988, 0.995, 0.991, 0.990, 0.992). At 64 the cached row, read
/// 66 frames after the plain row, scored the probes' settling (0.0648 against 0.0588). 256 is the count the
/// sweep below already lets the probes run ([`SWEEP_SETTLE_FRAMES`]).
const PROBE_FRAMES: u32 = 256;
const GATHER_FRAMES: u32 = 64;
const STILL_FRAMES: u32 = 2;
/// Frames the sweep lets the probes run before it reads a gather.
const SWEEP_SETTLE_FRAMES: u32 = 256;
/// § 2.2 leak target: interior luminance over the lit exterior ground's.
const LEAK_TARGET: f32 = 0.005;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("this test needs a GPU; lavapipe is a GPU here"))
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

fn probes_quality(ssgi: bool) -> FrameQuality {
    FrameQuality {
        gi: GiMode::Probes,
        gi_probes: true,
        screen_space_gi: ssgi,
        ..base_quality()
    }
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

fn renderer(scene: &GiScene, probes: bool) -> FrameRenderer {
    let mut r = FrameRenderer::new();
    r.set_ddgi_settings(scene.ddgi.clone());
    r.set_ddgi_enabled(Some(probes));
    r
}

/// Renders `frames` frames and returns the last `average` frames' mean linear HDR: the gather's
/// noise pattern moves per frame only under temporal AA, which the lab frames leave off.
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

fn realtime_direct(scene: &GiScene) -> Vec<[f32; 3]> {
    let mut lab = scene.lab.clone();
    lab.ambient_lux = 0.0;
    let quality = FrameQuality {
        ssao: SsaoMode::Off,
        ..base_quality()
    };
    capture(&mut renderer(scene, false), &lab, quality, STILL_FRAMES)
}

/// The default row's frame history (ADR-4000: every compared mode reads the same probe and cell ages;
/// a sequential capture scores the probes' settling, not the gather), with the last frame set up by
/// `last`, which returns that frame's quality. The cached row's frame count is the default row's.
fn matched_last_frame(
    scene: &GiScene,
    last: impl FnOnce(&mut FrameRenderer) -> FrameQuality,
) -> Vec<[f32; 3]> {
    let mut r = renderer(scene, true);
    capture(&mut r, &scene.lab, probes_quality(false), PROBE_FRAMES);
    capture(&mut r, &scene.lab, probes_quality(true), STILL_FRAMES);
    r.set_radiance_cache(true);
    capture(&mut r, &scene.lab, probes_quality(true), GATHER_FRAMES - 1);
    let quality = last(&mut r);
    capture(&mut r, &scene.lab, quality, 1)
}

/// The last frame of the default history with the gather writing `term` (ADR-4106).
fn matched_term(scene: &GiScene, term: RadianceCacheTerm, quality: FrameQuality) -> Vec<[f32; 3]> {
    matched_last_frame(scene, |r| {
        r.set_radiance_cache_settings(RadianceCacheSettings {
            attribution: term,
            ..RadianceCacheSettings::default()
        });
        quality
    })
}

/// One frame of the trench split into the cached gather's terms (ADR-4106), every plane the last frame
/// of the same history, with the tracer's planes for the same pixels.
struct Attribution {
    /// The tracer's indirect light, its first-hit albedo and normal.
    reference: Vec<[f32; 3]>,
    albedo: Vec<[f32; 3]>,
    normal: Vec<[f32; 3]>,
    shadow: Vec<bool>,
    surface: Vec<bool>,
    /// The real-time direct light (`realtime_direct`), subtracted from every frame.
    direct: Vec<[f32; 3]>,
    /// The tracer's direct light at the first hit.
    tracer_direct: Vec<[f32; 3]>,
    /// Captures by name: `shipped` (the default image), `total` (it, gathered afresh), `plain` (the
    /// gather without the cache), `probes` (no gather), `base_no_ao`, and each term's capture.
    planes: Vec<(&'static str, Vec<[f32; 3]>)>,
}

impl Attribution {
    fn plane(&self, name: &str) -> &[[f32; 3]] {
        &self
            .planes
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("no `{name}` plane"))
            .1
    }

    /// `a − b` per pixel.
    fn minus(&self, a: &str, b: &str) -> Vec<[f32; 3]> {
        sub_planes(self.plane(a), self.plane(b))
    }

    /// A term: its capture minus the base capture.
    fn term(&self, term: RadianceCacheTerm) -> Vec<[f32; 3]> {
        self.minus(term.name(), RadianceCacheTerm::Base.name())
    }
}

fn attribute(scene: &GiScene) -> Attribution {
    let image = reference(scene);
    let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    let reference = add_planes(&image.indirect_diffuse, &image.indirect_specular);
    let direct = realtime_direct(scene);
    let mut planes = vec![("shipped", matched_last_frame(scene, |_| probes_quality(true)))];
    for term in RadianceCacheTerm::ALL.into_iter().skip(1) {
        planes.push((term.name(), matched_term(scene, term, probes_quality(true))));
    }
    planes.push((
        "base_no_ao",
        matched_term(
            scene,
            RadianceCacheTerm::Base,
            FrameQuality {
                ssao: SsaoMode::Off,
                ..probes_quality(true)
            },
        ),
    ));
    planes.push((
        "plain",
        matched_last_frame(scene, |r| {
            r.set_radiance_cache(false);
            probes_quality(true)
        }),
    ));
    planes.push((
        "probes",
        matched_last_frame(scene, |r| {
            r.set_radiance_cache(false);
            probes_quality(false)
        }),
    ));
    // Controls on the probe field itself (no gather): four times the frames, and probes at half the
    // spacing over the same volume. If the deficit moves with these, it is the probes'.
    let frames = PROBE_FRAMES + STILL_FRAMES + GATHER_FRAMES;
    planes.push((
        "probes_aged",
        capture(
            &mut renderer(scene, true),
            &scene.lab,
            probes_quality(false),
            4 * frames,
        ),
    ));
    // 40 x 16 x 16 at half the spacing (20 x 8 x 8 m around the eye in the finest cascade) stays
    // inside a 128 MiB storage binding; twice every axis does not.
    let mut dense = scene.clone();
    dense.ddgi.spacing *= 0.5;
    dense.ddgi.dims = [40, 16, 16];
    planes.push((
        "probes_dense",
        capture(
            &mut renderer(&dense, true),
            &dense.lab,
            probes_quality(false),
            frames,
        ),
    ));
    // The cached gather over the denser probes (its cells follow the probe spacing).
    planes.push((
        "cache_dense",
        matched_last_frame(&dense, |_| probes_quality(true)),
    ));
    Attribution {
        reference,
        albedo: image.albedo,
        normal: image.normal,
        tracer_direct: image.direct,
        shadow,
        surface,
        direct,
        planes,
    }
}

/// Mean luminance of `plane` over `mask`.
fn masked_luminance(plane: &[[f32; 3]], mask: &[bool]) -> f64 {
    let (sum, n) = plane
        .iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .fold((0.0f64, 0usize), |(s, n), (c, _)| {
            (s + f64::from(luminance(*c)), n + 1)
        });
    sum / n.max(1) as f64
}

/// `plane` with each pixel's colour times the tracer's albedo over the gather's nominal one (0.35,
/// `ssgi.wgsl` `RECEIVER_ALBEDO`): what the term would be at the receiver's real albedo.
fn at_true_albedo(plane: &[[f32; 3]], albedo: &[[f32; 3]]) -> Vec<[f32; 3]> {
    plane
        .iter()
        .zip(albedo)
        .map(|(c, a)| [c[0] * a[0] / 0.35, c[1] * a[1] / 0.35, c[2] * a[2] / 0.35])
        .collect()
}

/// The rows of `scene`: the plain probe frame, the plain gather over it, and the cached gather (hits
/// only, the default), signed and with its miss read.
fn validate(scene: &GiScene) -> Vec<GiRow> {
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
    let mut r = renderer(scene, true);
    let full = capture(&mut r, lab, probes_quality(false), PROBE_FRAMES);
    push("probes", &sub_planes(&full, &direct));
    let full = capture(&mut r, lab, probes_quality(true), STILL_FRAMES);
    push("probes_ssgi", &sub_planes(&full, &direct));
    // The cached gather on the same settled probes, filled at the default budget.
    r.set_radiance_cache(true);
    let full = capture(&mut r, lab, probes_quality(true), GATHER_FRAMES);
    let status = r.radiance_cache_status();
    assert!(
        status.active,
        "{}: the gather read the cache: {status:?}",
        scene.name
    );
    assert!(status.total_updated >= status.settings.cells(), "{status:?}");
    push("probes_cache", &sub_planes(&full, &direct));
    // Signed, so the clamp hides nothing: what walking along a ray does to the field on its own.
    r.set_radiance_cache_settings(RadianceCacheSettings {
        darken_limit: 1.0,
        ..r.radiance_cache_settings()
    });
    let full = capture(&mut r, lab, probes_quality(true), GATHER_FRAMES);
    push("cache_signed", &sub_planes(&full, &direct));
    r.set_radiance_cache_settings(RadianceCacheSettings {
        darken_limit: 0.0,
        ..r.radiance_cache_settings()
    });
    r.set_radiance_cache_settings(RadianceCacheSettings {
        miss_cap: 4.0,
        ..r.radiance_cache_settings()
    });
    let full = capture(&mut r, lab, probes_quality(true), GATHER_FRAMES);
    push("cache_miss_read", &sub_planes(&full, &direct));

    let table = render_table(&rows);
    eprintln!("radiance cache on {}:\n{table}", scene.name);
    if let Some(dir) = regia_core::brand::env_var_os(GI_TABLE_ENV) {
        let dir = std::path::PathBuf::from(dir);
        let path = dir.join(format!("{}_radiance_cache.md", scene.name));
        if let Err(e) = std::fs::create_dir_all(&dir)
            .and_then(|()| regia_test_utils::write_atomic(&path, table.as_bytes()))
        {
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

/// How much worse than the plain gather the cached frame may score against the path tracer: relative
/// RMSE and mean FLIP. On lavapipe the frames are deterministic; on a hardware GPU the numbers move as
/// the records do (`GiTolerance::OTHER_ADAPTER`).
fn tolerance() -> (f32, f32) {
    match golden_icd(&device().adapter.get_info()) {
        GoldenIcd::Lavapipe { .. } => (0.02, 0.005),
        _ => (0.10, 0.05),
    }
}

fn assert_within(rows: &[GiRow], scene: &str) {
    assert_row_within(rows, scene, "probes_cache", "probes_ssgi");
}

/// `cached` against `plain`: relative RMSE, FLIP and how far the energy is from the tracer's.
fn assert_row_within(rows: &[GiRow], scene: &str, cached: &str, plain: &str) {
    let (c, p) = (row(rows, cached), row(rows, plain));
    let (rmse, flip) = tolerance();
    assert!(
        c.rel_rmse <= p.rel_rmse + rmse,
        "{scene}: {cached}'s rel. RMSE {} against {plain}'s {}",
        c.rel_rmse,
        p.rel_rmse
    );
    assert!(
        c.flip_mean <= p.flip_mean + flip,
        "{scene}: {cached}'s FLIP {} against {plain}'s {}",
        c.flip_mean,
        p.flip_mean
    );
    if let (Some(ce), Some(pe)) = (c.energy_ratio, p.energy_ratio) {
        assert!(
            (ce - 1.0).abs() <= (pe - 1.0).abs() + rmse,
            "{scene}: {cached}'s energy ratio {ce} against {plain}'s {pe}"
        );
    }
}

/// A miss compares like with like (`specs/79` § 12): signed, the gather stays near the frame it corrects.
/// Comparing a moved read with the pixel's plain read took these plates 17 % and 6 % below it.
fn assert_signed_stays_near_the_probes(rows: &[GiRow], scene: &str) {
    let floor = match golden_icd(&device().adapter.get_info()) {
        GoldenIcd::Lavapipe { .. } => 0.95,
        _ => 0.85,
    };
    let ratio = row(rows, "cache_signed").mean_test / row(rows, "probes").mean_test;
    eprintln!("{scene}: signed mean over the probes' {ratio:.4}");
    assert!(
        ratio >= floor,
        "{scene}: a signed gather darkens to {ratio} of the probes"
    );
}

#[test]
fn the_cached_gather_stays_within_tolerance_of_the_tracer_in_the_trench() {
    let rows = validate(&trench());
    assert_within(&rows, "trench");
    assert_signed_stays_near_the_probes(&rows, "trench");
}

#[test]
fn the_cached_gather_stays_within_tolerance_of_the_tracer_in_the_colour_room() {
    let rows = validate(&color_room());
    assert_within(&rows, "color_room");
    assert_signed_stays_near_the_probes(&rows, "color_room");
}

#[test]
fn the_cached_gather_brings_no_light_into_the_sealed_box() {
    let scene = sealed_box();
    let rows = validate(&scene);
    assert_within(&rows, "sealed_box");
    let scale = scene.scale.expect("the leak scene names its scale");
    let probes = row(&rows, "probes");
    let cached = row(&rows, "probes_cache");
    let leak = cached.mean_test / scale;
    let plain = probes.mean_test / scale;
    eprintln!(
        "sealed box leak: probes {:.4} %, with the cache {:.4} % (§ 2.2 bound {:.1} %)",
        plain * 100.0,
        leak * 100.0,
        LEAK_TARGET * 100.0
    );
    // The gather adds no light of its own: at most a hair over the frame it corrects.
    assert!(
        leak <= plain + 0.001,
        "the cached gather raised the leak from {:.4} % to {:.4} %",
        plain * 100.0,
        leak * 100.0
    );
}

/// A contact sheet of signed differences, for review by eye: green adds light, red removes it.
fn write_sheet(name: &str, cells: &[Vec<[f32; 3]>], ev100: f32) {
    let Some(dir) = regia_core::brand::env_var_os(GI_TABLE_ENV) else {
        return;
    };
    const COLUMNS: usize = 4;
    let (w, h) = (GI_WIDTH as usize, GI_HEIGHT as usize);
    let rows = cells.len().div_ceil(COLUMNS);
    let mut rgb = vec![0u8; COLUMNS * w * rows * h * 3];
    let scale = 16.0 / (1.2 * 2f32.powf(ev100));
    for (i, cell) in cells.iter().enumerate() {
        let (cx, cy) = ((i % COLUMNS) * w, (i / COLUMNS) * h);
        for (p, v) in cell.iter().enumerate() {
            let (x, y) = (cx + p % w, cy + p / w);
            let l = (0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2]) * scale;
            let o = (y * COLUMNS * w + x) * 3;
            rgb[o] = ((-l).clamp(0.0, 1.0).sqrt() * 255.0) as u8;
            rgb[o + 1] = (l.clamp(0.0, 1.0).sqrt() * 255.0) as u8;
        }
    }
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, (COLUMNS * w) as u32, (rows * h) as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let written = encoder.write_header().and_then(|mut e| e.write_image_data(&rgb));
    if let Err(e) = written {
        eprintln!("{GI_TABLE_ENV}: {e}");
        return;
    }
    let dir = std::path::PathBuf::from(dir);
    let path = dir.join(format!("{name}_sheet.png"));
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| regia_test_utils::write_atomic(&path, &bytes))
    {
        eprintln!("{GI_TABLE_ENV}: {e}");
    }
}

/// Measurement, not a gate: the gather's tunables on every plate, with a sheet of what each row adds to
/// the plain probe frame, the first cell being what the tracer says is missing
/// (`REGIA_GI_TABLE=<dir> cargo test -p regia_pt --test gi_radiance_cache -- --ignored --nocapture`).
/// The probes first settle for [`SWEEP_SETTLE_FRAMES`] (rows read `probes@n` on the way), and read once
/// more at the end, so a row's score is the gather's and not the probes' age.
#[test]
#[ignore = "measurement: prints a table per plate"]
fn sweep_the_gather() {
    for scene in [trench(), color_room(), sealed_box()] {
        let image = reference(&scene);
        let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
        let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
        let reference_indirect = add_planes(&image.indirect_diffuse, &image.indirect_specular);
        let direct = realtime_direct(&scene);
        let mut rows = Vec::new();
        let mut r = renderer(&scene, true);
        let mut push = |name: String, plane: &[[f32; 3]]| {
            rows.push(GiRow {
                scene: scene.name.to_owned(),
                technique: name,
                error: measure(&GiErrorInput {
                    reference: reference_indirect.as_slice(),
                    test: plane,
                    surface: &surface,
                    shadow: &shadow,
                    width: GI_WIDTH,
                    height: GI_HEIGHT,
                    ev100: scene.lab.exposure_ev,
                    scale: scene.scale,
                }),
            });
        };
        let mut age = 0;
        let mut probes = Vec::new();
        for frames in [32, 32, 64, SWEEP_SETTLE_FRAMES - 128] {
            age += frames;
            let full = capture(&mut r, &scene.lab, probes_quality(false), frames);
            probes = sub_planes(&full, &direct);
            push(format!("probes@{age}"), &probes);
        }
        let mut sheet = vec![sub_planes(&reference_indirect, &probes)];
        let full = capture(&mut r, &scene.lab, probes_quality(true), STILL_FRAMES);
        let plain = sub_planes(&full, &direct);
        push("probes_ssgi".to_owned(), &plain);
        sheet.push(sub_planes(&plain, &probes));
        r.set_radiance_cache(true);
        for (ray_scale, strength, miss_cap, darken_limit) in [
            (3.0f32, 1.0f32, 4.0f32, 0.0f32),
            (3.0, 1.0, 0.0, 0.0),
            (3.0, 1.0, 1.5, 0.0),
            (1.5, 1.0, 4.0, 0.0),
            (1.5, 1.0, 0.0, 0.0),
            (3.0, 0.75, 4.0, 0.0),
            (3.0, 0.75, 0.0, 0.0),
            (3.0, 1.5, 0.0, 0.0),
            (3.0, 1.0, 4.0, 0.25),
            (3.0, 1.0, 0.0, 0.25),
            (3.0, 1.0, 4.0, 1.0),
            (3.0, 1.0, 0.0, 1.0),
        ] {
            r.set_radiance_cache_settings(RadianceCacheSettings {
                ray_scale,
                strength,
                miss_cap,
                darken_limit,
                budget_cells: 16_384,
                ..RadianceCacheSettings::default()
            });
            let full = capture(&mut r, &scene.lab, probes_quality(true), 12);
            let plane = sub_planes(&full, &direct);
            push(
                format!("ray{ray_scale} str{strength} miss{miss_cap} dark{darken_limit}"),
                &plane,
            );
            sheet.push(sub_planes(&plane, &probes));
        }
        // The cheaper gather, and the default budget's slow fill, on the same probes.
        r.set_radiance_cache_settings(RadianceCacheSettings {
            budget_cells: 16_384,
            ..RadianceCacheSettings::economy()
        });
        let full = capture(&mut r, &scene.lab, probes_quality(true), 12);
        push("economy".to_owned(), &sub_planes(&full, &direct));
        r.set_radiance_cache_settings(RadianceCacheSettings::default());
        let full = capture(&mut r, &scene.lab, probes_quality(true), GATHER_FRAMES);
        push("default budget".to_owned(), &sub_planes(&full, &direct));
        r.set_radiance_cache(false);
        let full = capture(&mut r, &scene.lab, probes_quality(false), STILL_FRAMES);
        push("probes at the end".to_owned(), &sub_planes(&full, &direct));
        eprintln!("sweep on {}:\n{}", scene.name, render_table(&rows));
        write_sheet(scene.name, &sheet, scene.lab.exposure_ev);
    }
}

/// Frames of a temporal AA row: the composite is held from `HELD_CONVERGE_FRAMES` of a still view.
const TAA_FRAMES: u32 = 40;

/// The cells' budget of a temporal AA row: they fill within a few frames, so the row's frames are the
/// gather's and the resolve's.
const TAA_BUDGET: u32 = 16_384;

/// `scene`'s rows under temporal AA: the plain probe frame, the plain gather at full and half resolution,
/// then one row per `(name, settings, half)`, each reading the same settled probes.
fn temporal_aa_rows(scene: &GiScene, gathers: &[(&str, RadianceCacheSettings, bool)]) -> Vec<GiRow> {
    let image = reference(scene);
    let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    let reference_indirect = add_planes(&image.indirect_diffuse, &image.indirect_specular);
    let taa = |ssgi: bool| FrameQuality {
        taau: true,
        ..probes_quality(ssgi)
    };
    let direct = {
        let mut lab = scene.lab.clone();
        lab.ambient_lux = 0.0;
        let quality = FrameQuality {
            ssao: SsaoMode::Off,
            taau: true,
            ..base_quality()
        };
        capture(&mut renderer(scene, false), &lab, quality, TAA_FRAMES)
    };
    let mut rows = Vec::new();
    let mut r = renderer(scene, true);
    let mut push = |name: &str, plane: &[[f32; 3]]| {
        rows.push(GiRow {
            scene: scene.name.to_owned(),
            technique: name.to_owned(),
            error: measure(&GiErrorInput {
                reference: reference_indirect.as_slice(),
                test: plane,
                surface: &surface,
                shadow: &shadow,
                width: GI_WIDTH,
                height: GI_HEIGHT,
                ev100: scene.lab.exposure_ev,
                scale: scene.scale,
            }),
        });
    };
    capture(&mut r, &scene.lab, probes_quality(false), PROBE_FRAMES);
    let full = capture(&mut r, &scene.lab, taa(false), TAA_FRAMES);
    push("probes", &sub_planes(&full, &direct));
    let full = capture(&mut r, &scene.lab, taa(true), TAA_FRAMES);
    push("probes_ssgi", &sub_planes(&full, &direct));
    r.set_ssgi_half_res(Some(true));
    let full = capture(&mut r, &scene.lab, taa(true), TAA_FRAMES);
    push("probes_ssgi half", &sub_planes(&full, &direct));
    r.set_ssgi_half_res(None);
    r.set_radiance_cache(true);
    for &(name, settings, half) in gathers {
        r.set_ssgi_half_res(Some(half));
        r.set_radiance_cache_settings(RadianceCacheSettings {
            budget_cells: TAA_BUDGET,
            ..settings
        });
        let full = capture(&mut r, &scene.lab, taa(true), TAA_FRAMES);
        push(name, &sub_planes(&full, &direct));
    }
    eprintln!("under temporal AA on {}:\n{}", scene.name, render_table(&rows));
    rows
}

/// The economy gather (ADR-2605) under temporal AA, at full and half resolution, against the plain gather
/// at the same resolution. With the miss read it failed this on the trench (rel. RMSE 0.183 against 0.158).
fn economy_within_tolerance(scene: &GiScene) {
    let economy = RadianceCacheSettings::economy();
    let rows = temporal_aa_rows(
        scene,
        &[("economy", economy, false), ("economy half", economy, true)],
    );
    assert_row_within(&rows, scene.name, "economy", "probes_ssgi");
    assert_row_within(&rows, scene.name, "economy half", "probes_ssgi half");
}

#[test]
fn the_economy_gather_stays_within_tolerance_of_the_tracer_in_the_trench() {
    economy_within_tolerance(&trench());
}

#[test]
fn the_economy_gather_stays_within_tolerance_of_the_tracer_in_the_colour_room() {
    economy_within_tolerance(&color_room());
}

/// Measurement, not a gate: the gather as a real frame runs it, under temporal AA, where the noise pattern
/// moves and fewer rays take turns (`RadianceCacheSettings::rays`). The rows without it score one frame's
/// noise pattern, which is every shipped frame's worst case and no shipped frame's image. This decided the
/// cache's default tunables (ADR-2605).
#[test]
#[ignore = "measurement: prints a table per plate"]
fn sweep_the_gather_under_temporal_aa() {
    let full = RadianceCacheSettings::default();
    let economy = RadianceCacheSettings::economy();
    let miss = |miss_cap: f32, s: RadianceCacheSettings| RadianceCacheSettings { miss_cap, ..s };
    for scene in [trench(), color_room()] {
        temporal_aa_rows(
            &scene,
            &[
                ("full miss4", miss(4.0, full), false),
                ("full miss0", miss(0.0, full), false),
                ("full miss1.5", miss(1.5, full), false),
                ("economy miss4", miss(4.0, economy), false),
                ("economy miss0", miss(0.0, economy), false),
                ("half full miss4", miss(4.0, full), true),
                ("half full miss0", miss(0.0, full), true),
                ("half economy miss0", miss(0.0, economy), true),
            ],
        );
    }
}

/// Measurement, not a gate (ADR-4106): the trench's shadow deficit split into the cached gather's terms on
/// one frame, every plane the last frame of the same history, so no row scores the probes' age
/// (`cargo test -p regia_pt --test gi_radiance_cache -- --ignored --nocapture attribute`).
///
/// Per region of the tracer's sun shadow (all, the floor, the walls), the mean luminance of: the tracer;
/// the probe frame and what it lacks; what screen-space AO took from it; the gather (`total − base`) as
/// hits' radiance minus their correction plus what the darken clamp gave back; what a miss read would
/// add; the cache's own light at the pixel against the probe light the frame holds; and the plain
/// gather on the same frame. Each also over the tracer's mean in that region.
#[test]
#[ignore = "measurement: prints the attribution table"]
fn attribute_the_trench_shadow_deficit() {
    let full = trench();
    // The same trench under the sun alone: what is left of each row is the sun's bounce, and the
    // difference from the full plate is the sky's share.
    let mut sun_only = trench();
    sun_only.lab.ambient_lux = 0.0;
    for (label, scene) in [("trench", &full), ("trench_sun_only", &sun_only)] {
        attribute_one(label, scene);
    }
}

fn attribute_one(label: &str, scene: &GiScene) {
    let a = attribute(scene);
    let hits = a.term(RadianceCacheTerm::HitRadiance);
    let corr = a.term(RadianceCacheTerm::HitCorrection);
    let signed = sub_planes(&hits, &corr);
    let gather = a.minus("total", "base");
    let clamp = sub_planes(&gather, &signed);
    let miss = sub_planes(
        &a.term(RadianceCacheTerm::MissRead),
        &a.term(RadianceCacheTerm::MissCorrection),
    );
    let base = sub_planes(a.plane("base"), &a.direct);
    let base_no_ao = sub_planes(a.plane("base_no_ao"), &a.direct);
    let origin = a.term(RadianceCacheTerm::Origin);
    let derived: Vec<(&str, Vec<[f32; 3]>)> = vec![
        ("tracer", a.reference.clone()),
        ("probes (no gather)", sub_planes(a.plane("probes"), &a.direct)),
        ("base (cached frame, no gather)", base.clone()),
        ("shipped", sub_planes(a.plane("shipped"), &a.direct)),
        ("total (shipped, fresh)", sub_planes(a.plane("total"), &a.direct)),
        ("needed = tracer - base", sub_planes(&a.reference, &base)),
        ("AO took = base_no_ao - base", sub_planes(&base_no_ao, &base)),
        ("gather = total - base", gather.clone()),
        ("  hit radiance", hits.clone()),
        ("  hit correction (subtracted)", corr.clone()),
        ("  signed = hits - correction", signed.clone()),
        ("  darken clamp gave back", clamp.clone()),
        ("miss read would add (cap 4)", miss.clone()),
        ("origin (cache light, nominal albedo)", origin.clone()),
        ("origin at true albedo", at_true_albedo(&origin, &a.albedo)),
        ("probe light without AO", base_no_ao.clone()),
        ("signed at true albedo", at_true_albedo(&signed, &a.albedo)),
        ("plain gather = plain - base", a.minus("plain", "base")),
        (
            "probes, 4x the frames",
            sub_planes(a.plane("probes_aged"), &a.direct),
        ),
        (
            "probes at half the spacing",
            sub_planes(a.plane("probes_dense"), &a.direct),
        ),
        (
            "cached gather over them",
            sub_planes(a.plane("cache_dense"), &a.direct),
        ),
    ];
    let floor: Vec<bool> = a
        .shadow
        .iter()
        .zip(&a.normal)
        .map(|(s, n)| *s && n[1] > 0.7)
        .collect();
    let walls: Vec<bool> = a
        .shadow
        .iter()
        .zip(&a.normal)
        .map(|(s, n)| *s && n[1].abs() <= 0.7)
        .collect();
    let regions = [("shadow", &a.shadow), ("floor", &floor), ("walls", &walls)];
    let mut table = String::from("| plane |");
    for (name, mask) in regions {
        let n = mask.iter().filter(|m| **m).count();
        table.push_str(&format!(" {name} ({n} px) cd/m2 | / tracer |"));
    }
    table.push_str("\n|---|---|---|---|---|---|---|\n");
    for (name, plane) in &derived {
        table.push_str(&format!("| {name} |"));
        for (_, mask) in regions {
            let tracer = masked_luminance(&a.reference, mask);
            let v = masked_luminance(plane, mask);
            table.push_str(&format!(" {v:.2} | {:+.4} |", v / tracer.max(1e-9)));
        }
        table.push('\n');
    }
    let coverage = a.term(RadianceCacheTerm::Coverage);
    for (name, mask) in regions {
        let n = mask.iter().filter(|m| **m).count().max(1) as f64;
        let mean = |c: usize| {
            coverage
                .iter()
                .zip(mask.iter())
                .filter(|(_, m)| **m)
                .map(|(v, _)| f64::from(v[c]))
                .sum::<f64>()
                / n
        };
        table.push_str(&format!(
            "coverage {name}: hits {:.3}, valid misses {:.3}, gathered {:.3} of pixels\n",
            mean(0),
            mean(1),
            mean(2)
        ));
    }
    // The instrument: with positive upsample weights, the clamp can only give light back, up to the
    // half-float rounding of the four captures it is made of (2^-11 of each value; a sunlit pixel's
    // ulp is many cd/m2).
    let scale = masked_luminance(&a.reference, &a.surface);
    let captures = ["total", "base", "hit_radiance", "hit_correction"].map(|n| a.plane(n));
    let worst = (0..clamp.len())
        .filter(|&i| a.surface[i])
        .map(|i| {
            let ulps: f32 = captures.iter().map(|c| luminance(c[i]).abs()).sum::<f32>() / 1024.0;
            luminance(clamp[i]) + ulps
        })
        .fold(f32::INFINITY, f32::min);
    // The origin's cells (blue of the coverage term is one over the pre-exposure where a pixel gathered).
    let one = coverage.iter().map(|c| c[2]).fold(0.0f32, f32::max).max(1e-9);
    let cells = a.term(RadianceCacheTerm::Cells);
    for (name, mask) in regions {
        let n = mask.iter().filter(|m| **m).count().max(1) as f64;
        let mean = |c: usize| {
            cells
                .iter()
                .zip(mask.iter())
                .filter(|(_, m)| **m)
                .map(|(v, _)| f64::from(v[c] / one))
                .sum::<f64>()
                / n
        };
        table.push_str(&format!(
            "origin cells {name}: own tag {:.3}, counted {:.3}, free {:.3} of 8\n",
            mean(0),
            mean(1),
            mean(2)
        ));
    }
    // The bounce's source: the direct light on the sunlit surface, real time against the tracer.
    let lit: Vec<bool> = a.surface.iter().zip(&a.shadow).map(|(s, d)| *s && !*d).collect();
    let lit_count = lit.iter().filter(|m| **m).count();
    table.push_str(&format!(
        "direct light on the sunlit surface ({lit_count} px): real time {:.1}, tracer {:.1} cd/m2\n",
        masked_luminance(&a.direct, &lit),
        masked_luminance(&a.tracer_direct, &lit)
    ));
    let shipped_vs_total = masked_luminance(&a.minus("shipped", "total"), &a.shadow);
    let probes_vs_base = masked_luminance(&a.minus("probes", "base"), &a.shadow);
    table.push_str(&format!(
        "checks: most negative clamp past rounding {worst:.4} cd/m2 (surface mean {scale:.1}); shadow mean shipped - total \
         {shipped_vs_total:.4}, probes - base {probes_vs_base:.4}\n"
    ));
    eprintln!("{label} attribution (ADR-4106):\n{table}");
    if let Some(dir) = regia_core::brand::env_var_os(GI_TABLE_ENV) {
        let dir = std::path::PathBuf::from(dir);
        let path = dir.join(format!("{label}_attribution.md"));
        if let Err(e) = std::fs::create_dir_all(&dir)
            .and_then(|()| regia_test_utils::write_atomic(&path, table.as_bytes()))
        {
            eprintln!("{GI_TABLE_ENV}: {e}");
        }
        let ev = scene.lab.exposure_ev;
        write_sheet(
            &format!("{label}_attribution"),
            &[
                sub_planes(&a.reference, &base),
                gather,
                hits,
                corr.iter().map(|c| [-c[0], -c[1], -c[2]]).collect(),
                sub_planes(&base, &base_no_ao),
                miss,
                sub_planes(&at_true_albedo(&origin, &a.albedo), &base_no_ao),
                a.minus("plain", "base"),
            ],
            ev,
        );
    }
    assert!(
        worst >= 0.0,
        "the clamp took light past rounding: {worst} against a surface mean of {scale}"
    );
}

/// The attribution terms (ADR-4106) are faithful on the trench, without the tracer: on one history, the
/// `base` term is the frame with no gather at all (bit for bit on lavapipe), and every surface pixel's
/// gather (`total − base`) is its hits' radiance minus their correction plus what the darken clamp gave
/// back, which is never negative past the half-float rounding of the captures. The terms are positive,
/// the coverage term reads 1 where every pixel gathered, and the miss terms are zero-cost: the default
/// gather has no miss read, so its image does not depend on them.
#[test]
fn the_attribution_terms_add_up_to_the_gather() {
    let scene = trench();
    let q = probes_quality(true);
    let total = matched_term(&scene, RadianceCacheTerm::Total, q);
    let base = matched_term(&scene, RadianceCacheTerm::Base, q);
    let hits = matched_term(&scene, RadianceCacheTerm::HitRadiance, q);
    let corr = matched_term(&scene, RadianceCacheTerm::HitCorrection, q);
    let coverage = matched_term(&scene, RadianceCacheTerm::Coverage, q);
    let probes = matched_last_frame(&scene, |r| {
        r.set_radiance_cache(false);
        probes_quality(false)
    });
    let lavapipe = matches!(
        golden_icd(&device().adapter.get_info()),
        GoldenIcd::Lavapipe { .. }
    );
    let mean = |p: &[[f32; 3]]| p.iter().map(|c| f64::from(luminance(*c))).sum::<f64>() / p.len() as f64;
    let scale = mean(&base);
    if lavapipe {
        assert!(base == probes, "the base term is the frame without the gather");
    } else {
        let diff = mean(&sub_planes(&base, &probes)).abs();
        assert!(diff <= 1e-3 * scale, "base against no gather: {diff} of {scale}");
    }
    let cov = sub_planes(&coverage, &base);
    // Blue is one over the pre-exposure (in cd/m2) where the pixel gathered; red and green are
    // shares of it.
    let one = cov.iter().map(|c| c[2]).fold(0.0f32, f32::max);
    assert!(one > 0.0, "some pixel gathered");
    let (mut gathered, mut lit) = (0usize, 0usize);
    for i in 0..total.len() {
        let rounding = [total[i], base[i], hits[i], corr[i]]
            .iter()
            .map(|c| luminance(*c).abs())
            .sum::<f32>()
            / 1024.0;
        let gather = luminance(total[i]) - luminance(base[i]);
        let h = luminance(hits[i]) - luminance(base[i]);
        let c = luminance(corr[i]) - luminance(base[i]);
        assert!(h >= -rounding && c >= -rounding, "pixel {i}: terms {h}, {c}");
        let clamp = gather - (h - c);
        assert!(
            clamp >= -rounding,
            "pixel {i}: the clamp took {clamp} (rounding {rounding})"
        );
        if h > rounding {
            lit += 1;
        }
        let slack = (coverage[i].iter().fold(0.0f32, |m, v| m.max(v.abs())) + one) / 1024.0;
        assert!(
            cov[i].iter().all(|v| *v <= one + slack),
            "pixel {i}: coverage {:?} over {one}",
            cov[i]
        );
        if cov[i][2] > 0.5 * one {
            gathered += 1;
        }
    }
    assert!(
        gathered > total.len() / 2,
        "{gathered} of {} gathered",
        total.len()
    );
    assert!(lit > 0, "some pixel's rays hit the screen");
}

/// `scene` with every mesh, the camera and the probes' bake box lifted by `dy` metres.
fn lifted(scene: &GiScene, dy: f32) -> GiScene {
    let mut s = scene.clone();
    for mesh in &mut s.lab.meshes {
        mesh.model[3][1] += dy;
    }
    s.lab.eye[1] += dy;
    s.lab.target[1] += dy;
    s.bake_bounds.0[1] += dy;
    s.bake_bounds.1[1] += dy;
    s
}

/// The trench's floor lies on a plane of the cache's cells (y = 0, cells every 0.5 m). How far a lift
/// moves the floor off it.
const LIFT_M: f32 = 0.005;

/// What the origin read does on the gathered pixels of `scene` (ADR-4106): the mean share of the eight
/// cells that stand in free space, and the hit correction's mean luminance (cd/m2).
fn origin_read(scene: &GiScene) -> (f64, f64) {
    let q = probes_quality(true);
    let base = matched_term(scene, RadianceCacheTerm::Base, q);
    let coverage = sub_planes(&matched_term(scene, RadianceCacheTerm::Coverage, q), &base);
    let cells = sub_planes(&matched_term(scene, RadianceCacheTerm::Cells, q), &base);
    let corr = sub_planes(&matched_term(scene, RadianceCacheTerm::HitCorrection, q), &base);
    let one = coverage.iter().map(|c| c[2]).fold(0.0f32, f32::max);
    assert!(one > 0.0, "{}: some pixel gathered", scene.name);
    let gathered: Vec<bool> = coverage.iter().map(|c| c[2] > 0.5 * one).collect();
    let n = gathered.iter().filter(|g| **g).count().max(1) as f64;
    let free = cells
        .iter()
        .zip(&gathered)
        .filter(|(_, g)| **g)
        .map(|(c, _)| f64::from(c[2] / one))
        .sum::<f64>()
        / n;
    (free, masked_luminance(&corr, &gathered))
}

/// ADR-4106, the guilty term: the trench's floor lies on a cell plane, and whether its reconstructed
/// position lands a hair above or below that plane must not choose which layer of cells the gather
/// subtracts. A cell the floor stands in front of by 5 mm counts as on the floor's plane; one 5 mm
/// behind it was dropped, and the renormalized trilinear read then took the layer half a cell up (all of
/// the weight): a brighter correction toward the walls, and a darker shadow. The RTX 5080 made that
/// lookup on the unlifted plate (free share 0.50 against lavapipe's 0.004 on the floor). The free share
/// is printed, not asserted: it counts corners, and a floor just above the plane has the layer above
/// among its corners (at a weight near 0) where one just below has the layer beneath instead.
#[test]
fn the_origin_read_does_not_flip_with_the_floor_across_a_cell_plane() {
    let (free_down, corr_down) = origin_read(&lifted(&trench(), -LIFT_M));
    let (free_up, corr_up) = origin_read(&lifted(&trench(), LIFT_M));
    eprintln!(
        "origin read, floor {LIFT_M} m below / above a cell plane: free share {free_down:.3} / {free_up:.3}, \
         hit correction {corr_down:.2} / {corr_up:.2} cd/m2"
    );
    assert!(
        (corr_up - corr_down).abs() <= 0.03 * corr_up.max(corr_down),
        "the hit correction flips with the floor: {corr_down} below, {corr_up} above"
    );
}

/// The trench's sun-shadow ratio to the tracer (`1 + shadow_bias`) for the shipped cached gather, and
/// for the probe frame without a gather on the same history.
fn shipped_shadow_ratio(scene: &GiScene) -> (f64, f64) {
    let image = reference(scene);
    let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    let reference_indirect = add_planes(&image.indirect_diffuse, &image.indirect_specular);
    let direct = realtime_direct(scene);
    let ratio = |full: &[[f32; 3]]| {
        let error = measure(&GiErrorInput {
            reference: reference_indirect.as_slice(),
            test: &sub_planes(full, &direct),
            surface: &surface,
            shadow: &shadow,
            width: GI_WIDTH,
            height: GI_HEIGHT,
            ev100: scene.lab.exposure_ev,
            scale: scene.scale,
        });
        1.0 + f64::from(error.shadow_bias.expect("shadow pixels"))
    };
    let shipped = matched_last_frame(scene, |_| probes_quality(true));
    let probes = matched_last_frame(scene, |r| {
        r.set_radiance_cache(false);
        probes_quality(false)
    });
    (ratio(&shipped), ratio(&probes))
}

/// Lavapipe's shadow ratio of the trench lifted `LIFT_M` up, before ADR-4106's fix: the floor's
/// on-plane cells dropped (recorded on the fixed-seed plate, not a golden).
const LIFTED_RATIO_BEFORE: f64 = 0.950_191;

/// ADR-4106, the strict direction check on the plate that shows the defect: lifted 5 mm, the trench's
/// shadow luminance must come more than 0.001 nearer the tracer than before the fix, and what the gather
/// adds over the probe frame must match the plate lowered 5 mm (which read the on-plane cells all along)
/// within 0.001. The probes themselves see the 10 mm between the two plates, so their own change is
/// taken out.
#[test]
fn the_lifted_trench_reads_the_floor_cells_and_comes_nearer_the_tracer() {
    let (down, down_probes) = shipped_shadow_ratio(&lifted(&trench(), -LIFT_M));
    let (up, up_probes) = shipped_shadow_ratio(&lifted(&trench(), LIFT_M));
    eprintln!(
        "trench shadow ratio, floor {LIFT_M} m below / above a cell plane: {down:.6} / {up:.6} \
         (probes alone {down_probes:.6} / {up_probes:.6})"
    );
    let (gain_down, gain_up) = (down - down_probes, up - up_probes);
    assert!(
        (gain_up - gain_down).abs() <= 0.001,
        "lifting the floor across a cell plane moves the gather's gain from {gain_down} to {gain_up}"
    );
    if matches!(
        golden_icd(&device().adapter.get_info()),
        GoldenIcd::Lavapipe { .. }
    ) {
        assert!(
            (up - 1.0).abs() + 0.001 < (LIFTED_RATIO_BEFORE - 1.0).abs(),
            "lifted trench {up} against {LIFTED_RATIO_BEFORE} before the fix"
        );
    }
}
