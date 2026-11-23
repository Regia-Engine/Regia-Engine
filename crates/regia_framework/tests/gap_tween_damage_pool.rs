// SPDX-License-Identifier: MPL-2.0
//! Test-gap closure for `tween`, `damage` and `pool`: degenerate durations, path forms, clamping
//! of health, and pool bookkeeping across keys. Fixed time steps only.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Easing;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::damage::{DamageEvent, DamagePlugin, Died, DiedEvent, Health, damage};
use regia_framework::pool::{Pooled, PooledInactive, PrefabPool};
use regia_framework::script::ScriptPlugin;
use regia_framework::tween::{Tween, TweenPlugin};
use regia_scene::Transform;

fn tween_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, TweenPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    app.world_mut().resource_mut::<FixedTime>().hz = 10.0;
    app
}

#[test]
fn default_tween_targets_translation_x_over_one_second() {
    let t = Tween::default();
    assert_eq!(t.component, "Transform");
    assert_eq!(t.path, ".translation.x");
    assert_eq!((t.from, t.to), (0.0, 1.0));
    assert_eq!(t.duration_secs, 1.0);
    assert_eq!(t.elapsed_secs, 0.0);
}

#[test]
fn translation_axis_builds_the_axis_path() {
    let t = Tween::translation_axis('z', 1.0, 2.0, 0.5, Easing::Linear);
    assert_eq!(t.component, "Transform");
    assert_eq!(t.path, ".translation.z");
    assert_eq!(t.duration_secs, 0.5);
}

#[test]
fn negative_duration_is_clamped_and_progress_is_complete() {
    let t = Tween::field("Transform", ".translation.x", 0.0, 1.0, -3.0, Easing::Linear);
    assert_eq!(t.duration_secs, 0.0);
    assert_eq!(t.t(0.0), 1.0, "a zero-length tween is done at once");
    assert_eq!(t.t(-1.0), 1.0);
}

#[test]
fn progress_clamps_below_zero() {
    let t = Tween::translation_axis('x', 0.0, 1.0, 2.0, Easing::Linear);
    assert_eq!(t.t(-5.0), 0.0);
    assert_eq!(t.t(1.0), 0.5);
}

#[test]
fn a_decreasing_tween_interpolates_downward() {
    let t = Tween::translation_axis('y', 10.0, 2.0, 1.0, Easing::Linear);
    assert!((t.value_at(0.0) - 10.0).abs() < 1e-6);
    assert!((t.value_at(0.5) - 6.0).abs() < 1e-6);
    assert!((t.value_at(1.0) - 2.0).abs() < 1e-6);
}

#[test]
fn zero_duration_tween_snaps_to_the_end_value_and_removes_itself() {
    let mut app = tween_app();
    let entity = app
        .world_mut()
        .spawn((
            Transform::default(),
            Tween::translation_axis('x', 3.0, 7.0, 0.0, Easing::Linear),
        ))
        .id();
    app.update();
    let x = app.world().get::<Transform>(entity).map(|t| t.translation.x);
    assert_eq!(x, Some(7.0));
    assert!(app.world().get::<Tween>(entity).is_none());
}

#[test]
fn a_running_tween_stays_attached_and_accumulates_elapsed_time() {
    let mut app = tween_app();
    let entity = app
        .world_mut()
        .spawn((
            Transform::default(),
            Tween::translation_axis('x', 0.0, 10.0, 1.0, Easing::Linear),
        ))
        .id();
    for _ in 0..3 {
        app.update();
    }
    let tween = app.world().get::<Tween>(entity).cloned();
    let Some(tween) = tween else {
        panic!("the tween should still be running after 0.3 s of 1.0 s");
    };
    assert!(
        (tween.elapsed_secs - 0.3).abs() < 1e-4,
        "elapsed = {}",
        tween.elapsed_secs
    );
    let x = app
        .world()
        .get::<Transform>(entity)
        .map_or(f32::NAN, |t| t.translation.x);
    assert!((x - 3.0).abs() < 1e-3, "x = {x}");
}

#[test]
fn a_step_longer_than_the_duration_lands_exactly_on_the_end_value() {
    let mut app = tween_app();
    let entity = app
        .world_mut()
        .spawn((
            Transform::default(),
            Tween::translation_axis('y', 0.0, 5.0, 0.05, Easing::Linear),
        ))
        .id();
    app.update(); // 0.1 s step against a 0.05 s tween
    let y = app.world().get::<Transform>(entity).map(|t| t.translation.y);
    assert_eq!(y, Some(5.0), "no overshoot");
    assert!(app.world().get::<Tween>(entity).is_none());
}

#[test]
fn the_path_works_with_and_without_the_leading_dot() {
    let mut app = tween_app();
    let dotted = app
        .world_mut()
        .spawn((
            Transform::default(),
            Tween::field("Transform", ".translation.y", 0.0, 2.0, 0.0, Easing::Linear),
        ))
        .id();
    let bare = app
        .world_mut()
        .spawn((
            Transform::default(),
            Tween::field("Transform", "translation.y", 0.0, 2.0, 0.0, Easing::Linear),
        ))
        .id();
    app.update();
    for entity in [dotted, bare] {
        let y = app.world().get::<Transform>(entity).map(|t| t.translation.y);
        assert_eq!(y, Some(2.0));
    }
}

#[test]
fn unknown_field_path_drops_the_tween_and_leaves_the_component() {
    let mut app = tween_app();
    let entity = app
        .world_mut()
        .spawn((
            Transform::default(),
            Tween::field("Transform", ".translation.w", 0.0, 1.0, 1.0, Easing::Linear),
        ))
        .id();
    app.update();
    assert!(app.world().get::<Tween>(entity).is_none());
    assert_eq!(
        app.world().get::<Transform>(entity).map(|t| t.translation.x),
        Some(0.0)
    );
}

#[test]
fn tween_on_an_entity_missing_the_target_component_is_dropped() {
    let mut app = tween_app();
    let entity = app
        .world_mut()
        .spawn(Tween::translation_axis('x', 0.0, 1.0, 1.0, Easing::Linear))
        .id();
    app.update();
    assert!(app.world().get::<Tween>(entity).is_none());
}

#[test]
fn independent_tweens_on_different_entities_progress_independently() {
    let mut app = tween_app();
    let fast = app
        .world_mut()
        .spawn((
            Transform::default(),
            Tween::translation_axis('x', 0.0, 1.0, 0.15, Easing::Linear),
        ))
        .id();
    let slow = app
        .world_mut()
        .spawn((
            Transform::default(),
            Tween::translation_axis('x', 0.0, 1.0, 10.0, Easing::Linear),
        ))
        .id();
    app.update();
    app.update();
    assert!(
        app.world().get::<Tween>(fast).is_none(),
        "the short tween finished"
    );
    assert!(
        app.world().get::<Tween>(slow).is_some(),
        "the long one is still running"
    );
}

// ---------------------------------------------------------------------------
// Damage
// ---------------------------------------------------------------------------

fn damage_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, DamagePlugin, ScriptPlugin));
    enable_fixed_step(&mut app, 60.0);
    app
}

fn deaths(app: &App) -> Vec<DiedEvent> {
    let mut cursor = MessageCursor::default();
    cursor
        .read(app.world().resource::<Messages<DiedEvent>>())
        .cloned()
        .collect()
}

fn hit(app: &mut App, target: Entity, amount: f32) {
    app.world_mut().write_message(DamageEvent {
        target,
        amount,
        source: None,
        position: None,
    });
}

#[test]
fn health_fraction_is_clamped_and_safe_for_a_zero_max() {
    assert_eq!(
        Health {
            current: 150.0,
            max: 100.0
        }
        .fraction(),
        1.0,
        "over-full clamps to 1"
    );
    assert_eq!(
        Health {
            current: -10.0,
            max: 100.0
        }
        .fraction(),
        0.0,
        "negative clamps to 0"
    );
    assert_eq!(
        Health {
            current: 5.0,
            max: 0.0
        }
        .fraction(),
        0.0,
        "zero max never divides"
    );
    assert_eq!(
        Health {
            current: 5.0,
            max: -3.0
        }
        .fraction(),
        0.0,
        "negative max never divides"
    );
    assert_eq!(Health::new(40.0).fraction(), 1.0);
    assert!(
        (Health {
            current: 25.0,
            max: 100.0
        }
        .fraction()
            - 0.25)
            .abs()
            < 1e-6
    );
}

#[test]
fn health_is_dead_at_or_below_zero() {
    assert!(
        Health {
            current: 0.0,
            max: 10.0
        }
        .is_dead()
    );
    assert!(
        Health {
            current: -1.0,
            max: 10.0
        }
        .is_dead()
    );
    assert!(
        !Health {
            current: 0.001,
            max: 10.0
        }
        .is_dead()
    );
}

#[test]
fn negative_damage_heals_and_stops_at_max() {
    let mut app = damage_app();
    let target = app
        .world_mut()
        .spawn(Health {
            current: 10.0,
            max: 100.0,
        })
        .id();
    hit(&mut app, target, -50.0);
    app.update();
    assert_eq!(app.world().get::<Health>(target).map(|h| h.current), Some(60.0));
    hit(&mut app, target, -1000.0);
    app.update();
    assert_eq!(
        app.world().get::<Health>(target).map(|h| h.current),
        Some(100.0),
        "heal clamps at max"
    );
    assert!(deaths(&app).is_empty());
}

#[test]
fn several_events_in_one_step_accumulate() {
    let mut app = damage_app();
    let target = app.world_mut().spawn(Health::new(100.0)).id();
    hit(&mut app, target, 10.0);
    hit(&mut app, target, 20.0);
    hit(&mut app, target, 30.0);
    app.update();
    assert_eq!(app.world().get::<Health>(target).map(|h| h.current), Some(40.0));
}

#[test]
fn two_killing_blows_in_one_step_raise_one_death_attributed_to_the_first() {
    let mut app = damage_app();
    let first = app.world_mut().spawn(()).id();
    let second = app.world_mut().spawn(()).id();
    let target = app.world_mut().spawn(Health::new(10.0)).id();
    app.world_mut().write_message(DamageEvent {
        target,
        amount: 50.0,
        source: Some(first),
        position: None,
    });
    app.world_mut().write_message(DamageEvent {
        target,
        amount: 50.0,
        source: Some(second),
        position: None,
    });
    app.update();
    let all = deaths(&app);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].killer, Some(first));
}

#[test]
fn zero_damage_changes_nothing_and_does_not_kill() {
    let mut app = damage_app();
    let target = app.world_mut().spawn(Health::new(10.0)).id();
    hit(&mut app, target, 0.0);
    app.update();
    assert_eq!(app.world().get::<Health>(target).map(|h| h.current), Some(10.0));
    assert!(app.world().get::<Died>(target).is_none());
}

#[test]
fn damage_to_a_despawned_target_is_ignored() {
    let mut app = damage_app();
    let target = app.world_mut().spawn(Health::new(10.0)).id();
    app.world_mut().despawn(target);
    hit(&mut app, target, 5.0);
    app.update(); // must not panic
    assert!(deaths(&app).is_empty());
}

#[test]
fn killing_blow_without_a_source_has_no_killer() {
    let mut app = damage_app();
    let target = app.world_mut().spawn(Health::new(5.0)).id();
    damage(app.world_mut(), target, 5.0, None, None);
    app.update();
    let all = deaths(&app);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].killer, None);
    assert_eq!(all[0].entity, target);
}

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

fn bare_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app
}

#[test]
fn unknown_keys_report_empty_counts() {
    let pool = PrefabPool::default();
    assert_eq!(pool.free_len("nothing"), 0);
    assert_eq!(pool.live_len("nothing"), 0);
}

#[test]
fn acquired_entities_record_their_pool_key() {
    let mut app = bare_app();
    let mut pool = PrefabPool::default();
    let entity = pool.acquire(app.world_mut(), "rocket", |world| world.spawn(()).id());
    assert_eq!(
        app.world().get::<Pooled>(entity),
        Some(&Pooled { key: "rocket".into() })
    );
}

#[test]
fn free_entities_are_reused_last_in_first_out() {
    let mut app = bare_app();
    let mut pool = PrefabPool::default();
    let a = pool.acquire(app.world_mut(), "b", |world| world.spawn(()).id());
    let b = pool.acquire(app.world_mut(), "b", |world| world.spawn(()).id());
    assert_ne!(a, b);
    pool.recycle(app.world_mut(), "b", a);
    pool.recycle(app.world_mut(), "b", b);
    assert_eq!(pool.free_len("b"), 2);
    let next = pool.acquire(app.world_mut(), "b", |_| {
        unreachable!("the pool has free entities")
    });
    assert_eq!(next, b, "the most recently recycled entity comes back first");
    assert_eq!(pool.free_len("b"), 1);
    assert_eq!(pool.live_len("b"), 1);
}

#[test]
fn the_factory_runs_once_per_missing_entity() {
    let mut app = bare_app();
    let mut pool = PrefabPool::default();
    let mut calls = 0;
    for _ in 0..3 {
        pool.acquire(app.world_mut(), "b", |world| {
            calls += 1;
            world.spawn(()).id()
        });
    }
    assert_eq!(calls, 3);
    assert_eq!(pool.live_len("b"), 3);
}

#[test]
fn recycling_tags_inactive_and_acquiring_clears_the_tag_but_keeps_pooled() {
    let mut app = bare_app();
    let mut pool = PrefabPool::default();
    let e = pool.acquire(app.world_mut(), "b", |world| world.spawn(()).id());
    pool.recycle(app.world_mut(), "b", e);
    assert!(app.world().get::<PooledInactive>(e).is_some());
    assert!(app.world().get::<Pooled>(e).is_some());
    let again = pool.acquire(app.world_mut(), "b", |_| unreachable!("reuse expected"));
    assert_eq!(again, e);
    assert!(app.world().get::<PooledInactive>(e).is_none());
    assert!(app.world().get::<Pooled>(e).is_some());
}

#[test]
fn live_count_never_underflows() {
    let mut app = bare_app();
    let mut pool = PrefabPool::default();
    let stray = app.world_mut().spawn(()).id();
    pool.recycle(app.world_mut(), "b", stray); // never acquired
    assert_eq!(pool.live_len("b"), 0, "saturates at zero");
}
