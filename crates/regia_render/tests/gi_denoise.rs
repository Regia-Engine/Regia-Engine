// SPDX-License-Identifier: MPL-2.0
//! GI denoise regression tests on the CPU reference (ADR-864, `book/agents/gi-denoise.md`).
//!
//! Every scenario is a deterministic synthetic sequence with a known ground truth
//! (`regia_render::gi_denoise::scenario`), so each bound below is a fixed number, not a
//! statistical one. The bounds carry margin over the values measured when the filter landed
//! (the comments give the value at landing): they catch a regression (noise, flicker, leaking across
//! a depth edge, smearing a shadow edge, a history that is no longer rejected, lag), not
//! rounding. No GPU, no wall clock.

use regia_render::gi_denoise::scenario::{GiDenoiseReport, GiScenario, GiScenarioKind, run_scenario};
use regia_render::gi_denoise::{GiDenoiseSettings, GiDenoiser};

fn report(scenario: &GiScenario, settings: &GiDenoiseSettings) -> GiDenoiseReport {
    run_scenario(scenario, settings).expect("scenario runs")
}

fn default_report(kind: GiScenarioKind) -> GiDenoiseReport {
    report(&GiScenario::new(kind), &GiDenoiseSettings::default())
}

#[test]
fn static_view_converges_without_flicker_bias_or_leak() {
    let r = default_report(GiScenarioKind::Static);
    // The raw input is one exponential sample per pixel: error and flicker about 1.
    assert!(r.raw_rel_rmse > 1.0 && r.raw_flicker > 0.9, "{r:?}");
    // Noise: 1.27 -> 0.076 at landing.
    assert!(r.rel_rmse < 0.12, "converged error {r:?}");
    assert!(r.raw_rel_rmse / r.rel_rmse > 10.0, "noise reduction {r:?}");
    assert!(r.max_rel_rmse < 0.13, "worst frame {r:?}");
    // Flicker: 1.0 -> 0.012.
    assert!(r.flicker < 0.025, "flicker {r:?}");
    // Energy: -1.4 % (the luminance weight favours the darker side of the noise).
    assert!(r.bias.abs() < 0.04, "bias {r:?}");
    // The box 8x brighter or darker changes nothing beside it: exactly 0 at landing.
    let leak = r.edge_leak.expect("leak scored");
    assert!(leak.abs() < 0.005, "leak across the box's depth edge {r:?}");
    // Shadow edges: raw 0.74, filtered 0.19.
    let edge = r.shadow_edge_error.expect("edge scored");
    let raw_edge = r.raw_shadow_edge_error.expect("raw edge scored");
    assert!(edge < 0.3 && edge < raw_edge / 2.5, "shadow edge smeared {r:?}");
    // Every pixel keeps its history (27.7 of 32 at landing: the first frames ramp up, and
    // anti-lag now and then shortens a square's history on noise alone).
    assert!(r.mean_history > 25.0, "history {r:?}");
    assert_eq!(r.scored_frames, 32);
}

#[test]
fn panning_view_reprojects_its_history() {
    let r = default_report(GiScenarioKind::Pan);
    // 0.135 mean, 0.17 worst at landing; half the screen's history is younger than 32
    // frames because a new column enters each frame.
    assert!(r.rel_rmse < 0.2, "{r:?}");
    assert!(r.max_rel_rmse < 0.25, "{r:?}");
    assert!(r.flicker < 0.03, "{r:?}");
    assert!(r.bias.abs() < 0.07, "{r:?}");
    assert!(r.edge_leak.expect("leak").abs() < 0.005, "{r:?}");
    // History survives the motion (22 at landing; without reprojection it would be 1).
    assert!(r.mean_history > 18.0, "{r:?}");
}

#[test]
fn panning_the_other_way_faster_also_holds() {
    let scenario = GiScenario {
        pan_px: -2,
        ..GiScenario::new(GiScenarioKind::Pan)
    };
    let r = report(&scenario, &GiDenoiseSettings::default());
    // 0.129 / 0.21 / 0.023 / 15.8 at landing.
    assert!(
        r.rel_rmse < 0.2 && r.max_rel_rmse < 0.3 && r.flicker < 0.04,
        "{r:?}"
    );
    assert!(r.edge_leak.expect("leak").abs() < 0.005, "{r:?}");
    assert!(r.mean_history > 12.0, "{r:?}");
}

#[test]
fn disoccluded_pixels_drop_the_history_and_are_filtered_spatially() {
    let r = default_report(GiScenarioKind::Disocclusion);
    assert!(r.rel_rmse < 0.12 && r.flicker < 0.025, "{r:?}");
    assert!(r.edge_leak.expect("leak").abs() < 0.005, "{r:?}");
    // The column the box uncovers each frame has no history: the spatial pass alone takes it
    // from the raw 1.27 to 0.25. A reprojection that kept the box's history there would show
    // the box's 4x brighter light (an error above 2).
    let fresh = r.disoccluded_rel_rmse.expect("disoccluded pixels scored");
    assert!(fresh < 0.4, "disoccluded {r:?}");
    let fast = report(
        &GiScenario {
            box_speed: 3,
            ..GiScenario::new(GiScenarioKind::Disocclusion)
        },
        &GiDenoiseSettings::default(),
    );
    assert!(fast.rel_rmse < 0.14, "{fast:?}");
    assert!(fast.disoccluded_rel_rmse.expect("scored") < 0.4, "{fast:?}");
    assert!(fast.edge_leak.expect("leak").abs() < 0.005, "{fast:?}");
}

#[test]
fn a_lighting_change_settles_within_a_few_frames() {
    let r = default_report(GiScenarioKind::LightStep);
    // The anti-lag squares see the light quadruple and let the new frame through: 2 frames at
    // landing.
    let lag = r.lag_frames.expect("the output reaches the new light");
    assert!(lag <= 4, "lag {lag} frames: {r:?}");
}

#[test]
fn without_anti_lag_the_history_alone_sets_the_lag() {
    let off = GiDenoiseSettings {
        antilag_sigma: 0.0,
        ..GiDenoiseSettings::default()
    };
    let r = report(&GiScenario::new(GiScenarioKind::LightStep), &off);
    // alpha 0.2 from a quarter to full light: 0.8^n * 0.75 < 0.1 at n = 9.
    let lag = r.lag_frames.expect("the output reaches the new light");
    assert!((6..=12).contains(&lag), "lag {lag} frames: {r:?}");
}

#[test]
fn anti_lag_costs_no_flicker_and_leaks_nothing() {
    let off = GiDenoiseSettings {
        antilag_sigma: 0.0,
        ..GiDenoiseSettings::default()
    };
    for kind in [
        GiScenarioKind::Static,
        GiScenarioKind::Pan,
        GiScenarioKind::Disocclusion,
    ] {
        let on = default_report(kind);
        let without = report(&GiScenario::new(kind), &off);
        // 0.0119 vs 0.0118 (static), 0.0171 vs 0.0169 (pan), 0.0130 vs 0.0129 at landing: the
        // squares almost never fire on noise alone.
        assert!(
            on.flicker < without.flicker * 1.15,
            "{} {on:?} vs {without:?}",
            kind.name()
        );
        assert!(
            on.rel_rmse < without.rel_rmse * 1.1,
            "{} {on:?} vs {without:?}",
            kind.name()
        );
        assert!(
            on.edge_leak.expect("leak").abs() < 0.005,
            "{} {on:?}",
            kind.name()
        );
    }
}

#[test]
fn alpha_trades_flicker_for_lag() {
    // Anti-lag off on both sides: the trade is the history weight's alone.
    let base = GiDenoiseSettings {
        antilag_sigma: 0.0,
        ..GiDenoiseSettings::default()
    };
    let slow = GiDenoiseSettings {
        alpha_color: 0.1,
        alpha_moments: 0.1,
        ..base.clone()
    };
    let base_static = report(&GiScenario::new(GiScenarioKind::Static), &base);
    let slow_static = report(&GiScenario::new(GiScenarioKind::Static), &slow);
    assert!(
        slow_static.flicker < base_static.flicker * 0.75,
        "a longer history flickers less: {slow_static:?} vs {base_static:?}"
    );
    let base_lag = report(&GiScenario::new(GiScenarioKind::LightStep), &base)
        .lag_frames
        .expect("lag");
    let slow_lag = report(&GiScenario::new(GiScenarioKind::LightStep), &slow)
        .lag_frames
        .expect("lag");
    // 9 -> 19 at landing.
    assert!(
        slow_lag > base_lag + 5,
        "and follows a change more slowly: {slow_lag} vs {base_lag}"
    );
    // With anti-lag the slow history follows the light as fast as the default: 2 frames.
    let slow_antilag = GiDenoiseSettings {
        antilag_sigma: 5.0,
        ..slow
    };
    let lag = report(&GiScenario::new(GiScenarioKind::LightStep), &slow_antilag)
        .lag_frames
        .expect("lag");
    assert!(lag <= 4, "anti-lag with alpha 0.1: {lag}");
}

#[test]
fn the_spatial_filter_carries_most_of_the_noise_reduction() {
    let temporal_only = GiDenoiseSettings {
        atrous_iterations: 0,
        ..GiDenoiseSettings::default()
    };
    let t = report(&GiScenario::new(GiScenarioKind::Static), &temporal_only);
    let full = default_report(GiScenarioKind::Static);
    // 0.42 -> 0.076 and 0.156 -> 0.012 at landing.
    assert!(t.rel_rmse > 3.0 * full.rel_rmse, "{t:?} vs {full:?}");
    assert!(t.flicker > 5.0 * full.flicker, "{t:?} vs {full:?}");
    // Temporal accumulation alone is a plain average: nearly unbiased.
    assert!(t.bias.abs() < 0.01, "{t:?}");
}

#[test]
fn more_samples_per_pixel_give_a_cleaner_result() {
    let four = report(
        &GiScenario {
            spp: 4,
            ..GiScenario::new(GiScenarioKind::Static)
        },
        &GiDenoiseSettings::default(),
    );
    let one = default_report(GiScenarioKind::Static);
    // 0.047 vs 0.076 at landing.
    assert!(four.raw_rel_rmse < 0.6 * one.raw_rel_rmse, "{four:?}");
    assert!(four.rel_rmse < one.rel_rmse, "{four:?} vs {one:?}");
    assert!(four.bias.abs() < one.bias.abs() + 0.005, "{four:?} vs {one:?}");
}

#[test]
fn feeding_back_the_first_pass_still_converges() {
    // Schied et al.'s history feedback, off by default (ADR-864): 0.091 at landing.
    let settings = GiDenoiseSettings {
        history_from_first_pass: true,
        ..GiDenoiseSettings::default()
    };
    let r = report(&GiScenario::new(GiScenarioKind::Static), &settings);
    assert!(r.rel_rmse < 0.15 && r.flicker < 0.025, "{r:?}");
    assert!(r.edge_leak.expect("leak").abs() < 0.005, "{r:?}");
}

#[test]
fn a_camera_cut_restarts_from_the_spatial_filter() {
    let scenario = GiScenario::new(GiScenarioKind::Static);
    let mut d = GiDenoiser::new(GiDenoiseSettings::default()).expect("denoiser");
    for t in 0..20 {
        d.denoise(&scenario.frame(t).input()).expect("frame");
    }
    d.reset();
    let f = scenario.frame(20);
    let out = d.denoise(&f.input()).expect("frame after the cut");
    assert!(out.history_len.iter().all(|l| *l == 1.0));
    // One frame, spatial filtering only: 0.128 against the raw 1.17 at landing.
    let cut =
        regia_render::gi_denoise::metrics::relative_rmse(&out.color, &f.truth, &f.surface).expect("scored");
    let raw =
        regia_render::gi_denoise::metrics::relative_rmse(&f.noisy, &f.truth, &f.surface).expect("scored");
    assert!(cut < raw / 4.0, "cut {cut} raw {raw}");
}

#[test]
fn every_scenario_is_reproducible() {
    for kind in GiScenarioKind::ALL {
        let a = default_report(kind);
        let b = default_report(kind);
        assert_eq!(a, b, "{}", kind.name());
    }
}
