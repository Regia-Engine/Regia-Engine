// SPDX-License-Identifier: MPL-2.0
//! `PerceptionPlugin` end to end (ADR-2565): fixed steps only, no wall clock.

use regia_ai::perception_system::{
    Attitude, Attitudes, DamageEvent, LostReason, Perceivable, Perceived, PerceptionGained, PerceptionLost,
    PerceptionPlugin, PerceptionSettings, PerceptionStats, Sense, Stimulus,
};
use regia_ai::{
    AiPlugin, BbValue, BehaviorTree, BehaviorTreeBuilder, BehaviorTreeStatus, Blackboard, BtStatus,
    BtTaskRegistry, HearingEvent, Perception, StateTree, StateTreeDef, StateTreeStatus,
};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::message::Messages;
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, RigidBody};
use regia_scene::Transform;
use regia_scene::layers::LayerParked;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, PerceptionPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    app
}

/// Runs frames until `n` more perception steps have run.
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

/// Sight every step and no budget in the way, so a test reads one step at a time.
fn every_step(app: &mut App) {
    let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
    settings.sight_interval_ticks = 1;
    settings.sight_checks_per_tick = 10_000;
}

/// An agent at `position` facing -Z on `team`.
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

fn gained(app: &App) -> Vec<PerceptionGained> {
    let messages = app.world().resource::<Messages<PerceptionGained>>();
    let mut cursor = messages.get_cursor();
    cursor.read(messages).cloned().collect()
}

fn lost(app: &App) -> Vec<PerceptionLost> {
    let messages = app.world().resource::<Messages<PerceptionLost>>();
    let mut cursor = messages.get_cursor();
    cursor.read(messages).cloned().collect()
}

fn set_position(app: &mut App, entity: Entity, position: Vec3) {
    app.world_mut()
        .get_mut::<Transform>(entity)
        .expect("transform")
        .translation = position;
}

/// A rules [`regia_framework::rules::TeamId`] is copied onto [`Perceivable`] before sight, so the
/// attitude (friendly on the same team, hostile on another) follows the component.
#[test]
fn team_id_reaches_ai_perception() {
    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 1);
    let target = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, -5.0)),
            Perceivable {
                team: 0,
                ..Perceivable::default()
            },
            regia_framework::rules::TeamId(1),
        ))
        .id();
    step(&mut app, 1);
    assert_eq!(
        app.world().get::<Perceivable>(target).expect("perceivable").team,
        1
    );
    assert!(
        perceived(&app, agent).get(target, Sense::Sight).is_none(),
        "the same team is friendly and the default filter drops friendlies"
    );
    app.world_mut()
        .get_mut::<regia_framework::rules::TeamId>(target)
        .expect("team")
        .0 = 2;
    step(&mut app, 1);
    assert_eq!(
        app.world().get::<Perceivable>(target).expect("perceivable").team,
        2
    );
    let stimulus = perceived(&app, agent)
        .get(target, Sense::Sight)
        .expect("a different team is hostile");
    assert_eq!(stimulus.team, Some(2));
}

/// Acceptance: an agent sees a target in its cone, and forgets it after max-age once out of sight.
#[test]
fn an_agent_sees_a_target_in_its_cone_and_forgets_it_after_max_age() {
    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let seen_at = Vec3::new(0.0, 0.0, -5.0);
    let target = spawn_target(&mut app, seen_at, 1);
    step(&mut app, 1);

    let stimulus = perceived(&app, agent)
        .get(target, Sense::Sight)
        .expect("in the cone")
        .clone();
    assert_eq!(stimulus.last_known_position, seen_at);
    assert_eq!(stimulus.team, Some(1));
    assert!(stimulus.strength > 0.0 && stimulus.strength <= 1.0);
    assert_eq!(perceived(&app, agent).target, target);
    let events = gained(&app);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!((events[0].agent, events[0].source), (agent, target));
    assert_eq!(events[0].sense, Sense::Sight);

    // Behind the agent: out of sight, so the memory keeps the old position and ages.
    set_position(&mut app, target, Vec3::new(0.0, 0.0, 5.0));
    let last_seen = app.world().resource::<PerceptionStats>().tick;
    step(&mut app, 10);
    let still = perceived(&app, agent)
        .get(target, Sense::Sight)
        .expect("remembered up to and including max_age ticks");
    assert_eq!(still.last_known_position, seen_at);
    assert_eq!(still.last_sensed_tick, last_seen);

    step(&mut app, 1);
    assert!(
        perceived(&app, agent).get(target, Sense::Sight).is_none(),
        "forgotten past max_age"
    );
    assert_eq!(perceived(&app, agent).target, Entity::PLACEHOLDER);
    let events = lost(&app);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].source, target);
    assert_eq!(events[0].reason, LostReason::Expired);
}

#[test]
fn a_target_outside_the_cone_or_range_is_not_seen() {
    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let behind = spawn_target(&mut app, Vec3::new(0.0, 0.0, 5.0), 1);
    let far = spawn_target(&mut app, Vec3::new(0.0, 0.0, -40.0), 1);
    let beside = spawn_target(&mut app, Vec3::new(10.0, 0.0, -1.0), 1);
    step(&mut app, 3);
    let memory = perceived(&app, agent);
    assert!(!memory.knows(behind) && !memory.knows(far) && !memory.knows(beside));
    assert_eq!(memory.stimuli, Vec::<Stimulus>::new());
}

/// The agent turning is what moves the cone: the facing comes from its rotation.
#[test]
fn turning_toward_a_target_brings_it_into_sight() {
    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let target = spawn_target(&mut app, Vec3::new(0.0, 0.0, 5.0), 1);
    step(&mut app, 1);
    assert!(!perceived(&app, agent).knows(target));
    app.world_mut()
        .get_mut::<Transform>(agent)
        .expect("transform")
        .rotation = Quat::from_rotation_y(std::f32::consts::PI);
    step(&mut app, 1);
    assert!(perceived(&app, agent).knows(target));
}

/// Line of sight goes through the physics world: a wall hides the target; lifting it reveals it.
#[test]
fn a_wall_blocks_sight_through_the_physics_world() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, PhysicsPlugin, PerceptionPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let target = spawn_target(&mut app, Vec3::new(0.0, 0.0, -10.0), 1);
    let wall = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 1.0, -5.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(4.0, 4.0, 0.2),
                },
                ..Collider::default()
            },
        ))
        .id();
    step(&mut app, 6);
    assert!(!perceived(&app, agent).knows(target), "behind the wall");
    let stats = app.world().resource::<PerceptionStats>();
    assert_eq!(stats.sight_rays_last_tick, 1, "the cone passed, the ray ran");

    app.world_mut().despawn(wall);
    step(&mut app, 6);
    assert!(perceived(&app, agent).knows(target), "the wall is gone");
}

/// The target's and the agent's own colliders, and those of their children, never hide the target.
#[test]
fn own_and_target_colliders_do_not_block_the_line() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, PhysicsPlugin, PerceptionPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    every_step(&mut app);
    let body = || {
        (
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(3.0),
                },
                ..Collider::default()
            },
        )
    };
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    app.world_mut().entity_mut(agent).insert(body());
    let target = spawn_target(&mut app, Vec3::new(0.0, 0.0, -18.0), 1);
    app.world_mut().entity_mut(target).insert(body());
    // A child of the target standing in the way.
    app.world_mut().spawn((
        // Local to the target: at z = -12 in the world, between the two.
        Transform::from_translation(Vec3::new(0.0, 0.0, 6.0)),
        ChildOf(target),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::splat(1.5),
            },
            ..Collider::default()
        },
    ));
    step(&mut app, 8);
    assert!(perceived(&app, agent).knows(target));
}

/// Acceptance: hearing works across teams per the attitude table.
#[test]
fn hearing_works_across_teams_per_the_attitude_table() {
    let mut app = app();
    every_step(&mut app);
    let listener = spawn_agent(&mut app, Vec3::ZERO, 0);
    let source = app.world_mut().spawn_empty().id();
    let noise = |team: u8| HearingEvent {
        position: Vec3::new(5.0, 0.0, 5.0),
        radius: 12.0,
        team,
        source,
    };

    // Default table: another team is hostile, so it is heard (the old same-team rule refused this).
    app.world_mut().write_message(noise(1));
    step(&mut app, 1);
    let heard = perceived(&app, listener)
        .get(source, Sense::Hearing)
        .expect("a hostile team is heard")
        .clone();
    assert_eq!(heard.last_known_position, Vec3::new(5.0, 0.0, 5.0));
    assert_eq!(heard.team, Some(1));
    assert!(perceived(&app, listener).get(source, Sense::Sight).is_none());

    // Friendly by the table: not registered with the default filter.
    let mut app = self::app();
    every_step(&mut app);
    let listener = spawn_agent(&mut app, Vec3::ZERO, 0);
    let source = app.world_mut().spawn_empty().id();
    let noise = |team: u8| HearingEvent {
        position: Vec3::new(5.0, 0.0, 5.0),
        radius: 12.0,
        team,
        source,
    };
    app.world_mut()
        .resource_mut::<Attitudes>()
        .set(0, 1, Attitude::Friendly);
    app.world_mut().write_message(noise(1));
    step(&mut app, 1);
    assert!(!perceived(&app, listener).knows(source), "friendly by the table");

    // Neutral by the table: heard, but not a target.
    app.world_mut()
        .resource_mut::<Attitudes>()
        .set(0, 1, Attitude::Neutral);
    app.world_mut().write_message(noise(1));
    step(&mut app, 1);
    assert!(perceived(&app, listener).knows(source), "neutral is heard");
    assert_eq!(perceived(&app, listener).target, Entity::PLACEHOLDER);

    // The agent's own team is friendly by default, unless its filter asks for friends.
    let friend = app.world_mut().spawn_empty().id();
    let own = HearingEvent {
        source: friend,
        ..noise(0)
    };
    app.world_mut().write_message(own.clone());
    step(&mut app, 1);
    assert!(!perceived(&app, listener).knows(friend));
    app.world_mut()
        .get_mut::<Perceived>(listener)
        .expect("perceived")
        .detect
        .friendly = true;
    app.world_mut().write_message(own);
    step(&mut app, 1);
    assert!(perceived(&app, listener).knows(friend));
}

#[test]
fn hearing_stays_inside_both_radii_and_an_agent_does_not_hear_itself() {
    let mut app = app();
    every_step(&mut app);
    let listener = spawn_agent(&mut app, Vec3::ZERO, 0);
    let source = app.world_mut().spawn_empty().id();
    // Beyond the event's carry radius, then beyond the listener's.
    for (position, radius) in [(Vec3::X * 8.0, 2.0), (Vec3::X * 16.0, 50.0)] {
        app.world_mut().write_message(HearingEvent {
            position,
            radius,
            team: 1,
            source,
        });
    }
    // Its own footsteps.
    app.world_mut().write_message(HearingEvent {
        position: Vec3::ZERO,
        radius: 5.0,
        team: 1,
        source: listener,
    });
    step(&mut app, 1);
    assert_eq!(perceived(&app, listener).stimuli, Vec::<Stimulus>::new());
    assert_eq!(app.world().resource::<PerceptionStats>().hearing_events, 3);
}

/// An anonymous sound (no source entity) is remembered in one entry and never becomes the target.
#[test]
fn an_anonymous_sound_shares_one_entry_and_is_not_a_target() {
    let mut app = app();
    every_step(&mut app);
    let listener = spawn_agent(&mut app, Vec3::ZERO, 0);
    for x in [3.0, 4.0] {
        app.world_mut().write_message(HearingEvent {
            position: Vec3::X * x,
            radius: 10.0,
            team: 1,
            source: Entity::PLACEHOLDER,
        });
    }
    step(&mut app, 1);
    let memory = perceived(&app, listener);
    assert_eq!(memory.stimuli.len(), 1);
    assert_eq!(
        memory.stimuli[0].last_known_position,
        Vec3::X * 4.0,
        "the latest wins"
    );
    assert_eq!(memory.target, Entity::PLACEHOLDER);
}

/// Acceptance: a damage event registers the attacker even outside the cone.
#[test]
fn a_damage_event_registers_the_attacker_outside_the_cone() {
    let mut app = app();
    every_step(&mut app);
    let victim = spawn_agent(&mut app, Vec3::ZERO, 0);
    let behind = Vec3::new(0.0, 0.0, 15.0);
    let attacker = spawn_target(&mut app, behind, 1);
    step(&mut app, 1);
    assert!(!perceived(&app, victim).knows(attacker), "behind the agent");

    app.world_mut().write_message(DamageEvent {
        victim,
        attacker,
        position: behind,
        amount: 12.5,
    });
    step(&mut app, 1);
    let memory = perceived(&app, victim);
    let stimulus = memory
        .get(attacker, Sense::Damage)
        .expect("the attacker is registered");
    assert_eq!(stimulus.last_known_position, behind);
    assert_eq!(stimulus.team, Some(1));
    assert!((stimulus.strength - 0.5).abs() < 1e-6, "{}", stimulus.strength);
    assert!(memory.get(attacker, Sense::Sight).is_none(), "still not seen");
    assert_eq!(memory.target, attacker, "whoever shot me is my target");
    let events = gained(&app);
    assert!(
        events
            .iter()
            .any(|e| e.agent == victim && e.source == attacker && e.sense == Sense::Damage),
        "{events:?}"
    );
}

/// A wound from a friend registers but does not make the friend a target; one from a neutral does.
#[test]
fn damage_ignores_the_filter_and_targets_follow_the_attitude() {
    let mut app = app();
    every_step(&mut app);
    let victim = spawn_agent(&mut app, Vec3::ZERO, 0);
    let friend = spawn_target(&mut app, Vec3::new(0.0, 0.0, 5.0), 0);
    let neutral = spawn_target(&mut app, Vec3::new(0.0, 0.0, 9.0), 2);
    app.world_mut()
        .resource_mut::<Attitudes>()
        .set(0, 2, Attitude::Neutral);
    app.world_mut().write_message(DamageEvent {
        victim,
        attacker: friend,
        position: Vec3::new(0.0, 0.0, 5.0),
        amount: 25.0,
    });
    step(&mut app, 1);
    assert!(perceived(&app, victim).get(friend, Sense::Damage).is_some());
    assert_eq!(
        perceived(&app, victim).target,
        Entity::PLACEHOLDER,
        "not a friend"
    );

    app.world_mut().write_message(DamageEvent {
        victim,
        attacker: neutral,
        position: Vec3::new(0.0, 0.0, 9.0),
        amount: 1.0,
    });
    step(&mut app, 1);
    assert_eq!(perceived(&app, victim).target, neutral);
}

/// Attacker and victim the same, no attacker, junk numbers: none of them panics or stores a NaN.
#[test]
fn damage_edge_cases() {
    let mut app = app();
    every_step(&mut app);
    let victim = spawn_agent(&mut app, Vec3::ZERO, 0);
    let attacker = app.world_mut().spawn_empty().id();
    let base = DamageEvent {
        victim,
        attacker,
        position: Vec3::X,
        amount: 1.0,
    };
    for event in [
        DamageEvent {
            attacker: victim,
            ..base.clone()
        },
        DamageEvent {
            position: Vec3::NAN,
            ..base.clone()
        },
        DamageEvent {
            victim: attacker, // Not an agent.
            attacker: victim,
            ..base.clone()
        },
    ] {
        app.world_mut().write_message(event);
    }
    step(&mut app, 1);
    assert_eq!(perceived(&app, victim).stimuli, Vec::<Stimulus>::new());

    app.world_mut().write_message(DamageEvent {
        amount: f32::NAN,
        ..base.clone()
    });
    step(&mut app, 1);
    let stimulus = perceived(&app, victim)
        .get(attacker, Sense::Damage)
        .expect("registered");
    assert_eq!(stimulus.strength, 0.0, "a NaN amount reads as 0");
    assert_eq!(stimulus.team, None, "no team component on the attacker");

    // A fall: nobody did it.
    app.world_mut().write_message(DamageEvent {
        attacker: Entity::PLACEHOLDER,
        ..base
    });
    step(&mut app, 1);
    assert!(
        perceived(&app, victim)
            .get(Entity::PLACEHOLDER, Sense::Damage)
            .is_some()
    );
}

/// Acceptance: the sight budget caps checks per step at the configured count, and round-robin still
/// reaches every pair.
#[test]
fn the_sight_budget_caps_checks_per_step_and_round_robin_reaches_every_pair() {
    let mut app = app();
    {
        let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
        settings.sight_interval_ticks = 1;
        settings.sight_checks_per_tick = 7;
    }
    let agents: Vec<Entity> = (0..10)
        .map(|i| spawn_agent(&mut app, Vec3::new(i as f32 * 0.1, 0.0, 0.0), 0))
        .collect();
    let targets: Vec<Entity> = (0..10)
        .map(|i| spawn_target(&mut app, Vec3::new(i as f32 * 0.1, 0.0, -5.0), 1))
        .collect();
    for &agent in &agents {
        let mut memory = app.world_mut().get_mut::<Perceived>(agent).expect("perceived");
        memory.max_age_ticks = 1000;
    }
    // 100 pairs at 7 a step: 15 steps (14 full steps, 2 pairs left over).
    for tick in 1..=14 {
        step(&mut app, 1);
        let stats = app.world().resource::<PerceptionStats>();
        assert_eq!(stats.sight_checks_last_tick, 7, "step {tick}");
        assert_eq!(stats.sight_checks_total, 7 * tick);
        assert_eq!(stats.sweeps_completed, 0, "step {tick}");
    }
    step(&mut app, 1);
    let stats = app.world().resource::<PerceptionStats>();
    assert_eq!(stats.sight_checks_last_tick, 2);
    assert_eq!(stats.sight_checks_total, 100, "each pair once, no pair twice");
    assert_eq!(stats.sweeps_completed, 1);
    for &agent in &agents {
        let memory = perceived(&app, agent);
        for &target in &targets {
            assert!(memory.get(target, Sense::Sight).is_some(), "{agent} {target}");
        }
    }
    // The next sweep starts over.
    step(&mut app, 1);
    assert_eq!(
        app.world().resource::<PerceptionStats>().sight_checks_last_tick,
        7
    );
}

/// fixrev-05-k (audit-kimirev-fix-ai-determinism-muse, High 2): the sight candidates were sorted
/// by entity id, so under a budget a run that allocated the ids the other way round visited the
/// pairs in the opposite order. The visiting order is the `Perceivable` arrival order.
#[test]
fn the_sight_budget_visits_candidates_in_arrival_order() {
    let mut app = app();
    {
        let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
        settings.sight_interval_ticks = 1;
        settings.sight_checks_per_tick = 1;
    }
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    app.world_mut()
        .get_mut::<Perceived>(agent)
        .expect("perceived")
        .max_age_ticks = 1000;
    // The holders get ids in spawn order; the `Perceivable` arrives in the opposite order, so
    // the earlier arrival has the HIGHER id.
    let low = app.world_mut().spawn_empty().id();
    let high = app.world_mut().spawn_empty().id();
    for (holder, position) in [
        (high, Vec3::new(0.0, 0.0, -5.0)),
        (low, Vec3::new(0.0, 0.0, -10.0)),
    ] {
        app.world_mut().entity_mut(holder).insert((
            Transform::from_translation(position),
            Perceivable {
                team: 1,
                ..Perceivable::default()
            },
        ));
    }
    step(&mut app, 1);
    assert!(
        perceived(&app, agent).knows(high),
        "the earlier arrival is sight-checked first, not the lower id {low}"
    );
    assert!(
        !perceived(&app, agent).knows(low),
        "one check a step: the later arrival waits its turn"
    );
    step(&mut app, 1);
    assert!(
        perceived(&app, agent).knows(low),
        "the round-robin reaches the later arrival on the next step"
    );
}

/// A sweep starts at most every `sight_interval_ticks`.
#[test]
fn the_sight_rate_is_configurable() {
    let mut app = app();
    app.world_mut()
        .resource_mut::<PerceptionSettings>()
        .sight_interval_ticks = 6;
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let target = spawn_target(&mut app, Vec3::new(0.0, 0.0, -5.0), 1);
    let mut checked = Vec::new();
    for tick in 1..=18u64 {
        step(&mut app, 1);
        if app.world().resource::<PerceptionStats>().sight_checks_last_tick > 0 {
            checked.push(tick);
        }
    }
    assert_eq!(checked, [1, 7, 13]);
    assert_eq!(
        perceived(&app, agent)
            .get(target, Sense::Sight)
            .expect("seen")
            .last_sensed_tick,
        13
    );
}

#[test]
fn a_zero_budget_stops_sight_and_keeps_the_rest_running() {
    let mut app = app();
    app.world_mut()
        .resource_mut::<PerceptionSettings>()
        .sight_checks_per_tick = 0;
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let target = spawn_target(&mut app, Vec3::new(0.0, 0.0, -5.0), 1);
    app.world_mut().write_message(DamageEvent {
        victim: agent,
        attacker: target,
        position: Vec3::Z,
        amount: 1.0,
    });
    step(&mut app, 3);
    let memory = perceived(&app, agent);
    assert!(memory.get(target, Sense::Sight).is_none());
    assert!(memory.get(target, Sense::Damage).is_some());
    assert_eq!(app.world().resource::<PerceptionStats>().sight_checks_total, 0);
}

/// Spawning and despawning mid-sweep neither skips a surviving pair nor checks one twice.
#[test]
fn the_round_robin_survives_spawns_and_despawns_mid_sweep() {
    let mut app = app();
    {
        let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
        settings.sight_interval_ticks = 1;
        settings.sight_checks_per_tick = 2;
    }
    let agents: Vec<Entity> = (0..4)
        .map(|i| spawn_agent(&mut app, Vec3::X * i as f32, 0))
        .collect();
    let targets: Vec<Entity> = (0..4)
        .map(|i| spawn_target(&mut app, Vec3::new(i as f32, 0.0, -5.0), 1))
        .collect();
    for &agent in &agents {
        app.world_mut()
            .get_mut::<Perceived>(agent)
            .expect("perceived")
            .max_age_ticks = 1000;
    }
    step(&mut app, 1); // (a0, t0), (a0, t1)
    app.world_mut().despawn(agents[0]);
    app.world_mut().despawn(targets[3]);
    step(&mut app, 7);
    for &agent in &agents[1..] {
        for &target in &targets[..3] {
            assert!(perceived(&app, agent).knows(target), "{agent} {target}");
        }
        assert!(!perceived(&app, agent).knows(targets[3]), "never reached");
    }
}

/// The capacity cap: the stalest stimulus goes first, and `PerceptionLost` says so.
#[test]
fn the_capacity_cap_evicts_the_stalest_stimulus() {
    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    app.world_mut()
        .get_mut::<Perceived>(agent)
        .expect("perceived")
        .capacity = 2;
    let sources: Vec<Entity> = (0..3).map(|_| app.world_mut().spawn_empty().id()).collect();
    for &source in &sources {
        app.world_mut().write_message(HearingEvent {
            position: Vec3::X,
            radius: 10.0,
            team: 1,
            source,
        });
        step(&mut app, 1);
    }
    let memory = perceived(&app, agent);
    assert_eq!(memory.stimuli.len(), 2);
    assert!(!memory.knows(sources[0]), "the stalest went");
    assert!(memory.knows(sources[1]) && memory.knows(sources[2]));
    let events = lost(&app);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].source, sources[0]);
    assert_eq!(events[0].reason, LostReason::Evicted);

    app.world_mut()
        .get_mut::<Perceived>(agent)
        .expect("perceived")
        .capacity = 0;
    let another = app.world_mut().spawn_empty().id();
    app.world_mut().write_message(HearingEvent {
        position: Vec3::X,
        radius: 10.0,
        team: 1,
        source: another,
    });
    step(&mut app, 1);
    assert!(
        !perceived(&app, agent).knows(another),
        "capacity 0 keeps nothing new"
    );
}

/// A despawned source leaves at once instead of waiting out its age; so the target clears.
#[test]
fn a_despawned_source_is_forgotten_at_once() {
    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let target = spawn_target(&mut app, Vec3::new(0.0, 0.0, -5.0), 1);
    step(&mut app, 1);
    assert_eq!(perceived(&app, agent).target, target);
    app.world_mut().despawn(target);
    step(&mut app, 1);
    assert_eq!(perceived(&app, agent).stimuli, Vec::<Stimulus>::new());
    assert_eq!(perceived(&app, agent).target, Entity::PLACEHOLDER);
    let events = lost(&app);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].reason, LostReason::Despawned);
}

/// The blackboard write-through: entity, position and a bool a tree condition can read; cleared with
/// the target; off with an empty key; a renamed key is honored.
#[test]
fn the_current_target_is_written_through_to_the_blackboard() {
    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let at = Vec3::new(0.0, 0.0, -5.0);
    let target = spawn_target(&mut app, at, 1);
    let bb = |app: &App| app.world().get::<Blackboard>(agent).expect("bb").clone();
    step(&mut app, 1);
    let board = bb(&app);
    assert_eq!(board.get_entity("perception_target"), target);
    assert_eq!(board.get_vec3("perception_target_position"), at);
    assert!(board.get_bool("perception_target_known"));

    app.world_mut().despawn(target);
    step(&mut app, 1);
    let board = bb(&app);
    assert!(!board.contains("perception_target"));
    assert!(!board.contains("perception_target_position"));
    assert_eq!(board.get("perception_target_known"), Some(&BbValue::Bool(false)));

    // A renamed key, then an empty one.
    app.world_mut()
        .get_mut::<Perceived>(agent)
        .expect("perceived")
        .target_key = "enemy".to_string();
    let another = spawn_target(&mut app, at, 1);
    step(&mut app, 1);
    assert_eq!(bb(&app).get_entity("enemy"), another);
    app.world_mut().entity_mut(agent).insert(Blackboard::default());
    app.world_mut()
        .get_mut::<Perceived>(agent)
        .expect("perceived")
        .target_key
        .clear();
    step(&mut app, 1);
    assert_eq!(bb(&app), Blackboard::default(), "an empty key writes nothing");
}

/// A quiet agent's blackboard does not read as changed: nothing is written when nothing changed.
#[test]
fn a_quiet_agent_does_not_touch_its_blackboard() {
    #[derive(Resource, Default)]
    struct Changes(u32);
    let mut app = app();
    every_step(&mut app);
    app.init_resource::<Changes>();
    app.add_systems(
        Update,
        |q: Query<(), Changed<Blackboard>>, mut changes: ResMut<Changes>| {
            changes.0 += q.iter().count() as u32;
        },
    );
    spawn_agent(&mut app, Vec3::ZERO, 0);
    step(&mut app, 2);
    let settled = app.world().resource::<Changes>().0;
    step(&mut app, 8);
    assert_eq!(app.world().resource::<Changes>().0, settled);
}

/// A behavior tree and a state tree read the write-through in the same step: `PerceptionStep` runs
/// before `AiStep`.
#[test]
fn trees_read_the_target_in_the_same_step() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AiPlugin, PerceptionPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    every_step(&mut app);
    let mut builder = BehaviorTreeBuilder::new();
    let root = builder.condition("sees", "perception_target_known");
    let def = builder.build(root);
    let tree = BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles");
    let state_def = StateTreeDef::from_ron(
        r#"(
            format_version: 2,
            root: "Root",
            states: [
                (name: "Root", children: ["Idle", "Hunt"]),
                (name: "Idle", transitions: [
                    (trigger: Tick, condition: Key("perception_target_known"), target: State("Hunt")),
                ]),
                (name: "Hunt"),
            ],
        )"#,
    )
    .expect("state tree parses");
    let state_tree =
        StateTree::compile(&state_def, app.world().resource::<BtTaskRegistry>()).expect("compiles");
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    app.world_mut().entity_mut(agent).insert((tree, state_tree));
    step(&mut app, 1);
    let status = |app: &App| {
        app.world()
            .get::<BehaviorTreeStatus>(agent)
            .expect("status")
            .last_status
    };
    assert_eq!(status(&app), BtStatus::Failure, "nothing perceived");

    spawn_target(&mut app, Vec3::new(0.0, 0.0, -5.0), 1);
    step(&mut app, 1);
    assert_eq!(status(&app), BtStatus::Success, "the same step's perception");
    let active = app
        .world()
        .get::<StateTreeStatus>(agent)
        .expect("state status")
        .active_path
        .clone();
    assert_eq!(active.last().map(String::as_str), Some("Hunt"), "{active:?}");
}

/// Hearing and damage messages survive frames with no fixed step (a frame rate far above the step).
#[test]
fn messages_survive_frames_without_a_fixed_step() {
    let mut app = app();
    every_step(&mut app);
    // 240 fps frames on a 15 Hz step: most frames run no step, and a message buffer lasts two frames.
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / 240.0),
    ));
    app.world_mut().resource_mut::<FixedTime>().hz = 15.0;
    let listener = spawn_agent(&mut app, Vec3::ZERO, 0);
    let source = app.world_mut().spawn_empty().id();
    step(&mut app, 1);
    app.world_mut().write_message(HearingEvent {
        position: Vec3::X,
        radius: 10.0,
        team: 1,
        source,
    });
    app.world_mut().write_message(DamageEvent {
        victim: listener,
        attacker: source,
        position: Vec3::X,
        amount: 1.0,
    });
    step(&mut app, 2);
    let memory = perceived(&app, listener);
    assert!(memory.get(source, Sense::Hearing).is_some());
    assert!(memory.get(source, Sense::Damage).is_some());
    assert_eq!(app.world().resource::<PerceptionStats>().messages_dropped, 0);
}

/// `Perceived` arrives by itself on any `Perception` agent.
#[test]
fn a_perception_agent_gets_its_memory_without_asking() {
    let mut app = app();
    every_step(&mut app);
    let agent = app
        .world_mut()
        .spawn((Transform::default(), Perception::default()))
        .id();
    let target = spawn_target(&mut app, Vec3::new(0.0, 0.0, -5.0), 1);
    step(&mut app, 1);
    assert!(perceived(&app, agent).knows(target), "the same step");
}

#[test]
fn a_parked_agent_or_target_takes_no_part() {
    let parked = || LayerParked {
        restore: regia_scene::prelude::Visibility::Inherited,
        inherited: false,
    };
    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    app.world_mut().entity_mut(agent).insert(parked());
    let other = spawn_agent(&mut app, Vec3::ZERO, 0);
    let target = spawn_target(&mut app, Vec3::new(0.0, 0.0, -5.0), 1);
    let hidden = spawn_target(&mut app, Vec3::new(0.0, 0.0, -6.0), 1);
    app.world_mut().entity_mut(hidden).insert(parked());
    step(&mut app, 2);
    assert_eq!(perceived(&app, agent).stimuli, Vec::<Stimulus>::new());
    assert!(perceived(&app, other).knows(target));
    assert!(!perceived(&app, other).knows(hidden));
}

/// The table is directional, and a change applies to the next pick without new sensing.
#[test]
fn the_attitude_table_is_directional_and_a_change_applies_at_once() {
    let mut table = Attitudes::default();
    assert_eq!(table.get(3, 3), Attitude::Friendly);
    assert_eq!(table.get(3, 4), Attitude::Hostile);
    assert_eq!(table.set(3, 4, Attitude::Neutral), Attitude::Hostile);
    assert_eq!(table.get(3, 4), Attitude::Neutral);
    assert_eq!(table.get(4, 3), Attitude::Hostile, "the other way is unchanged");

    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let target = spawn_target(&mut app, Vec3::new(0.0, 0.0, -5.0), 1);
    step(&mut app, 1);
    assert_eq!(perceived(&app, agent).target, target);
    app.world_mut()
        .resource_mut::<Attitudes>()
        .set(0, 1, Attitude::Friendly);
    step(&mut app, 1);
    assert_eq!(perceived(&app, agent).target, Entity::PLACEHOLDER);
    // The memory is not rewritten; it ages out (no new sight, the filter refuses a friend).
    assert!(perceived(&app, agent).knows(target));
}

/// The target goes to the strongest; a similar challenger does not steal it every step.
#[test]
fn the_target_is_the_strongest_and_sticks_against_a_near_tie() {
    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let far = spawn_target(&mut app, Vec3::new(0.0, 0.0, -10.0), 1);
    step(&mut app, 1);
    assert_eq!(perceived(&app, agent).target, far);
    // A little closer: not enough to take over.
    let near_tie = spawn_target(&mut app, Vec3::new(0.0, 0.0, -9.5), 1);
    step(&mut app, 1);
    assert!(perceived(&app, agent).knows(near_tie));
    assert_eq!(perceived(&app, agent).target, far, "commitment");
    // Far closer: takes over.
    let close = spawn_target(&mut app, Vec3::new(0.0, 0.0, -1.0), 1);
    step(&mut app, 1);
    assert_eq!(perceived(&app, agent).target, close);
}

/// Acceptance: two runs give the same perceived lists, step by step.
#[test]
fn two_runs_give_the_same_perceived_lists() {
    fn run() -> Vec<Vec<Perceived>> {
        let mut app = app();
        {
            let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
            settings.sight_interval_ticks = 2;
            settings.sight_checks_per_tick = 5;
        }
        // Spawned out of order across archetypes, so archetype order differs from entity order.
        let mut agents = Vec::new();
        let mut targets = Vec::new();
        for i in 0..8 {
            let agent = spawn_agent(&mut app, Vec3::new(i as f32, 0.0, 0.0), (i % 2) as u8);
            app.world_mut()
                .get_mut::<Perceived>(agent)
                .expect("perceived")
                .max_age_ticks = 40;
            if i % 3 == 0 {
                app.world_mut().entity_mut(agent).insert(Perceivable {
                    team: (i % 2) as u8,
                    ..Perceivable::default()
                });
            }
            agents.push(agent);
            targets.push(spawn_target(&mut app, Vec3::new(i as f32, 0.0, -6.0), 2));
        }
        let mut trace = Vec::new();
        for tick in 0..120u32 {
            let target = targets[tick as usize % targets.len()];
            let angle = tick as f32 * 0.1;
            set_position(
                &mut app,
                target,
                Vec3::new(angle.cos() * 12.0, 0.0, angle.sin() * 12.0),
            );
            if tick % 7 == 0 {
                app.world_mut().write_message(HearingEvent {
                    position: Vec3::new(1.0, 0.0, tick as f32 * 0.05),
                    radius: 10.0,
                    team: 2,
                    source: targets[(tick as usize + 3) % targets.len()],
                });
            }
            if tick % 11 == 0 {
                app.world_mut().write_message(DamageEvent {
                    victim: agents[tick as usize % agents.len()],
                    attacker: targets[(tick as usize + 1) % targets.len()],
                    position: Vec3::new(0.0, 0.0, 3.0),
                    amount: 5.0,
                });
            }
            step(&mut app, 1);
            trace.push(agents.iter().map(|&a| perceived(&app, a).clone()).collect());
        }
        trace
    }
    let (a, b) = (run(), run());
    assert_eq!(a, b);
    let richest = a.last().expect("steps").iter().map(|p| p.stimuli.len()).max();
    assert!(richest > Some(2), "the run perceived something: {richest:?}");
}

/// `Perceived` and its neighbors go through the reflection path `scene.get_entity` and scene files use.
#[test]
fn perceived_survives_the_reflection_path() {
    use regia_ecs::reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
    use regia_ecs::reflect::{AppTypeRegistry, FromReflect};
    use serde::de::DeserializeSeed;

    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    spawn_target(&mut app, Vec3::new(0.0, 0.0, -5.0), 1);
    step(&mut app, 1);
    let memory = perceived(&app, agent).clone();
    assert_eq!(memory.stimuli.len(), 1);
    let registry = app.world().resource::<AppTypeRegistry>().read();
    let text = ron::to_string(&TypedReflectSerializer::new(&memory, &registry)).expect("serialize");
    let registration = registry
        .get(std::any::TypeId::of::<Perceived>())
        .expect("registered");
    let mut deserializer = ron::Deserializer::from_str(&text).expect("text");
    let dynamic = TypedReflectDeserializer::new(registration, &registry)
        .deserialize(&mut deserializer)
        .expect("deserialize");
    assert_eq!(
        Perceived::from_reflect(dynamic.as_ref()).expect("from reflect"),
        memory
    );
}

/// A child's pose comes from the local transforms up its chain, this step's and not the last frame's
/// propagation. Nothing propagates here, so a read of `GlobalTransform` would see the origin.
#[test]
fn children_are_placed_by_their_chain_without_waiting_for_propagation() {
    let mut app = app();
    every_step(&mut app);
    // An agent on a mount at x = 100, facing -Z, and a target on a mount in front of it.
    let agent_mount = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)))
        .id();
    let agent = spawn_agent(&mut app, Vec3::new(1.0, 0.0, 0.0), 0);
    app.world_mut().entity_mut(agent).insert(ChildOf(agent_mount));
    let target_mount = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::new(101.0, 0.0, -5.0)))
        .id();
    let target = spawn_target(&mut app, Vec3::new(0.0, 0.0, -1.0), 1);
    app.world_mut().entity_mut(target).insert(ChildOf(target_mount));
    step(&mut app, 1);
    let seen = perceived(&app, agent)
        .get(target, Sense::Sight)
        .expect("in the cone");
    assert_eq!(seen.last_known_position, Vec3::new(101.0, 0.0, -6.0));

    // The mount moves between steps: the next step sees it.
    set_position(&mut app, target_mount, Vec3::new(101.0, 0.0, -9.0));
    step(&mut app, 1);
    let seen = perceived(&app, agent)
        .get(target, Sense::Sight)
        .expect("in the cone");
    assert_eq!(seen.last_known_position, Vec3::new(101.0, 0.0, -10.0));

    // The mount turning turns the agent's cone: now it faces +Z, away from the target.
    app.world_mut()
        .get_mut::<Transform>(agent_mount)
        .expect("transform")
        .rotation = Quat::from_rotation_y(std::f32::consts::PI);
    let stale = app.world().resource::<PerceptionStats>().tick;
    step(&mut app, 2);
    let seen = perceived(&app, agent)
        .get(target, Sense::Sight)
        .expect("remembered");
    assert_eq!(seen.last_sensed_tick, stale, "not seen again");
}

/// Lowering the capacity holds from the next step, not only when something new arrives.
#[test]
fn lowering_the_capacity_trims_at_once() {
    let mut app = app();
    every_step(&mut app);
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let targets: Vec<Entity> = (0..3)
        .map(|i| spawn_target(&mut app, Vec3::new(i as f32, 0.0, -5.0), 1))
        .collect();
    step(&mut app, 1);
    assert_eq!(perceived(&app, agent).stimuli.len(), 3);
    app.world_mut()
        .get_mut::<Perceived>(agent)
        .expect("perceived")
        .capacity = 1;
    step(&mut app, 1);
    assert_eq!(perceived(&app, agent).stimuli.len(), 1);
    let events = lost(&app);
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(events.iter().all(|e| e.reason == LostReason::Evicted));
    app.world_mut()
        .get_mut::<Perceived>(agent)
        .expect("perceived")
        .capacity = 0;
    step(&mut app, 1);
    assert_eq!(perceived(&app, agent).stimuli, Vec::<Stimulus>::new());
    assert_eq!(perceived(&app, agent).target, Entity::PLACEHOLDER);
    assert!(targets.iter().all(|&t| !perceived(&app, agent).knows(t)));
}

/// The sight sweep resumes by observer identity. The observer list is spawn (arrival) order, not
/// entity-id order, so a later perceiver with a lower entity index is still visited after the
/// sweep stops on an earlier arrival.
#[test]
fn sight_sweep_continues_in_spawn_order_when_entity_ids_are_reversed() {
    let mut app = app();
    every_step(&mut app);
    {
        let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
        settings.sight_checks_per_tick = 2;
    }
    // Holders get ids in spawn order; the agent bundles arrive in the opposite order, so the
    // visiting (arrival) order is the reverse of the entity-id order.
    let holders: Vec<Entity> = (0..3).map(|_| app.world_mut().spawn_empty().id()).collect();
    for &holder in &[holders[2], holders[0], holders[1]] {
        app.world_mut().entity_mut(holder).insert((
            Transform::from_translation(Vec3::ZERO),
            Perception {
                sight_radius: 20.0,
                sight_half_angle: 90f32.to_radians(),
                hearing_radius: 0.0,
                team: 0,
                // Zero: the lose cone matches the acquire cone (perception-hysteresis, ADR-4586).
                sight_lose_radius: 0.0,
                sight_lose_half_angle: 0.0,
                ..Perception::default()
            },
            Perceived {
                max_age_ticks: 100,
                ..Perceived::default()
            },
            Blackboard::default(),
        ));
    }
    let t1 = spawn_target(&mut app, Vec3::new(-1.0, 0.0, -1.0), 1);
    let t2 = spawn_target(&mut app, Vec3::new(1.0, 0.0, -1.0), 1);
    // Tick 1: the budget covers exactly the first arrival's two pairs.
    step(&mut app, 1);
    assert!(
        perceived(&app, holders[2]).knows(t1),
        "the first arrival in spawn order was sight-checked on tick 1"
    );
    // The observer the sweep stopped on leaves the world; the remaining observers must still be
    // visited on the following ticks, in their arrival order.
    app.world_mut().despawn(holders[2]);
    step(&mut app, 1);
    assert!(
        perceived(&app, holders[0]).knows(t1),
        "the second arrival must be sight-checked on tick 2, not skipped by a mis-resumed sweep"
    );
    step(&mut app, 1);
    assert!(
        perceived(&app, holders[1]).knows(t2),
        "the third arrival must be sight-checked on tick 3"
    );
}

/// A sight budget smaller than the pair count must spend its one check on the earlier
/// [`Perceivable`] arrival. The old candidate sort was `(index, to_bits)`, which is not arrival
/// order and is not stable across runs. `Entity::to_bits` is not monotonic in spawn order.
#[test]
fn audfix_sight_budget_visits_the_earlier_perceivable_not_the_lower_id() {
    let mut app = app();
    every_step(&mut app);
    {
        let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
        settings.sight_checks_per_tick = 1;
        settings.sight_interval_ticks = 1;
    }
    let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
    let first = app.world_mut().spawn_empty().id();
    let second = app.world_mut().spawn_empty().id();
    let entity_key = |entity: Entity| (entity.index_u32(), entity.to_bits());
    // The holder with the greater entity key receives `Perceivable` first, so arrival order
    // opposes the old `(index, to_bits)` sort.
    let (earlier, later) = if entity_key(first) > entity_key(second) {
        (first, second)
    } else {
        (second, first)
    };
    assert!(
        entity_key(earlier) > entity_key(later),
        "the two holders share an entity key, so the sort cannot disagree with arrival"
    );
    app.world_mut().entity_mut(earlier).insert((
        Transform::from_translation(Vec3::new(0.2, 0.0, -5.0)),
        Perceivable {
            team: 1,
            ..Perceivable::default()
        },
    ));
    app.world_mut().entity_mut(later).insert((
        Transform::from_translation(Vec3::new(-0.2, 0.0, -5.0)),
        Perceivable {
            team: 1,
            ..Perceivable::default()
        },
    ));
    step(&mut app, 1);
    assert!(
        perceived(&app, agent).knows(earlier),
        "the one budgeted check must be the earlier arrival {earlier:?} key {:?}, not {later:?} key {:?}",
        entity_key(earlier),
        entity_key(later),
    );
    assert!(
        !perceived(&app, agent).knows(later),
        "the later arrival waits for the next budget"
    );
}

/// Finding 5 (audit G): with scores exactly equal, the target was the stimulus stored first — the
/// lower entity id — which is not stable across runs (ids come from record-time allocation). The
/// tie must go to the source whose `Perceivable` arrived first.
#[test]
fn equal_nearness_keeps_the_earlier_arrival() {
    fn run_once() -> (Entity, Entity, Entity) {
        let mut app = app();
        // No real sensing: the authored stimuli below are the only input.
        app.world_mut()
            .resource_mut::<PerceptionSettings>()
            .sight_checks_per_tick = 0;
        let agent = spawn_agent(&mut app, Vec3::ZERO, 0);
        // Two sources at the same distance from the eye; the lower-id one receives its
        // `Perceivable` second, so the earlier arrival has the higher id.
        let sx = app.world_mut().spawn_empty().id();
        let sy = app.world_mut().spawn_empty().id();
        for &(source, position) in &[(sy, Vec3::new(0.0, 0.0, -5.0)), (sx, Vec3::new(3.0, 0.0, 4.0))] {
            app.world_mut().entity_mut(source).insert((
                Transform::from_translation(position),
                Perceivable {
                    team: 1,
                    ..Perceivable::default()
                },
            ));
        }
        let stimulus = |entity, last_known_position| Stimulus {
            entity,
            sense: Sense::Sight,
            team: Some(1),
            last_known_position,
            last_sensed_tick: 1,
            strength: 0.5,
            ..Stimulus::default()
        };
        app.world_mut()
            .get_mut::<Perceived>(agent)
            .expect("perceived")
            .stimuli = vec![
            stimulus(sx, Vec3::new(3.0, 0.0, 4.0)),
            stimulus(sy, Vec3::new(0.0, 0.0, -5.0)),
        ];
        step(&mut app, 1);
        (
            app.world().get::<Perceived>(agent).expect("perceived").target,
            sx,
            sy,
        )
    }
    let (target, sx, sy) = run_once();
    assert_eq!(
        target, sy,
        "the earlier arrival wins the tie, not the lower id {sx}"
    );
    let (swapped, _, sy2) = run_once();
    assert_eq!(swapped, sy2);
    assert_eq!(target, swapped, "the same arrival wins in every run");
}

/// Sight reads a world pose only for the pairs this step's budget actually tests.
#[test]
fn sight_poses_only_the_pairs_this_step_tests() {
    let mut app = app();
    {
        let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
        settings.sight_interval_ticks = 10_000;
        settings.sight_checks_per_tick = 8;
    }
    for i in 0..80 {
        spawn_agent(&mut app, Vec3::new(i as f32, 0.0, 0.0), 0);
        spawn_target(&mut app, Vec3::new(i as f32, 0.0, -5.0), 1);
    }
    step(&mut app, 1);
    let stats = app.world().resource::<PerceptionStats>();
    assert_eq!(stats.sight_checks_last_tick, 8, "the budget is spent");
    assert!(
        stats.sight_pose_reads_last_tick <= 8 * 2,
        "posed {} worlds for 8 checks",
        stats.sight_pose_reads_last_tick
    );
}

/// Hearing asks `can_hear` of the agents in the cells the sound reaches, not of every agent.
#[test]
fn hearing_tests_only_agents_near_the_sound() {
    let mut app = app();
    {
        let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
        settings.sight_checks_per_tick = 0;
    }
    let mut far = Entity::PLACEHOLDER;
    for x in -5..=5 {
        for z in -5..=5 {
            let entity = spawn_agent(&mut app, Vec3::new(x as f32 * 4.0, 0.0, z as f32 * 4.0), 0);
            if x == 5 && z == 5 {
                far = entity;
            }
        }
    }
    app.world_mut().write_message(HearingEvent {
        position: Vec3::ZERO,
        radius: 6.0,
        team: 1,
        source: Entity::PLACEHOLDER,
    });
    step(&mut app, 1);
    let tested = app.world().resource::<PerceptionStats>().hearing_tests_last_tick;
    assert!(tested > 0, "a nearby agent is tested");
    assert!(tested < 64, "tested {tested} agents for one sound of radius 6");
    assert!(!perceived(&app, far).knows(Entity::PLACEHOLDER));
}

/// The hearing grid watches `GlobalTransform` too. An agent posed only by its `GlobalTransform`
/// (no `Transform` anywhere; `world_pose` falls back to it) moves without any `Transform` change,
/// so a grid keyed on `Changed<Transform>` alone hears the next sound at the old position.
#[test]
fn a_global_only_move_on_a_quiet_step_reaches_the_hearing_grid() {
    use regia_core::math::Affine3A;
    use regia_scene::GlobalTransform;

    let mut app = app();
    every_step(&mut app);
    let listener = app
        .world_mut()
        .spawn((
            GlobalTransform(Affine3A::from_translation(Vec3::ZERO)),
            Perception {
                hearing_radius: 15.0,
                team: 0,
                ..Perception::default()
            },
            Perceived {
                max_age_ticks: 10,
                ..Perceived::default()
            },
            Blackboard::default(),
        ))
        .id();
    let first = app.world_mut().spawn_empty().id();
    let near_new = app.world_mut().spawn_empty().id();
    let near_old = app.world_mut().spawn_empty().id();
    let noise = |position: Vec3, source: Entity| HearingEvent {
        position,
        radius: 2.0,
        team: 1,
        source,
    };
    // Step 1: a sound builds the grid with the listener at the origin.
    app.world_mut().write_message(noise(Vec3::ZERO, first));
    step(&mut app, 1);
    assert!(perceived(&app, listener).knows(first), "the first sound is heard");
    // Step 2: only the global pose moves; no `Transform` exists to change.
    app.world_mut()
        .get_mut::<GlobalTransform>(listener)
        .expect("global transform")
        .0 = Affine3A::from_translation(Vec3::new(100.0, 0.0, 0.0));
    step(&mut app, 1);
    // Step 3: nothing moves; one sound at the new spot, one at the old.
    app.world_mut()
        .write_message(noise(Vec3::new(100.0, 0.0, 0.0), near_new));
    app.world_mut().write_message(noise(Vec3::ZERO, near_old));
    step(&mut app, 1);
    assert!(
        perceived(&app, listener).knows(near_new),
        "a sound at the listener's feet must be heard"
    );
    assert!(
        !perceived(&app, listener).knows(near_old),
        "a sound 100 m away, at the old position, must not be heard"
    );
}

/// The hearing grid is reused while nothing moves. A move on a step with no sound must still drop
/// it: the next sound is heard where the listener is now, not where it stood at the last sound.
#[test]
fn a_move_on_a_quiet_step_reaches_the_hearing_grid() {
    let mut app = app();
    every_step(&mut app);
    let listener = spawn_agent(&mut app, Vec3::ZERO, 0);
    let first = app.world_mut().spawn_empty().id();
    let near_new = app.world_mut().spawn_empty().id();
    let near_old = app.world_mut().spawn_empty().id();
    let noise = |position: Vec3, source: Entity| HearingEvent {
        position,
        radius: 2.0,
        team: 1,
        source,
    };
    // Step 1: a sound builds the grid with the listener at the origin.
    app.world_mut().write_message(noise(Vec3::ZERO, first));
    step(&mut app, 1);
    assert!(perceived(&app, listener).knows(first), "the first sound is heard");
    // Step 2: the listener moves 100 m; nothing makes a sound.
    app.world_mut()
        .get_mut::<Transform>(listener)
        .expect("transform")
        .translation = Vec3::new(100.0, 0.0, 0.0);
    step(&mut app, 1);
    // Step 3: nothing moves; one sound at the new spot, one at the old.
    app.world_mut()
        .write_message(noise(Vec3::new(100.0, 0.0, 0.0), near_new));
    app.world_mut().write_message(noise(Vec3::ZERO, near_old));
    step(&mut app, 1);
    assert!(
        perceived(&app, listener).knows(near_new),
        "a sound at the listener's feet must be heard"
    );
    assert!(
        !perceived(&app, listener).knows(near_old),
        "a sound 100 m away, at the old position, must not be heard"
    );
}
