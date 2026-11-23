// SPDX-License-Identifier: MPL-2.0
//! Perception depth (ADR-8357): confidence, last-known motion, squad knowledge, a detection
//! meter, touch and a custom sense. Fixed steps only. No wall clock.

use regia_ai::perception_system::{
    Perceivable, Perceived, PerceptionPlugin, PerceptionSenses, PerceptionSettings, PerceptionStats, Sense,
    SightSamples, Stimulus, TeamKnowledge, Visibility,
};
use regia_ai::{AiSquad, AiSquadMember, AiSquadPlugin, Blackboard, Perception};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, RigidBody};
use regia_scene::Transform;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, PerceptionPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    app
}

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

fn spawn_agent(app: &mut App, position: Vec3, team: u8) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(position),
            Perception {
                sight_radius: 20.0,
                sight_half_angle: 45f32.to_radians(),
                hearing_radius: 15.0,
                team,
                ..Perception::default()
            },
            Perceived {
                max_age_ticks: 10,
                ..Perceived::default()
            },
            Blackboard::default(),
        ))
        .id()
}

fn spawn_target(app: &mut App, position: Vec3, team: u8) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(position),
            Perceivable {
                team,
                ..Perceivable::default()
            },
        ))
        .id()
}

fn perceived(app: &App, agent: Entity) -> &Perceived {
    app.world().get::<Perceived>(agent).expect("perceived")
}

fn set_position(app: &mut App, entity: Entity, position: Vec3) {
    app.world_mut()
        .get_mut::<Transform>(entity)
        .expect("transform")
        .translation = position;
}

/// Default half-lives are off, so a memory still drops at `max_age_ticks` and its last known
/// position does not drift. Confidence stays 1, which leaves target scores unchanged.
#[test]
fn default_settings_keep_todays_behavior() {
    let settings = PerceptionSettings::default();
    assert_eq!(settings.sight_confidence_half_life, None);
    assert_eq!(settings.hearing_confidence_half_life, None);
    assert_eq!(settings.damage_confidence_half_life, None);
    assert_eq!(settings.touch_confidence_half_life, None);
    assert_eq!(settings.detection_threshold, 0.0);
    assert_eq!(Perception::default().touch_radius, 0.0);
    assert!(!Perception::default().share_knowledge);

    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let seen_at = Vec3::new(0.0, 0.0, -5.0);
    let target = spawn_target(&mut app, seen_at, 1);
    step(&mut app, 1);
    let stimulus = perceived(&app, agent)
        .get(target, Sense::Sight)
        .expect("seen")
        .clone();
    assert_eq!(stimulus.last_known_position, seen_at);
    assert_eq!(stimulus.confidence, 1.0);
    assert_eq!(stimulus.last_known_velocity, Vec3::ZERO);

    set_position(&mut app, target, Vec3::new(0.0, 0.0, 5.0));
    step(&mut app, 10);
    let still = perceived(&app, agent)
        .get(target, Sense::Sight)
        .expect("held for max age");
    assert_eq!(
        still.last_known_position, seen_at,
        "no half-life, so the point does not drift"
    );
    assert_eq!(still.confidence, 1.0);
    step(&mut app, 1);
    assert!(perceived(&app, agent).get(target, Sense::Sight).is_none());
}

/// A half-life of one tick halves confidence and steps the last known position by the last
/// known velocity. The higher confidence wins an otherwise equal target score.
#[test]
fn confidence_decays_and_the_last_known_position_ages() {
    let mut aging = app();
    every_step(&mut aging);
    aging
        .world_mut()
        .resource_mut::<PerceptionSettings>()
        .sight_confidence_half_life = Some(1);
    let agent = spawn_agent(&mut aging, Vec3::ZERO, 0);
    let target = spawn_target(&mut aging, Vec3::new(0.0, 0.0, -5.0), 1);
    step(&mut aging, 1);
    let first = perceived(&aging, agent)
        .get(target, Sense::Sight)
        .expect("seen")
        .clone();
    assert_eq!(first.confidence, 1.0);
    assert_eq!(first.last_known_velocity, Vec3::ZERO);

    set_position(&mut aging, target, Vec3::new(0.0, 0.0, -6.0));
    step(&mut aging, 1);
    let moved = perceived(&aging, agent)
        .get(target, Sense::Sight)
        .expect("refreshed")
        .clone();
    assert_eq!(moved.confidence, 1.0);
    assert!(
        (moved.last_known_velocity - Vec3::new(0.0, 0.0, -1.0)).length() < 1e-3,
        "velocity {:?}",
        moved.last_known_velocity
    );
    assert!((moved.last_known_position.z + 6.0).abs() < 1e-3);

    aging
        .world_mut()
        .resource_mut::<PerceptionSettings>()
        .sight_checks_per_tick = 0;
    step(&mut aging, 2);
    let aged = perceived(&aging, agent)
        .get(target, Sense::Sight)
        .expect("still inside max age");
    assert!(
        (aged.confidence - 0.25).abs() < 1e-3,
        "two half-lives from 1: {}",
        aged.confidence
    );
    assert!(
        (aged.last_known_position.z + 8.0).abs() < 1e-2,
        "two steps of velocity: {}",
        aged.last_known_position
    );

    // Equal strength, equal age: the higher confidence is the target. Half-life stays off here
    // so the step does not change the authored numbers.
    let mut weighed = app();
    every_step(&mut weighed);
    weighed
        .world_mut()
        .resource_mut::<PerceptionSettings>()
        .sight_checks_per_tick = 0;
    let chooser = spawn_agent(&mut weighed, Vec3::ZERO, 0);
    let low = spawn_target(&mut weighed, Vec3::new(-2.0, 0.0, -4.0), 1);
    let high = spawn_target(&mut weighed, Vec3::new(2.0, 0.0, -4.0), 1);
    let authored = |entity, confidence| Stimulus {
        entity,
        sense: Sense::Sight,
        team: Some(1),
        last_known_position: Vec3::new(0.0, 0.0, -4.0),
        last_sensed_tick: 1,
        strength: 0.5,
        confidence,
        ..Stimulus::default()
    };
    weighed
        .world_mut()
        .get_mut::<Perceived>(chooser)
        .expect("perceived")
        .stimuli = vec![authored(low, 0.2), authored(high, 0.9)];
    step(&mut weighed, 1);
    assert_eq!(perceived(&weighed, chooser).target, high);
}

/// A squad mate with no sight of its own learns a target the watcher shared.
#[test]
fn a_squad_mate_learns_a_target_it_never_saw() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, PerceptionPlugin, AiSquadPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    every_step(&mut app);
    let squad = app
        .world_mut()
        .spawn((AiSquad::new("scouts"), Blackboard::default()))
        .id();
    let watcher = spawn_agent(&mut app, Vec3::ZERO, 0);
    let mate = spawn_agent(&mut app, Vec3::new(30.0, 0.0, 0.0), 2);
    {
        let world = app.world_mut();
        world
            .get_mut::<Perception>(watcher)
            .expect("perception")
            .share_knowledge = true;
        let mut mate_senses = world.get_mut::<Perception>(mate).expect("perception");
        mate_senses.share_knowledge = true;
        mate_senses.sight_radius = 0.0;
        mate_senses.hearing_radius = 0.0;
        world.entity_mut(watcher).insert(AiSquadMember::new(squad));
        world.entity_mut(mate).insert(AiSquadMember::new(squad));
    }
    let target = spawn_target(&mut app, Vec3::new(0.0, 0.0, -5.0), 1);
    step(&mut app, 2);
    assert!(perceived(&app, watcher).knows(target), "the watcher sees it");
    assert!(
        perceived(&app, mate).get(target, Sense::Sight).is_some(),
        "the mate learned the squad's sighting"
    );
    let shared = app.world().resource::<TeamKnowledge>().squad(squad);
    assert!(
        shared
            .iter()
            .any(|fact| fact.entity == target && fact.sense == Sense::Sight)
    );
}

fn ticks_until_known(app: &mut App, agent: Entity, target: Entity) -> u64 {
    for _ in 0..8 {
        step(app, 1);
        if perceived(app, agent).knows(target) {
            return app.world().resource::<PerceptionStats>().tick;
        }
    }
    u64::MAX
}

fn meter_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, PhysicsPlugin, PerceptionPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    every_step(&mut app);
    app.world_mut()
        .resource_mut::<PerceptionSettings>()
        .detection_threshold = 1.0;
    app
}

/// Two of four sample rays hit a wall, so the meter fills on the second tick instead of the first.
#[test]
fn a_half_hidden_target_fills_the_meter_slower() {
    let mut full = meter_app();
    let full_agent = spawn_agent(&mut full, Vec3::ZERO, 0);
    let full_target = spawn_target(&mut full, Vec3::new(0.0, 0.0, -10.0), 1);
    let full_tick = ticks_until_known(&mut full, full_agent, full_target);

    let mut half = meter_app();
    let half_agent = spawn_agent(&mut half, Vec3::ZERO, 0);
    half.world_mut()
        .entity_mut(half_agent)
        .insert(SightSamples { count: 4 });
    let half_target = half
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, -10.0)),
            Perceivable {
                team: 1,
                bounds_half_extents: Vec3::new(1.0, 1.0, 0.0),
                ..Perceivable::default()
            },
        ))
        .id();
    half.world_mut().spawn((
        Transform::from_translation(Vec3::new(2.0, 1.0, -5.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(2.0, 3.0, 0.2),
            },
            ..Collider::default()
        },
    ));
    let half_tick = ticks_until_known(&mut half, half_agent, half_target);
    assert_eq!(
        full_tick, 1,
        "a clear target fills a threshold of 1 on the first check"
    );
    assert!(
        half_tick > full_tick,
        "half the samples blocked (tick {half_tick}) is slower than a clear view (tick {full_tick})"
    );
}

/// Visibility 0.3 fills a threshold of 1 on the fourth tick. A fully lit target is seen on the first.
#[test]
fn a_dark_target_is_seen_later() {
    let mut lit = app();
    every_step(&mut lit);
    lit.world_mut()
        .resource_mut::<PerceptionSettings>()
        .detection_threshold = 1.0;
    let lit_agent = spawn_agent(&mut lit, Vec3::ZERO, 0);
    let lit_target = spawn_target(&mut lit, Vec3::new(0.0, 0.0, -5.0), 1);
    let lit_tick = ticks_until_known(&mut lit, lit_agent, lit_target);

    let mut dark = app();
    every_step(&mut dark);
    dark.world_mut()
        .resource_mut::<PerceptionSettings>()
        .detection_threshold = 1.0;
    let dark_agent = spawn_agent(&mut dark, Vec3::ZERO, 0);
    let dark_target = spawn_target(&mut dark, Vec3::new(0.0, 0.0, -5.0), 1);
    dark.world_mut()
        .entity_mut(dark_target)
        .insert(Visibility { factor: 0.3 });
    let dark_tick = ticks_until_known(&mut dark, dark_agent, dark_target);
    assert_eq!(lit_tick, 1);
    assert!(
        dark_tick > lit_tick,
        "visibility 0.3 (tick {dark_tick}) is later than a lit target (tick {lit_tick})"
    );
    assert_eq!(dark_tick, 4, "gain 0.3 reaches 1 on the fourth check");
}

/// Touch needs no line of sight. A custom scent callback reports a target behind the agent.
#[test]
fn touch_and_a_custom_sense_fire() {
    fn scent(world: &World, _observer: Entity, candidate: Entity) -> Option<f32> {
        let position = world.get::<Transform>(candidate)?.translation;
        if position.z > 4.0 { Some(0.7) } else { None }
    }

    let mut app = app();
    every_step(&mut app);
    let id = app
        .world_mut()
        .resource_mut::<PerceptionSenses>()
        .register("scent", 30.0, 1, scent)
        .expect("registered");
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    app.world_mut()
        .get_mut::<Perception>(agent)
        .expect("perception")
        .touch_radius = 2.0;
    let touched = spawn_target(&mut app, Vec3::new(0.0, 0.0, 1.0), 1);
    let smelled = spawn_target(&mut app, Vec3::new(0.0, 0.0, 8.0), 1);
    step(&mut app, 1);
    let memory = perceived(&app, agent);
    assert!(
        memory.get(touched, Sense::Touch).is_some(),
        "contact behind the facing"
    );
    assert!(
        memory.get(touched, Sense::Sight).is_none(),
        "touch does not require the cone"
    );
    let scent_hit = memory.get(smelled, Sense::Custom(id)).expect("scent");
    assert!((scent_hit.strength - 0.7).abs() < 1e-4);
    assert!(memory.get(smelled, Sense::Sight).is_none());
    assert!(
        memory.get(smelled, Sense::Touch).is_none(),
        "outside the touch radius"
    );
}

/// Sample rays count against the sight-check budget. Eight samples on several targets cannot
/// cast more rays in one tick than `sight_checks_per_tick`.
#[test]
fn ray_budget_is_respected_with_samples() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, PhysicsPlugin, PerceptionPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    {
        let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
        settings.sight_interval_ticks = 1;
        settings.sight_checks_per_tick = 16;
    }
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    app.world_mut()
        .entity_mut(agent)
        .insert(SightSamples { count: 8 });
    for i in 0..6 {
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(i as f32 * 0.3, 0.0, -6.0)),
            Perceivable {
                team: 1,
                bounds_half_extents: Vec3::splat(0.4),
                ..Perceivable::default()
            },
        ));
    }
    step(&mut app, 1);
    let stats = app.world().resource::<PerceptionStats>();
    let rays = stats.sight_rays_last_tick;
    assert!(rays > 0, "samples cast rays");
    assert!(rays <= 16, "rays {rays} stay inside the check budget");
    assert!(rays < 6 * 8, "not every sample of every target was cast");
    assert!(
        rays.is_multiple_of(8),
        "a target's samples are cast together, got {rays}"
    );
}
