// SPDX-License-Identifier: MPL-2.0
//! Spec 58 § 10: a frame-time overrun for N consecutive frames raises the `xr_over_budget`
//! diagnostic and, with `auto_scale` on, lowers the render scale one step. Frame times are scripted
//! (the simulator never reads the wall clock for this), so every count here is exact.

use regia_app::prelude::*;
use regia_xr::{
    AppliedAt, OptionValue, XrBackendKind, XrConfig, XrFrameBudget, XrOverBudget, XrPlugin, XrState,
    budget::{AUTO_SCALE_STEP, OVER_BUDGET_FRAMES, budget_ms_for, scale_down},
    drive_session_state, option_value, set_option,
    sim::ScriptState,
};

fn focused_app() -> App {
    let mut app = App::new();
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(XrPlugin::new(XrConfig {
        backend: XrBackendKind::Simulator,
        preferred_refresh_hz: Some(90.0),
        ..XrConfig::default()
    }));
    drive_session_state(app.world_mut(), ScriptState::Focused).expect("focused");
    app
}

fn script_frame_ms(app: &mut App, ms: Option<f32>) {
    app.world_mut()
        .resource_mut::<XrFrameBudget>()
        .script_frame_ms(ms);
}

fn raised(app: &mut App) -> Vec<XrOverBudget> {
    app.world_mut()
        .resource_mut::<Messages<XrOverBudget>>()
        .drain()
        .collect()
}

fn scale(app: &App) -> (f32, f32) {
    (
        app.world().resource::<XrConfig>().render_scale,
        app.world().resource::<XrState>().render_scale,
    )
}

#[test]
fn the_budget_is_nine_tenths_of_the_display_period() {
    assert!(
        (budget_ms_for(90.0) - 10.0).abs() < 1e-4,
        "{}",
        budget_ms_for(90.0)
    );
    assert!(
        (budget_ms_for(72.0) - 12.5).abs() < 1e-4,
        "{}",
        budget_ms_for(72.0)
    );
    // No refresh rate known: the 90 Hz budget.
    assert!((budget_ms_for(0.0) - 10.0).abs() < 1e-4);
    assert!((budget_ms_for(f32::NAN) - 10.0).abs() < 1e-4);
}

#[test]
fn one_step_down_stops_at_the_floor_and_lands_on_the_grid() {
    assert_eq!(scale_down(1.0), Some(0.9));
    assert_eq!(scale_down(0.9), Some(0.8));
    assert_eq!(scale_down(1.25), Some(1.15));
    assert_eq!(scale_down(0.55), Some(0.5));
    assert_eq!(scale_down(0.5), None);
    assert!((AUTO_SCALE_STEP - 0.1).abs() < f32::EPSILON);
    // Ten steps from 1.0 land exactly on the floor, never below it.
    let mut s = 1.0;
    let mut steps = 0;
    while let Some(next) = scale_down(s) {
        s = next;
        steps += 1;
    }
    assert_eq!((s, steps), (0.5, 5));
}

#[test]
fn n_consecutive_overruns_raise_the_diagnostic_once_and_a_good_frame_resets_the_run() {
    let mut app = focused_app();
    script_frame_ms(&mut app, Some(20.0));
    for _ in 0..OVER_BUDGET_FRAMES - 1 {
        app.update();
    }
    assert!(raised(&mut app).is_empty(), "N - 1 overruns raise nothing");
    // A frame inside the budget restarts the count.
    script_frame_ms(&mut app, Some(5.0));
    app.update();
    script_frame_ms(&mut app, Some(20.0));
    for _ in 0..OVER_BUDGET_FRAMES - 1 {
        app.update();
    }
    assert!(
        raised(&mut app).is_empty(),
        "the run restarted after the good frame"
    );
    app.update();
    let got = raised(&mut app);
    assert_eq!(got.len(), 1, "the Nth consecutive overrun raises it");
    let event = got[0];
    assert_eq!(event.frames, OVER_BUDGET_FRAMES);
    assert!((event.frame_ms - 20.0).abs() < 1e-4);
    assert!((event.budget_ms - 10.0).abs() < 1e-4);
    // auto_scale is off by default: the diagnostic only.
    assert_eq!((event.scale_from, event.scale_to), (1.0, 1.0));
    assert_eq!(scale(&app), (1.0, 1.0));
    assert_eq!(app.world().resource::<XrState>().stats.over_budget, 1);
    // The run restarts after a raise: the next one needs N more frames.
    for _ in 0..OVER_BUDGET_FRAMES - 1 {
        app.update();
    }
    assert!(raised(&mut app).is_empty());
    app.update();
    assert_eq!(raised(&mut app).len(), 1);
    assert_eq!(app.world().resource::<XrState>().stats.over_budget, 2);
}

#[test]
fn auto_scale_lowers_the_render_scale_one_step_per_raise_down_to_the_floor() {
    let mut app = focused_app();
    assert_eq!(
        option_value(app.world(), "auto_scale"),
        Ok(OptionValue::Bool(false))
    );
    assert_eq!(
        set_option(app.world_mut(), "auto_scale", &OptionValue::Bool(true)),
        Ok(AppliedAt::Now)
    );
    assert_eq!(
        option_value(app.world(), "auto_scale"),
        Ok(OptionValue::Bool(true))
    );
    assert_eq!(
        set_option(app.world_mut(), "auto_scale", &OptionValue::Number(1.0)).map_err(|e| e.code()),
        Err("xr_invalid_value")
    );
    script_frame_ms(&mut app, Some(30.0));
    let mut seen = Vec::new();
    for _ in 0..OVER_BUDGET_FRAMES * 7 {
        app.update();
        for e in raised(&mut app) {
            seen.push((e.scale_from, e.scale_to));
        }
    }
    assert_eq!(
        seen,
        vec![
            (1.0, 0.9),
            (0.9, 0.8),
            (0.8, 0.7),
            (0.7, 0.6),
            (0.6, 0.5),
            // At the floor the diagnostic still fires, the scale stays.
            (0.5, 0.5),
            (0.5, 0.5),
        ]
    );
    assert_eq!(scale(&app), (0.5, 0.5));
    assert_eq!(app.world().resource::<XrState>().stats.over_budget, 7);
}

#[test]
fn an_unscripted_simulator_frame_is_never_measured() {
    let mut app = focused_app();
    set_option(app.world_mut(), "auto_scale", &OptionValue::Bool(true)).expect("auto_scale");
    for _ in 0..OVER_BUDGET_FRAMES * 3 {
        app.update();
    }
    assert!(raised(&mut app).is_empty());
    let budget = app.world().resource::<XrFrameBudget>();
    assert_eq!(budget.last_frame_ms(), None);
    assert_eq!(budget.over_run(), 0);
    assert_eq!(scale(&app), (1.0, 1.0));
}

#[test]
fn frames_that_do_not_run_are_not_counted() {
    let mut app = App::new();
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(XrPlugin::new(XrConfig {
        backend: XrBackendKind::Simulator,
        ..XrConfig::default()
    }));
    // Idle: no frames flow.
    script_frame_ms(&mut app, Some(50.0));
    for _ in 0..OVER_BUDGET_FRAMES * 2 {
        app.update();
    }
    assert!(raised(&mut app).is_empty());
    assert_eq!(app.world().resource::<XrFrameBudget>().over_run(), 0);
}
