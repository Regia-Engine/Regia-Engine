// SPDX-License-Identifier: MPL-2.0
//! Sight hysteresis through [`regia_ai::perception_system::PerceptionPlugin`] (ADR-4586).
//! Fixed steps only: a target in the band between the acquire radius and the lose radius is not
//! learned from an empty memory, stays for a counted run of steps once learned, and leaves on the
//! step it passes the lose radius because `max_age_ticks` is 0.

use regia_ai::Perception;
use regia_ai::perception_system::{
    LostReason, Perceivable, Perceived, PerceptionLost, PerceptionPlugin, PerceptionSettings,
    PerceptionStats, Sense,
};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::message::Messages;
use regia_scene::Transform;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, PerceptionPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    app
}

/// Runs frames until `n` more perception steps have run. The bound is a step count.
fn step(app: &mut App, n: u64) {
    let goal = app.world().resource::<PerceptionStats>().tick + n;
    for _ in 0..(n + 2) * 64 {
        if app.world().resource::<PerceptionStats>().tick >= goal {
            return;
        }
        app.update();
    }
    panic!("perception did not reach tick {goal}");
}

fn every_step(app: &mut App) {
    let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
    settings.sight_interval_ticks = 1;
    settings.sight_checks_per_tick = 10_000;
}

fn set_position(app: &mut App, entity: Entity, position: Vec3) {
    app.world_mut()
        .get_mut::<Transform>(entity)
        .expect("transform")
        .translation = position;
}

fn knows(app: &App, agent: Entity, target: Entity) -> bool {
    app.world()
        .get::<Perceived>(agent)
        .expect("perceived")
        .get(target, Sense::Sight)
        .is_some()
}

/// Acquire radius 10, lose radius 14, equal half-angles. 12 m is the band; 15 m is outside.
#[test]
fn a_target_at_12m_is_kept_for_30_steps_and_dropped_at_15m() {
    let mut app = app();
    every_step(&mut app);
    let half = std::f32::consts::FRAC_PI_2;
    let agent = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::ZERO),
            Perception {
                sight_radius: 10.0,
                sight_half_angle: half,
                sight_lose_radius: 14.0,
                sight_lose_half_angle: half,
                team: 0,
                ..Perception::default()
            },
            Perceived {
                max_age_ticks: 0,
                ..Perceived::default()
            },
        ))
        .id();
    let target = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, -12.0)),
            Perceivable {
                team: 1,
                ..Perceivable::default()
            },
        ))
        .id();

    step(&mut app, 1);
    assert!(!knows(&app, agent, target), "12 m is outside the acquire radius");

    set_position(&mut app, target, Vec3::new(0.0, 0.0, -9.0));
    step(&mut app, 1);
    assert!(knows(&app, agent, target), "9 m is inside the acquire radius");
    let acquired_at = app.world().resource::<PerceptionStats>().tick;

    set_position(&mut app, target, Vec3::new(0.0, 0.0, -12.0));
    step(&mut app, 30);
    let tick = app.world().resource::<PerceptionStats>().tick;
    assert_eq!(tick, acquired_at + 30);
    let stimulus = app
        .world()
        .get::<Perceived>(agent)
        .expect("perceived")
        .get(target, Sense::Sight)
        .expect("kept inside the lose radius")
        .clone();
    assert_eq!(stimulus.last_known_position, Vec3::new(0.0, 0.0, -12.0));
    assert_eq!(
        stimulus.last_sensed_tick, tick,
        "each of the 30 steps refreshed it"
    );

    set_position(&mut app, target, Vec3::new(0.0, 0.0, -15.0));
    step(&mut app, 1);
    assert!(
        !knows(&app, agent, target),
        "15 m leaves the lose radius on that step"
    );
    let messages = app.world().resource::<Messages<PerceptionLost>>();
    let mut cursor = messages.get_cursor();
    let lost: Vec<PerceptionLost> = cursor.read(messages).cloned().collect();
    let event = lost
        .iter()
        .find(|event| event.source == target)
        .expect("a lost message");
    assert_eq!(event.reason, LostReason::Expired);
    assert_eq!(event.agent, agent);
    assert_eq!(event.sense, Sense::Sight);
}
