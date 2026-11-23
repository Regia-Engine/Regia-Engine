// SPDX-License-Identifier: MPL-2.0
//! Multi-bounce bake controls (ADR-1096) on the real kernels: the bounce count against the
//! integrating sphere's analytic series, the luminance clamps and what they report, sample
//! groups (exactness, determinism, the noise estimate falling as `1/√samples`), firefly
//! suppression on a rare bright emitter, and the bake report. Runs on whatever adapter
//! `RenderDevice::headless` picks (lavapipe in CI). No golden images.

use std::f32::consts::PI;
use std::sync::OnceLock;

use regia_asset::baked_gi::{GiComponent, SH_Y00, VolumeLayout, irradiance};
use regia_core::glam::Mat4;
use regia_pt::bake_quality::{bounce_energy_bounds, median};
use regia_pt::probe::{GiBakeSettings, ProbeTrace, ProbeTraceSettings, bake_gi_volume, detail_brick};
use regia_pt::scenes::{self, quad};
use regia_pt::{PathTracer, PtCamera, PtMaterial, PtScene, PtSky};
use regia_render::RenderDevice;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn settings(rays: u32, samples: u32, bounces: u32) -> ProbeTraceSettings {
    ProbeTraceSettings {
        rays_per_probe: rays,
        samples,
        max_bounces: bounces,
        seed: 5,
        samples_per_submit: 8,
        ..ProbeTraceSettings::default()
    }
}

fn trace(scene: &PtScene, positions: &[[f32; 3]], s: &ProbeTraceSettings) -> Vec<ProbeTrace> {
    tracer()
        .trace_probes(scene, positions, s, |_, _| true)
        .expect("trace probes")
}

fn camera() -> PtCamera {
    PtCamera::pinhole([0.0, 0.0, 5.0], [0.0; 3], 0.5, 4, 4)
}

/// Mean incident radiance (luminance) of one component: L00 / (4π·Y00).
fn mean_radiance(t: &ProbeTrace, c: GiComponent) -> f32 {
    let sh = &t.sh[c as usize];
    (0.2126 * sh[0][0] + 0.7152 * sh[1][0] + 0.0722 * sh[2][0]) / (4.0 * PI * SH_Y00)
}

/// 27 probes in the open air of the Cornell box: above the tall block (0.33 m) and below the
/// ceiling light (0.549 m), so every probe sees light.
fn cornell_grid() -> Vec<[f32; 3]> {
    let mut p = Vec::new();
    for k in 0..3 {
        for j in 0..3 {
            for i in 0..3 {
                p.push([
                    0.1 + 0.17 * i as f32,
                    0.36 + 0.07 * j as f32,
                    0.1 + 0.17 * k as f32,
                ]);
            }
        }
    }
    p
}

/// Inside a closed sphere of albedo a with an emitting cap (radiance e, solid-angle fraction f
/// from the centre) the converged mean radiance at the centre is f·e/(1 − a). A bake that stops
/// after B path vertices keeps between 1 − a^B and 1 − a^(B+1) of it
/// (`bake_quality::bounce_energy_bounds`), and more bounces never keep less.
#[test]
fn the_bounce_count_converges_on_the_integrating_sphere_series() {
    let (a, e, c) = (0.5f32, 1.0f32, 0.5f32);
    let (scene, f) = scenes::closed_sphere(a, e, c, 4);
    let converged = f as f32 * e / (1.0 - a);
    let mut last = 0.0f32;
    for b in [1u32, 2, 3, 4, 6, 8, 12, 16] {
        let s = ProbeTraceSettings {
            rr_start: 64,
            ..settings(256, 32, b)
        };
        let t = &trace(&scene, &[[0.0; 3]], &s)[0];
        assert_eq!(t.rejected, 0);
        let kept = mean_radiance(t, GiComponent::Emissive) / converged;
        let (lo, hi) = bounce_energy_bounds(a, b);
        eprintln!("bounces {b}: kept {kept:.4} of the converged light, bounds {lo:.4}..{hi:.4}");
        let tol = 0.02;
        assert!(
            kept >= lo - tol && kept <= hi + tol,
            "bounces {b}: {kept} outside {lo}..{hi}"
        );
        assert!(kept >= last - tol, "bounces {b}: {kept} below {last}");
        last = kept;
    }
    assert!((last - 1.0).abs() < 0.02, "16 bounces: {last}");
}

/// Russian roulette ends paths early without biasing them: 16 bounces with roulette from the
/// second vertex land on the converged series as without it, with fewer path vertices traced.
#[test]
fn russian_roulette_keeps_the_bounce_series_unbiased() {
    let (a, e, c) = (0.5f32, 1.0f32, 0.5f32);
    let (scene, f) = scenes::closed_sphere(a, e, c, 4);
    let converged = f as f32 * e / (1.0 - a);
    for rr_start in [2u32, 4, 64] {
        let s = ProbeTraceSettings {
            rr_start,
            ..settings(256, 64, 16)
        };
        let t = &trace(&scene, &[[0.0; 3]], &s)[0];
        let kept = mean_radiance(t, GiComponent::Emissive) / converged;
        eprintln!("rr_start {rr_start}: kept {kept:.4}");
        assert!((kept - 1.0).abs() < 0.03, "rr_start {rr_start}: {kept}");
    }
}

/// `clamp_direct` caps light the probe sees directly (here the sky) and leaves light after a
/// bounce alone; the capped contribution keeps its color, and the trace reports the count and
/// the share removed.
#[test]
fn clamp_direct_caps_the_sky_and_keeps_its_color() {
    let mut scene = PtScene::new(camera());
    scene.sky = PtSky::Uniform([2.0, 1.0, 0.5]);
    let lum = 0.2126 * 2.0 + 0.7152 * 1.0 + 0.0722 * 0.5;
    let base = settings(64, 4, 2);
    let open = &trace(&scene, &[[0.0; 3]], &base)[0];
    assert_eq!((open.clamped, open.clamp_loss), (0, 0.0));
    let capped = &trace(
        &scene,
        &[[0.0; 3]],
        &ProbeTraceSettings {
            clamp_direct: 0.5 * lum,
            ..base
        },
    )[0];
    assert_eq!(
        capped.clamped, capped.rays,
        "every escaped ray is one direct sky contribution"
    );
    assert!((capped.clamp_loss - 0.5).abs() < 1e-3, "{}", capped.clamp_loss);
    let sky = &capped.sh[GiComponent::Sky as usize];
    let full = &open.sh[GiComponent::Sky as usize];
    for (c, f) in sky.iter().zip(full) {
        assert!((c[0] - 0.5 * f[0]).abs() < 1e-3 * f[0], "{sky:?} {full:?}");
    }
    assert!((sky[0][0] / sky[1][0] - 2.0).abs() < 1e-3, "color kept: {sky:?}");
    // The indirect cap does not touch light seen directly.
    let other = &trace(
        &scene,
        &[[0.0; 3]],
        &ProbeTraceSettings {
            clamp_indirect: 0.01,
            ..base
        },
    )[0];
    assert_eq!(other.clamped, 0);
    assert_eq!(other.sh, open.sh);
}

/// `clamp_indirect` caps light after a bounce: over a sunlit ground every lit hit adds one
/// sun-bounce contribution of radiance ρ·E·sin(elev)/π; capping it at half halves E↓.
#[test]
fn clamp_indirect_caps_the_sun_bounce() {
    let (rho, e_sun, elevation) = (0.5f32, 1000.0f32, 1.0f32);
    let mut scene = scenes::sun_plane(PtMaterial::lambert([rho; 3]), elevation, e_sun, 0.0, 4);
    scene.sky = PtSky::None;
    let lp = rho * e_sun * elevation.sin() / PI;
    let s = ProbeTraceSettings {
        clamp_indirect: 0.5 * lp,
        ..settings(256, 16, 1)
    };
    let t = &trace(&scene, &[[0.0, 1.0, 0.0]], &s)[0];
    let down = irradiance(&t.sh[GiComponent::SunBounce as usize], [0.0, -1.0, 0.0])[0];
    eprintln!("clamped sun bounce: E↓ {down} want {}", 0.5 * PI * lp);
    assert!((down - 0.5 * PI * lp).abs() < 0.03 * PI * lp, "{down}");
    assert_eq!(t.clamped, t.hits, "one sun contribution per lit ground hit");
    assert!((t.clamp_loss - 0.5).abs() < 0.01, "{}", t.clamp_loss);
    // The direct cap does not touch the bounce.
    let free = &trace(
        &scene,
        &[[0.0, 1.0, 0.0]],
        &ProbeTraceSettings {
            clamp_direct: 0.01,
            ..settings(256, 16, 1)
        },
    )[0];
    assert_eq!(free.clamped, 0);
    let down = irradiance(&free.sh[GiComponent::SunBounce as usize], [0.0, -1.0, 0.0])[0];
    assert!((down - PI * lp).abs() < 0.03 * PI * lp, "{down}");
}

/// Sample groups do not change the estimate (only the float summation order), stay
/// deterministic however the probes are batched, and give a noise estimate where one sum does
/// not.
#[test]
fn sample_groups_keep_the_estimate_and_add_a_noise_estimate() {
    let scene = scenes::cornell_box(4, 4, true);
    let positions = cornell_grid();
    let one = trace(&scene, &positions, &settings(64, 8, 3));
    let grouped_settings = ProbeTraceSettings {
        sample_groups: 4,
        ..settings(64, 8, 3)
    };
    let grouped = trace(&scene, &positions, &grouped_settings);
    for (a, b) in one.iter().zip(&grouped) {
        assert_eq!(
            (a.rays, a.hits, a.back_faces, a.rejected),
            (b.rays, b.hits, b.back_faces, b.rejected)
        );
        // Summation order only: within 1e-4 of the probe's largest coefficient.
        let scale =
            a.sh.iter()
                .flatten()
                .flatten()
                .fold(0.0f32, |m, v| m.max(v.abs()));
        for (x, y) in
            a.sh.iter()
                .flatten()
                .flatten()
                .zip(b.sh.iter().flatten().flatten())
        {
            assert!((x - y).abs() <= 1e-4 * scale + 1e-7, "{x} vs {y}");
        }
        for (x, y) in a.moments.iter().zip(&b.moments) {
            assert!(
                (x[0] - y[0]).abs() < 1e-4 && (x[1] - y[1]).abs() < 1e-3,
                "{x:?} vs {y:?}"
            );
        }
        assert_eq!(a.closest_back, b.closest_back);
        assert_eq!(a.closest_front, b.closest_front);
        assert!(a.noise.is_none());
        assert!(b.noise.is_some_and(|n| n > 0.0 && n < 1.0), "{:?}", b.noise);
        assert_eq!(b.suppressed_groups, 0, "suppression is off");
    }
    let rebatched = trace(
        &scene,
        &positions,
        &ProbeTraceSettings {
            probes_per_batch: 5,
            samples_per_submit: 3,
            firefly_threshold: 4.0,
            ..grouped_settings
        },
    );
    let again = trace(
        &scene,
        &positions,
        &ProbeTraceSettings {
            firefly_threshold: 4.0,
            ..grouped_settings
        },
    );
    assert_eq!(rebatched, again, "same seed and groups, different batching");
}

/// The noise estimate falls as `1/√samples`: four times the samples halve the median noise of a
/// grid of Cornell probes. Prints the rays × samples table of `book/agents/baked-gi-bake-quality.md`.
#[test]
fn the_noise_estimate_falls_as_one_over_root_samples() {
    let scene = scenes::cornell_box(4, 4, true);
    let positions = cornell_grid();
    let mut table = Vec::new();
    for rays in [32u32, 128] {
        let mut row = Vec::new();
        for samples in [8u32, 32, 128] {
            let s = ProbeTraceSettings {
                sample_groups: 8,
                ..settings(rays, samples, 4)
            };
            let noise: Vec<f32> = trace(&scene, &positions, &s)
                .iter()
                .map(|t| t.noise.expect("noise with 8 groups"))
                .collect();
            row.push(median(&noise));
        }
        table.push((rays, row));
    }
    eprintln!("| rays per sample | 8 samples | 32 samples | 128 samples |");
    eprintln!("|---|---|---|---|");
    for (rays, row) in &table {
        eprintln!(
            "| {rays} | {:.2} % | {:.2} % | {:.2} % |",
            100.0 * row[0],
            100.0 * row[1],
            100.0 * row[2]
        );
    }
    for (rays, row) in &table {
        for w in row.windows(2) {
            let ratio = w[0] / w[1];
            assert!((1.4..=2.9).contains(&ratio), "{rays} rays: {row:?}");
        }
    }
    // More rays per sample lower the noise too.
    for (more, fewer) in table[1].1.iter().zip(&table[0].1) {
        assert!(more < fewer, "{table:?}");
    }
}

/// A rare, very bright light (a small emitter hit by a few rays of a few samples) is a firefly
/// for the median of means: the groups that hit it are scaled down to `k ×` the median group,
/// the trace reports it, and with suppression off the emitter's light is all there.
#[test]
fn firefly_suppression_scales_down_rare_bright_groups() {
    let mut scene = PtScene::new(camera());
    scene.sky = PtSky::Uniform([1.0; 3]);
    // A 0.2 m square 1 m above the probe (~0.04 sr): one hit in about one sample of five, ~6 in
    // 64 rays × 32 samples, so most single-sample groups see only the sky.
    let h = 0.1;
    let mesh = scene.add_mesh(quad([[-h, 1.0, -h], [h, 1.0, -h], [h, 1.0, h], [-h, 1.0, h]]));
    let light = scene.add_material(PtMaterial::emitter([10_000.0; 3]));
    scene.add_instance(mesh, light, Mat4::IDENTITY);
    let base = ProbeTraceSettings {
        sample_groups: 32,
        ..settings(64, 32, 1)
    };
    let plain = &trace(&scene, &[[0.0; 3]], &base)[0];
    let robust = &trace(
        &scene,
        &[[0.0; 3]],
        &ProbeTraceSettings {
            firefly_threshold: 4.0,
            ..base
        },
    )[0];
    let em_plain = mean_radiance(plain, GiComponent::Emissive);
    let em_robust = mean_radiance(robust, GiComponent::Emissive);
    eprintln!(
        "firefly: emissive {em_plain} -> {em_robust}, groups {} loss {} noise {:?} -> {:?}",
        robust.suppressed_groups, robust.firefly_loss, plain.noise, robust.noise
    );
    assert!(plain.hits > 0, "the emitter must be hit at least once");
    assert_eq!(plain.suppressed_groups, 0);
    assert!(
        robust.suppressed_groups >= 1 && robust.suppressed_groups < 16,
        "{}",
        robust.suppressed_groups
    );
    assert!(
        robust.firefly_loss > 0.5,
        "the emitter dominates: {}",
        robust.firefly_loss
    );
    assert!(em_robust < 0.1 * em_plain, "{em_robust} vs {em_plain}");
    assert!(
        robust.noise < plain.noise,
        "{:?} vs {:?}",
        robust.noise,
        plain.noise
    );
    // The groups that saw only sky are untouched: the sky stays within the share scaled down.
    let sky_plain = mean_radiance(plain, GiComponent::Sky);
    let sky_robust = mean_radiance(robust, GiComponent::Sky);
    let share = robust.suppressed_groups as f32 / 32.0;
    assert!(
        sky_robust >= (1.0 - share) * sky_plain - 1e-4,
        "{sky_robust} vs {sky_plain}"
    );
}

/// A bake reports what the controls did: the clamp share, the noise percentiles, the bounce
/// budget, and serializes them for `regia bake-gi`.
#[test]
fn the_bake_report_carries_the_quality_numbers() {
    let mut scene = PtScene::new(camera());
    scene.sky = PtSky::Uniform([2.0; 3]);
    let layout = VolumeLayout {
        terrain: None,
        bricks: vec![detail_brick([-1.0; 3], [1.0; 3], 1.0)],
        moment_resolution: 4,
        max_distance: 4.0,
    };
    let bake = bake_gi_volume(
        tracer(),
        &scene,
        &layout,
        &GiBakeSettings {
            trace: ProbeTraceSettings {
                clamp_direct: 1.0,
                sample_groups: 4,
                ..settings(32, 4, 2)
            },
            ..GiBakeSettings::default()
        },
        |_, _, _| true,
    )
    .expect("bake");
    let r = &bake.report;
    assert_eq!(r.valid, 27);
    assert_eq!(r.clamped_contributions, r.rays_traced);
    assert!(
        (r.clamp_loss_mean - 0.5).abs() < 1e-3 && (r.clamp_loss_max - 0.5).abs() < 1e-3,
        "{r:?}"
    );
    // A uniform sky: every group sees the same light.
    assert_eq!(r.noise_median, Some(0.0));
    assert_eq!(r.noise_p95, Some(0.0));
    assert_eq!((r.suppressed_groups, r.probes_with_fireflies), (0, 0));
    assert_eq!(r.bounces.max_albedo, 0.0);
    assert_eq!(r.bounces.max_bounces, 2);
    let json = serde_json::to_value(r).expect("json");
    for key in ["clamp_loss_mean", "noise_p95", "firefly_loss_max", "bounces"] {
        assert!(json.get(key).is_some(), "{key}: {json}");
    }
    assert_eq!(json["bounces"]["missing_bound"], 0.0);
}
