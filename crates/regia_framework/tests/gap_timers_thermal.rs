// SPDX-License-Identifier: MPL-2.0
//! Test-gap closure for `timers` and `thermal`: boundary values, clamping, owner cancellation and
//! plugin behavior with missing resources. All inputs are fixed steps; nothing reads the clock.

use std::time::Duration;

use regia_app::prelude::*;
use regia_framework::script::ScriptPlugin;
use regia_framework::thermal::{ThermalGovernor, ThermalGovernorPlugin};
use regia_framework::timers::{
    TimerHub, TimerPlugin, schedule_after, schedule_every, schedule_message_after,
};
use regia_platform::{ThermalState, ThermalStatus};
use regia_render::RenderSettings;

/// Counts callback firings.
#[derive(Resource, Default)]
struct Count(u32);

fn bump(world: &mut World) {
    world.resource_mut::<Count>().0 += 1;
}

fn count(app: &App) -> u32 {
    app.world().resource::<Count>().0
}

/// A timer app that advances 100 ms of game time per update.
fn timer_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, TimerPlugin, ScriptPlugin))
        .init_resource::<Count>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    app.world_mut().resource_mut::<FixedTime>().hz = 10.0;
    app
}

#[test]
fn zero_delay_one_shot_fires_on_the_next_update_and_is_removed() {
    let mut app = timer_app();
    schedule_after(app.world_mut(), 0.0, None, bump);
    app.update();
    assert_eq!(count(&app), 1);
    app.update();
    assert_eq!(count(&app), 1, "a one-shot never fires twice");
    assert!(app.world().resource::<TimerHub>().is_empty());
}

/// Deep-zai finding 5: the hub holds whatever the game schedules (no cap), and owner binding is
/// the cleanup path — at scale, despawning owners cancels exactly their timers, nothing else is
/// dropped, and every remaining timer stays observable through `infos` (`timers.list`).
#[test]
fn owner_despawn_cancels_only_their_timers_at_scale_and_all_stay_listed() {
    let mut app = timer_app();
    let mut owners = Vec::new();
    for k in 0..2_048u32 {
        let owner = app.world_mut().spawn_empty().id();
        schedule_after(app.world_mut(), 0.0, Some(owner), move |world| {
            world.resource_mut::<Count>().0 += 1;
        });
        if k % 2 == 0 {
            owners.push(owner);
        }
    }
    assert_eq!(app.world().resource::<TimerHub>().len(), 2_048);
    for owner in owners {
        app.world_mut().despawn(owner);
    }
    app.update();
    assert_eq!(
        count(&app),
        1_024,
        "only the owned-by-despawned timers were dropped"
    );
    assert_eq!(app.world().resource::<TimerHub>().len(), 0, "the rest fired");
    for _ in 0..64 {
        schedule_after(app.world_mut(), 5.0, None, bump);
    }
    assert_eq!(app.world().resource::<TimerHub>().infos().len(), 64);
}

#[test]
fn negative_delay_is_clamped_to_zero() {
    let mut app = timer_app();
    schedule_after(app.world_mut(), -5.0, None, bump);
    app.update();
    assert_eq!(
        count(&app),
        1,
        "a negative delay fires on the next update, not never"
    );
}

#[test]
fn two_timers_due_on_the_same_update_both_fire_once() {
    let mut app = timer_app();
    schedule_after(app.world_mut(), 0.1, None, bump);
    schedule_after(app.world_mut(), 0.1, None, bump);
    app.update();
    assert_eq!(count(&app), 2);
    app.update();
    assert_eq!(count(&app), 2);
}

#[test]
fn a_timer_not_yet_due_survives_while_another_fires() {
    let mut app = timer_app();
    schedule_after(app.world_mut(), 0.1, None, bump);
    schedule_after(app.world_mut(), 10.0, None, bump);
    app.update();
    assert_eq!(count(&app), 1);
    assert_eq!(
        app.world().resource::<TimerHub>().len(),
        1,
        "the far timer is still pending"
    );
}

#[test]
fn zero_interval_repeating_timer_fires_every_update() {
    let mut app = timer_app();
    schedule_every(app.world_mut(), 0.0, None, bump);
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(count(&app), 3);
    assert_eq!(app.world().resource::<TimerHub>().len(), 1, "it keeps repeating");
}

#[test]
fn cancel_all_only_removes_the_given_owner() {
    let mut app = timer_app();
    let a = app.world_mut().spawn(()).id();
    let b = app.world_mut().spawn(()).id();
    let mut hub = TimerHub::default();
    hub.after(1.0, Some(a), |_| {});
    hub.after(1.0, Some(a), |_| {});
    hub.every(1.0, Some(b), |_| {});
    hub.after(1.0, None, |_| {});
    assert_eq!(hub.len(), 4);
    hub.cancel_all(a);
    assert_eq!(
        hub.len(),
        2,
        "both of a's timers go; b's and the ownerless one stay"
    );
    hub.cancel_all(a);
    assert_eq!(
        hub.len(),
        2,
        "cancelling an owner with nothing scheduled is a no-op"
    );
    hub.cancel_all(b);
    hub.cancel_all(Entity::PLACEHOLDER);
    assert_eq!(hub.len(), 1);
    assert!(!hub.is_empty());
}

#[test]
fn ownerless_timers_ignore_despawns_of_unrelated_entities() {
    let mut app = timer_app();
    let other = app.world_mut().spawn(()).id();
    schedule_after(app.world_mut(), 0.3, None, bump);
    app.world_mut().despawn(other);
    // Five 100 ms steps: well past 0.3 s (three f32 steps of 0.1 stop a hair short of it).
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(count(&app), 1);
}

#[test]
fn despawned_owner_drops_a_one_shot_before_it_fires() {
    let mut app = timer_app();
    let owner = app.world_mut().spawn(()).id();
    schedule_after(app.world_mut(), 0.2, Some(owner), bump);
    app.world_mut().despawn(owner);
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(count(&app), 0);
    assert!(
        app.world().resource::<TimerHub>().is_empty(),
        "the orphan is dropped"
    );
}

#[test]
fn message_timer_with_a_despawned_owner_is_dropped_without_delivery() {
    #[derive(Reflect, Default, Clone)]
    struct Ping;

    let mut app = timer_app();
    let owner = app.world_mut().spawn(()).id();
    schedule_message_after(app.world_mut(), 0.1, Some(owner), Entity::PLACEHOLDER, Ping);
    assert_eq!(app.world().resource::<TimerHub>().len(), 1);
    app.world_mut().despawn(owner);
    app.update();
    assert!(app.world().resource::<TimerHub>().is_empty());
}

#[test]
fn scheduling_without_the_plugin_inserts_the_hub() {
    let mut world = World::new();
    assert!(world.get_resource::<TimerHub>().is_none());
    schedule_after(&mut world, 1.0, None, |_| {});
    assert_eq!(world.resource::<TimerHub>().len(), 1);
    schedule_every(&mut world, 1.0, None, |_| {});
    assert_eq!(world.resource::<TimerHub>().len(), 2);
}

#[test]
fn ticking_without_a_hub_does_not_panic() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, TimerPlugin, ScriptPlugin));
    app.world_mut().remove_resource::<TimerHub>();
    app.update();
    assert!(app.world().get_resource::<TimerHub>().is_none());
}

// ---------------------------------------------------------------------------
// Thermal governor
// ---------------------------------------------------------------------------

fn reading(status: ThermalStatus, headroom: Option<f32>) -> ThermalState {
    ThermalState { status, headroom }
}

#[test]
fn headroom_thresholds_are_inclusive_at_the_lower_edge() {
    let w = ThermalGovernor::wanted_level;
    let none = ThermalStatus::None;
    assert_eq!(w(&reading(none, Some(0.799))), 0);
    assert_eq!(w(&reading(none, Some(0.8))), 1);
    assert_eq!(w(&reading(none, Some(0.919))), 1);
    assert_eq!(w(&reading(none, Some(0.92))), 2);
    assert_eq!(w(&reading(none, Some(0.999))), 2);
    assert_eq!(w(&reading(none, Some(1.0))), 3);
    assert_eq!(
        w(&reading(none, Some(50.0))),
        3,
        "far past the threshold stays at the top level"
    );
}

#[test]
fn nan_or_negative_headroom_asks_for_no_throttling() {
    let w = ThermalGovernor::wanted_level;
    assert_eq!(w(&reading(ThermalStatus::None, Some(f32::NAN))), 0);
    assert_eq!(w(&reading(ThermalStatus::None, Some(-1.0))), 0);
}

#[test]
fn the_worst_of_status_and_headroom_wins() {
    let w = ThermalGovernor::wanted_level;
    assert_eq!(
        w(&reading(ThermalStatus::Severe, Some(0.85))),
        2,
        "status is worse"
    );
    assert_eq!(
        w(&reading(ThermalStatus::Moderate, Some(0.95))),
        2,
        "headroom is worse"
    );
    assert_eq!(w(&reading(ThermalStatus::Emergency, None)), 3);
    assert_eq!(w(&reading(ThermalStatus::Shutdown, None)), 3);
}

#[test]
fn advance_clamps_an_out_of_range_request_to_the_top_level() {
    let mut g = ThermalGovernor::default();
    assert_eq!(g.advance(200, 0.0), 3);
    assert_eq!(g.level(), 3);
}

#[test]
fn cooling_steps_down_one_level_per_relax_period() {
    let mut g = ThermalGovernor::default();
    assert_eq!(g.advance(3, 0.0), 3);
    assert_eq!(g.advance(0, 0.0), 3, "the calm timer starts");
    assert_eq!(g.advance(0, 31.0), 2);
    assert_eq!(g.advance(0, 32.0), 2, "the timer restarted at the step");
    assert_eq!(g.advance(0, 62.0), 1);
    assert_eq!(g.advance(0, 93.0), 0);
    assert_eq!(g.advance(0, 200.0), 0, "level 0 is the floor");
}

#[test]
fn a_partial_cooldown_still_counts_as_cooler() {
    let mut g = ThermalGovernor::default();
    g.advance(3, 0.0);
    assert_eq!(g.advance(1, 1.0), 3, "cooler but inside the relax period");
    assert_eq!(
        g.advance(1, 31.0),
        2,
        "after the period it steps down once, not to the wanted level"
    );
}

#[test]
fn a_request_equal_to_the_current_level_resets_the_calm_timer() {
    let mut g = ThermalGovernor::default();
    g.advance(2, 0.0);
    g.advance(0, 10.0); // calm since 10
    assert_eq!(g.advance(2, 25.0), 2, "heat is back at the same level");
    assert_eq!(g.advance(0, 26.0), 2, "the timer restarted");
    assert_eq!(g.advance(0, 50.0), 2, "only 24 s since the restart");
    assert_eq!(g.advance(0, 56.0), 1);
}

#[test]
fn custom_relax_seconds_is_honored() {
    let mut g = ThermalGovernor::default();
    g.relax_seconds = 5.0;
    g.advance(1, 0.0);
    g.advance(0, 0.0);
    assert_eq!(g.advance(0, 4.9), 1);
    assert_eq!(g.advance(0, 5.0), 0, "the limit itself is enough");
}

#[test]
fn scale_for_uses_custom_factors_and_never_exceeds_the_base() {
    let mut g = ThermalGovernor::default();
    g.factors = [1.0, 0.5, 0.5, 0.5];
    g.min_scale = 0.1;
    assert_eq!(
        g.scale_for(0.3),
        0.3,
        "level 0 returns the base even under the floor"
    );
    g.advance(1, 0.0);
    assert!((g.scale_for(1.0) - 0.5).abs() < 1e-6);
    assert!(
        (g.scale_for(0.15) - 0.1).abs() < 1e-6,
        "the floor lifts a tiny product"
    );

    let mut floor = ThermalGovernor::default();
    floor.advance(1, 0.0);
    assert!(
        (floor.scale_for(0.4) - 0.4).abs() < 1e-6,
        "a base already under the floor is left alone"
    );
}

#[test]
fn a_fresh_governor_holds_nothing() {
    let g = ThermalGovernor::default();
    assert!(g.enabled);
    assert_eq!(g.level(), 0);
    assert_eq!(g.base_scale(), None);
}

fn thermal_app(scale: f32) -> App {
    let mut app = App::new();
    let mut settings = RenderSettings::default();
    settings.profile.render_scale = scale;
    app.insert_resource(settings)
        .init_resource::<ThermalState>()
        .add_plugins(ThermalGovernorPlugin);
    app
}

fn render_scale(app: &App) -> f32 {
    app.world().resource::<RenderSettings>().profile.render_scale
}

#[test]
fn base_scale_is_reported_only_while_the_governor_holds_the_scale_down() {
    let mut app = thermal_app(0.9);
    app.update();
    assert_eq!(app.world().resource::<ThermalGovernor>().base_scale(), None);
    *app.world_mut().resource_mut::<ThermalState>() = reading(ThermalStatus::Moderate, None);
    app.update();
    let g = app.world().resource::<ThermalGovernor>();
    assert_eq!(g.level(), 1);
    assert_eq!(g.base_scale(), Some(0.9));
    assert!((render_scale(&app) - 0.9 * 0.85).abs() < 1e-5);
}

#[test]
fn plugin_without_render_settings_does_not_panic() {
    let mut app = App::new();
    app.init_resource::<ThermalState>()
        .add_plugins(ThermalGovernorPlugin);
    *app.world_mut().resource_mut::<ThermalState>() = reading(ThermalStatus::Critical, None);
    app.update();
    assert_eq!(
        app.world().resource::<ThermalGovernor>().level(),
        0,
        "nothing to govern, nothing changes"
    );
}

#[test]
fn plugin_without_a_thermal_state_leaves_the_scale_alone() {
    let mut app = App::new();
    let mut settings = RenderSettings::default();
    settings.profile.render_scale = 0.8;
    app.insert_resource(settings).add_plugins(ThermalGovernorPlugin);
    app.update();
    app.update();
    assert_eq!(render_scale(&app), 0.8);
    assert_eq!(app.world().resource::<ThermalGovernor>().level(), 0);
}

#[test]
fn a_disabled_governor_ignores_heat() {
    let mut app = thermal_app(1.0);
    app.world_mut().resource_mut::<ThermalGovernor>().enabled = false;
    *app.world_mut().resource_mut::<ThermalState>() = reading(ThermalStatus::Emergency, Some(2.0));
    app.update();
    assert_eq!(render_scale(&app), 1.0);
    assert_eq!(app.world().resource::<ThermalGovernor>().level(), 0);
}

#[test]
fn the_deepest_level_applies_its_factor_and_the_floor() {
    let mut app = thermal_app(1.0);
    *app.world_mut().resource_mut::<ThermalState>() = reading(ThermalStatus::Critical, None);
    app.update();
    assert!((render_scale(&app) - 0.55).abs() < 1e-5);
    assert_eq!(app.world().resource::<ThermalGovernor>().level(), 3);
}
