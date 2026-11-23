// SPDX-License-Identifier: MPL-2.0
//! Where the probes' deficit on the GI plates came from (`specs/79` § 12, ADR-2605): the trench read 41 %
//! and the color room 25 % under the path tracer's indirect light on the plates' first layouts. A
//! measurement, not a gate; it splits the number by what could cause it: the sky against the sun, the
//! frame's screen-space AO, the specular share of the reference, the surface's facing and the layout.
//!
//! `cargo test -p regia_pt --test gi_probe_deficit -- --ignored --nocapture`

use std::sync::OnceLock;

use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtImage, PtSettings};
use regia_render::{
    FrameQuality, FrameRenderer, GiMode, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode,
};
use regia_test_utils::gi_error::{
    GiErrorInput, add_planes, luminance, measure, shadow_mask, sub_planes, surface_mask,
};
use regia_test_utils::gi_scenes::{GI_HEIGHT, GI_WIDTH, GiScene, color_room, sealed_box, trench};

const PROBE_FRAMES: u32 = 64;
const STILL_FRAMES: u32 = 2;
const SPP: u32 = 512;

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

fn trace(scene: &GiScene, max_bounces: u32) -> PtImage {
    let frame = scene.lab.as_frame_scene_with_quality(base_quality());
    let pt = from_frame(&frame, &ExtractOptions::default());
    tracer()
        .render(
            &pt,
            &PtSettings {
                spp: SPP,
                max_bounces,
                seed: 23,
                jitter: false,
                samples_per_submit: 8,
                ..PtSettings::default()
            },
        )
        .expect("trace the reference")
}

fn capture(scene: &GiScene, quality: FrameQuality, probes: bool, frames: u32) -> Vec<[f32; 3]> {
    let mut r = FrameRenderer::new();
    r.set_ddgi_settings(scene.ddgi.clone());
    r.set_ddgi_enabled(Some(probes));
    let frame = scene.lab.as_frame_scene_with_quality(quality);
    for i in 0..frames {
        if i + 1 == frames {
            r.request_hdr_capture();
        }
        r.render_offscreen(device(), &frame).expect("frame");
    }
    r.take_hdr_capture(device())
        .expect("read back")
        .expect("the frame recorded its HDR copy")
        .rgb
}

fn direct(scene: &GiScene) -> Vec<[f32; 3]> {
    let mut sun = scene.clone();
    sun.lab.ambient_lux = 0.0;
    let q = FrameQuality {
        ssao: SsaoMode::Off,
        ..base_quality()
    };
    capture(&sun, q, false, STILL_FRAMES)
}

fn probe_frame(scene: &GiScene, ssao: bool) -> Vec<[f32; 3]> {
    let mut q = FrameQuality {
        gi: GiMode::Probes,
        gi_probes: true,
        ..base_quality()
    };
    if !ssao {
        q.ssao = SsaoMode::Off;
    }
    let full = capture(scene, q, true, PROBE_FRAMES);
    sub_planes(&full, &direct(scene))
}

const FACINGS: [(&str, [f32; 3]); 6] = [
    ("+x", [1.0, 0.0, 0.0]),
    ("-x", [-1.0, 0.0, 0.0]),
    ("+y", [0.0, 1.0, 0.0]),
    ("-y", [0.0, -1.0, 0.0]),
    ("+z", [0.0, 0.0, 1.0]),
    ("-z", [0.0, 0.0, -1.0]),
];

/// Mean luminance of each plane over `mask`, and the pixel count.
fn means(planes: &[&[[f32; 3]]], mask: &[bool]) -> (Vec<f32>, usize) {
    let n = mask.iter().filter(|m| **m).count();
    let means = planes
        .iter()
        .map(|p| {
            let sum: f32 = p
                .iter()
                .zip(mask)
                .filter(|(_, m)| **m)
                .map(|(c, _)| luminance(*c))
                .sum();
            sum / n.max(1) as f32
        })
        .collect();
    (means, n)
}

/// One row per facing plus the total: the tracer's diffuse and specular indirect light, and the probes'
/// indirect light with and without the frame's AO.
fn report(label: &str, scene: &GiScene) {
    let image = trace(scene, 6);
    let one = trace(scene, 1);
    let ao = probe_frame(scene, true);
    let no_ao = probe_frame(scene, false);
    let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
    let reference = add_planes(&image.indirect_diffuse, &image.indirect_specular);
    let planes: [&[[f32; 3]]; 6] = [
        &reference,
        &image.indirect_diffuse,
        &image.indirect_specular,
        &one.indirect_diffuse,
        &ao,
        &no_ao,
    ];
    eprintln!("{label}");
    eprintln!(
        "  facing     px      ref     diff     spec  diff_1b   probes  probes_noao | probes/ref noao/ref noao/diff"
    );
    let mut rows: Vec<(&str, Vec<bool>)> = FACINGS
        .iter()
        .map(|(name, axis)| {
            let mask = surface
                .iter()
                .zip(&image.normal)
                .map(|(s, n)| *s && n[0] * axis[0] + n[1] * axis[1] + n[2] * axis[2] > 0.9)
                .collect();
            (*name, mask)
        })
        .collect();
    rows.push(("all", surface.clone()));
    for (name, mask) in rows {
        let (m, n) = means(&planes, &mask);
        if n == 0 {
            continue;
        }
        eprintln!(
            "  {name:>6} {n:>6} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1}     | {:>9.3} {:>8.3} {:>8.3}",
            m[0],
            m[1],
            m[2],
            m[3],
            m[4],
            m[5],
            m[4] / m[0].max(1e-3),
            m[5] / m[0].max(1e-3),
            m[5] / m[1].max(1e-3),
        );
    }
}

#[test]
#[ignore = "measurement: prints where the probes' deficit sits on the lit plates"]
fn probe_deficit_by_light_and_facing() {
    for scene in [trench(), color_room()] {
        report(&format!("{} (sun + sky)", scene.name), &scene);
        let mut sky = scene.clone();
        sky.lab.directional.illuminance = 0.0;
        report(&format!("{} (sky only)", scene.name), &sky);
        let mut sun = scene.clone();
        sun.lab.ambient_lux = 0.0;
        report(&format!("{} (sun only)", scene.name), &sun);
    }
}

/// The probe frame's indirect light and the coverage of the screen lookup per pixel (the lookup's
/// nearest texel, half resolution): under 1 the frame mixes in the flat sky term, not probe light.
fn probes_and_coverage(scene: &GiScene) -> (Vec<[f32; 3]>, Vec<f32>) {
    let q = FrameQuality {
        gi: GiMode::Probes,
        gi_probes: true,
        ..base_quality()
    };
    let mut r = FrameRenderer::new();
    r.set_ddgi_settings(scene.ddgi.clone());
    r.set_ddgi_enabled(Some(true));
    let frame = scene.lab.as_frame_scene_with_quality(q);
    for i in 0..PROBE_FRAMES {
        if i + 1 == PROBE_FRAMES {
            r.request_hdr_capture();
        }
        r.render_offscreen(device(), &frame).expect("frame");
    }
    let hdr = r
        .take_hdr_capture(device())
        .expect("read back")
        .expect("the frame recorded its HDR copy");
    let rb = r
        .ddgi_read_back(device())
        .expect("read back")
        .expect("the probes ran");
    let (tw, th, texels) = rb.screen_irradiance().expect("the lookup ran");
    let (w, h) = (GI_WIDTH as usize, GI_HEIGHT as usize);
    let (tw, th) = (tw as usize, th as usize);
    let coverage = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            texels[(y * th / h).min(th - 1) * tw + (x * tw / w).min(tw - 1)].coverage
        })
        .collect();
    (sub_planes(&hdr.rgb, &direct(scene)), coverage)
}

/// That the plates' deficit was coverage: the plates' first layouts (their volumes stop short of what the
/// camera sees, so those pixels read the flat sky term), the plates' layouts, and larger ones.
/// `cargo test -p regia_pt --test gi_probe_deficit -- --ignored --nocapture by_layout`
#[test]
#[ignore = "measurement: prints the plates under layouts that do and do not cover the view"]
fn probe_deficit_by_layout() {
    for (scene, first, larger) in [
        (trench(), [16, 6, 8], [24, 16, 16]),
        (color_room(), [12, 8, 12], [16, 24, 32]),
        (sealed_box(), [10, 8, 10], [14, 16, 14]),
    ] {
        let frame = scene.lab.as_frame_scene_with_quality(base_quality());
        let pt = from_frame(&frame, &ExtractOptions::default());
        let image = tracer()
            .render(
                &pt,
                &PtSettings {
                    spp: 256,
                    max_bounces: 6,
                    seed: 23,
                    jitter: false,
                    samples_per_submit: 8,
                    ..PtSettings::default()
                },
            )
            .expect("trace the reference");
        let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
        let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
        let reference = add_planes(&image.indirect_diffuse, &image.indirect_specular);
        let mut short = scene.clone();
        short.ddgi.dims = first;
        let mut wide = scene.clone();
        wide.ddgi.dims = larger;
        for (label, s) in [
            ("first layout", &short),
            ("plate layout", &scene),
            ("larger layout", &wide),
        ] {
            let (test, coverage) = probes_and_coverage(s);
            let e = measure(&GiErrorInput {
                reference: reference.as_slice(),
                test: &test,
                surface: &surface,
                shadow: &shadow,
                width: GI_WIDTH,
                height: GI_HEIGHT,
                ev100: scene.lab.exposure_ev,
                scale: scene.scale,
            });
            eprintln!(
                "{} {label} {:?}: rel_rmse {:.3} shadow_bias {:?} energy {:?} flip {:.3}",
                scene.name, s.ddgi.dims, e.rel_rmse, e.shadow_bias, e.energy_ratio, e.flip_mean
            );
            let mut rows: Vec<(&str, Vec<bool>)> = FACINGS
                .iter()
                .map(|(name, axis)| {
                    let mask = surface
                        .iter()
                        .zip(&image.normal)
                        .map(|(s, n)| *s && n[0] * axis[0] + n[1] * axis[1] + n[2] * axis[2] > 0.9)
                        .collect();
                    (*name, mask)
                })
                .collect();
            rows.push(("all", surface.clone()));
            for (name, mask) in rows {
                let (m, n) = means(&[&reference, &test], &mask);
                if n == 0 {
                    continue;
                }
                let cov: f32 = coverage
                    .iter()
                    .zip(&mask)
                    .filter(|(_, m)| **m)
                    .map(|(c, _)| c.clamp(0.0, 1.0))
                    .sum::<f32>()
                    / n as f32;
                eprintln!(
                    "  {name:>4} {n:>6} ref {:>8.1} probes {:>8.1} ratio {:>6.3} coverage {cov:.3}",
                    m[0],
                    m[1],
                    m[1] / m[0].max(1e-3)
                );
            }
        }
    }
}
