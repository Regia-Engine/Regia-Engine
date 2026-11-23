// SPDX-License-Identifier: MPL-2.0
//! Timers (`docs/specs/14-scene-and-framework.md` § B6): `after(secs, callback)`, repeating
//! timers, message timers — all owner-bound (cancelled when the owner entity is despawned).
//!
//! Timers live in the [`TimerHub`] resource and tick in [`FixedUpdate`] **before** the fixed script
//! pass, so a timer a script schedules fires on a later fixed step and its message is delivered in
//! the same application frame it fires. Callbacks receive `&mut World`.
//!
//! # Hot reload
//!
//! Callbacks are closures in an engine resource — they do not survive a game-library reload
//! (`specs/20` § 4). Use [`TimerAction::Message`] (or [`schedule_message_after`]) for timers that
//! must survive: a message timer only carries a reflected payload and a type path, and the
//! receiving script (rebuilt by the restore) reacts to it after the reload.

use regia_app::prelude::*;
use regia_ecs::reflect::Reflect;

use crate::script::{MessageEnvelope, MessageTarget, ScriptMessages, TypedMessage};

/// What a timer does when it fires.
pub enum TimerAction {
    /// Runs a callback with full world access. Does not survive a hot reload (see the module docs).
    Call(Box<dyn FnMut(&mut World) + Send + Sync>),
    /// Delivers a reflected message to a script entity. Survives a hot reload.
    Message {
        /// The receiving entity.
        target: Entity,
        /// The queued envelope.
        envelope: Box<dyn MessageEnvelope>,
    },
}

/// One scheduled timer.
struct TimerEntry {
    /// Seconds until the timer fires (or until the next repeat).
    remaining: f32,
    /// `Some(interval)` for repeating timers.
    interval: Option<f32>,
    /// The timer is cancelled when this entity no longer exists.
    owner: Option<Entity>,
    /// What happens on fire.
    action: TimerAction,
}

/// All live timers. Add them through the free functions ([`schedule_after`],
/// [`schedule_every`], [`schedule_message_after`]) or `ScriptCtx::after` and friends
/// (`regia_framework::script::ScriptCtx`); those insert the resource when absent so a missing
/// [`TimerPlugin`] can never panic user code.
///
/// # Capacity
///
/// The hub is deliberately unbounded: no spec names a live-timer budget, one entry costs one
/// scan slot per fixed step plus its boxed action, and owner binding (a despawned owner cancels
/// its timers) is the intended cleanup path — game code owns the lifetime of what it schedules.
/// A repeating callback that schedules further timers is a leak in that game's code, not in the
/// hub; `timers.list` reports every live timer (with its owner), so a leak is observable from
/// the agent protocol (deep-zai finding 5: documented intent, no cap added).
///
/// # Example
///
/// ```
/// use regia_app::prelude::Entity;
/// use regia_framework::timers::TimerHub;
///
/// let mut hub = TimerHub::default();
/// let owner = Entity::PLACEHOLDER;
/// hub.after(1.0, Some(owner), |_world| {});
/// hub.every(0.5, None, |_world| {});
/// assert_eq!(hub.len(), 2);
///
/// hub.cancel_all(owner);
/// assert_eq!(hub.len(), 1, "the owner-bound timer is cancelled");
/// ```
#[derive(Resource, Default)]
pub struct TimerHub {
    entries: Vec<TimerEntry>,
}

impl TimerHub {
    /// Schedules `callback` in `secs` game-seconds. Cancels itself when `owner` is despawned.
    pub fn after(
        &mut self,
        secs: f32,
        owner: Option<Entity>,
        callback: impl FnMut(&mut World) + Send + Sync + 'static,
    ) {
        self.entries.push(TimerEntry {
            remaining: secs.max(0.0),
            interval: None,
            owner,
            action: TimerAction::Call(Box::new(callback)),
        });
    }

    /// Schedules `callback` every `secs` game-seconds until its owner is despawned.
    pub fn every(
        &mut self,
        secs: f32,
        owner: Option<Entity>,
        callback: impl FnMut(&mut World) + Send + Sync + 'static,
    ) {
        self.entries.push(TimerEntry {
            remaining: secs.max(0.0),
            interval: Some(secs.max(0.0)),
            owner,
            action: TimerAction::Call(Box::new(callback)),
        });
    }

    /// Schedules a one-shot message to `target`'s scripts in `secs` game-seconds.
    pub fn after_message<M: Reflect + Clone + Send + Sync + 'static>(
        &mut self,
        secs: f32,
        owner: Option<Entity>,
        target: Entity,
        message: M,
    ) {
        self.entries.push(TimerEntry {
            remaining: secs.max(0.0),
            interval: None,
            owner,
            action: TimerAction::Message {
                target,
                envelope: Box::new(TypedMessage { message }),
            },
        });
    }

    /// Number of live timers (one-shot and repeating).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no timer is scheduled.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every scheduled timer as observable data, in scheduling order (the agent
    /// protocol's `timers.list`, `book/agents/runtime-gameplay.md`). Callbacks are reported
    /// by kind only; message timers report their target and the reflected message's type path.
    pub fn infos(&self) -> Vec<TimerInfo> {
        self.entries
            .iter()
            .map(|entry| TimerInfo {
                remaining_secs: entry.remaining,
                interval_secs: entry.interval,
                owner: entry.owner,
                action: match &entry.action {
                    TimerAction::Call(_) => TimerInfoAction::Call,
                    TimerAction::Message { target, envelope } => TimerInfoAction::Message {
                        target: *target,
                        message_type: envelope.type_path(),
                    },
                },
            })
            .collect()
    }

    /// Cancels every timer owned by `owner`.
    pub fn cancel_all(&mut self, owner: Entity) {
        self.entries.retain(|entry| entry.owner != Some(owner));
    }
}

/// What one scheduled timer does when it fires, as observable data ([`TimerHub::infos`]).
#[derive(Debug, Clone, PartialEq)]
pub enum TimerInfoAction {
    /// A callback closure. Reported by kind only (the closure itself is not observable);
    /// it does not survive a hot reload (see the module docs).
    Call,
    /// Delivers a reflected message to `target`'s scripts on fire. Survives a hot reload.
    Message {
        /// The receiving entity.
        target: Entity,
        /// The reflected message's type path.
        message_type: &'static str,
    },
}

/// One scheduled timer as observable data ([`TimerHub::infos`]).
#[derive(Debug, Clone, PartialEq)]
pub struct TimerInfo {
    /// Seconds until the timer fires (or until the next repeat).
    pub remaining_secs: f32,
    /// The repeat interval for a repeating timer (`None` for a one-shot).
    pub interval_secs: Option<f32>,
    /// The owning entity: the timer cancels itself when the owner no longer exists.
    pub owner: Option<Entity>,
    /// What the timer does on fire.
    pub action: TimerInfoAction,
}

/// Schedules a one-shot callback (`TimerHub::after`, inserting the resource when absent).
pub fn schedule_after(
    world: &mut World,
    secs: f32,
    owner: Option<Entity>,
    callback: impl FnMut(&mut World) + Send + Sync + 'static,
) {
    world
        .get_resource_or_insert_with(TimerHub::default)
        .after(secs, owner, callback);
}

/// Schedules a repeating callback (`TimerHub::every`, inserting the resource when absent).
pub fn schedule_every(
    world: &mut World,
    secs: f32,
    owner: Option<Entity>,
    callback: impl FnMut(&mut World) + Send + Sync + 'static,
) {
    world
        .get_resource_or_insert_with(TimerHub::default)
        .every(secs, owner, callback);
}

/// Schedules a one-shot message timer (`TimerHub::after_message`, inserting resources as needed).
pub fn schedule_message_after<M: Reflect + Clone + Send + Sync + 'static>(
    world: &mut World,
    secs: f32,
    owner: Option<Entity>,
    target: Entity,
    message: M,
) {
    world
        .get_resource_or_insert_with(TimerHub::default)
        .after_message(secs, owner, target, message);
}

/// `FixedUpdate` (before the fixed script pass): advances every timer by one fixed-step delta,
/// fires the ones that elapsed, and drops the ones whose owner is gone.
///
/// Exclusive: callbacks mutate the world. Owner checks use the live entity registry, so a
/// despawned owner's timers — including message timers — never fire.
#[tracing::instrument(skip_all, target = "regia_framework", name = "timers_tick")]
pub(crate) fn tick_timers(world: &mut World) {
    let delta = world.resource::<Time>().delta_secs();
    let Some(mut hub) = world.remove_resource::<TimerHub>() else {
        return;
    };

    let mut fired: Vec<usize> = Vec::new();
    for (index, entry) in hub.entries.iter_mut().enumerate() {
        // Owner-bound: cancelled on despawn.
        if let Some(owner) = entry.owner
            && world.get_entity(owner).is_err()
        {
            continue;
        }
        entry.remaining -= delta;
        if entry.remaining <= 0.0 {
            fired.push(index);
        }
    }

    // Entries stay in place until every callback has run, so firing in start order is index-safe.
    // Repeating timers rearm in place; newly scheduled timers go into a separate hub until the merge.
    for &index in &fired {
        let Some(entry) = hub.entries.get_mut(index) else {
            continue;
        };
        if let Some(owner) = entry.owner
            && world.get_entity(owner).is_err()
        {
            continue;
        }
        if entry.interval.is_some() {
            // Repeating timers hold callbacks only (message timers are one-shot); run in place
            // and rearm relative to the fire time.
            if let TimerAction::Call(callback) = &mut entry.action {
                callback(world);
            }
            entry.remaining += entry.interval.unwrap_or(0.0).max(delta.max(0.0));
        } else {
            // One-shot: take the action out so message envelopes move into the queue.
            let action = std::mem::replace(&mut entry.action, TimerAction::Call(Box::new(|_| {})));
            match action {
                TimerAction::Call(mut callback) => callback(world),
                TimerAction::Message { target, envelope } => {
                    world
                        .get_resource_or_insert_with(ScriptMessages::default)
                        .queue
                        .push((MessageTarget::Entity(target), envelope));
                }
            }
        }
    }

    // Drop elapsed one-shots and orphaned entries; repeating timers stay (rearmed above).
    hub.entries = hub
        .entries
        .into_iter()
        .enumerate()
        .filter(|(index, entry)| {
            // `fired` is in index order: a binary search, not a scan per entry (ADR-385).
            let fired = fired.binary_search(index).is_ok();
            let orphaned = entry.owner.is_some_and(|o| world.get_entity(o).is_err());
            !orphaned && (entry.interval.is_some() || (!fired && entry.remaining > 0.0))
        })
        .map(|(_, entry)| entry)
        .collect();

    // A callback may have scheduled timers: `schedule_*` inserted a fresh hub while ours was
    // removed. Merge its entries instead of letting the write-back drop them.
    if let Some(scheduled) = world.remove_resource::<TimerHub>() {
        hub.entries.extend(scheduled.entries);
    }
    world.insert_resource(hub);
}

/// Registers the [`TimerHub`] and ticks it every [`FixedUpdate`], before the fixed script pass.
pub struct TimerPlugin;

impl Plugin for TimerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TimerHub>().add_systems(
            FixedUpdate,
            tick_timers.before(crate::script::ScriptsSet::FixedUpdate),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Counts callback firings.
    #[derive(Resource, Default)]
    struct Count(u32);

    fn make_app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TimerPlugin, crate::script::ScriptPlugin))
            .init_resource::<Count>()
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
        let mut fixed = app.world_mut().resource_mut::<FixedTime>();
        fixed.hz = 10.0;
        fixed.max_substeps = 8;
        app
    }

    #[test]
    fn one_shot_fires_after_the_delay() {
        let mut app = make_app();
        schedule_after(app.world_mut(), 0.5, None, |world| {
            world.resource_mut::<Count>().0 += 1;
        });

        // 0.4 s: not yet.
        for _ in 0..4 {
            app.update();
        }
        assert_eq!(app.world().resource::<Count>().0, 0);

        // 0.5 s: fires exactly once.
        app.update();
        app.update();
        assert_eq!(app.world().resource::<Count>().0, 1);
        assert!(
            app.world().resource::<TimerHub>().is_empty(),
            "one-shot is removed"
        );
    }

    #[test]
    fn timers_fire_on_the_same_fixed_step_under_different_frame_pacing() {
        fn run(frame_steps: &[u32]) -> Vec<u32> {
            #[derive(Resource, Default)]
            struct Seen(Vec<u32>);

            let mut app = make_app();
            let mut fixed = app.world_mut().resource_mut::<FixedTime>();
            fixed.hz = 60.0;
            fixed.max_substeps = 8;
            schedule_after(app.world_mut(), 0.02, None, |world| {
                world.resource_mut::<Count>().0 += 1;
            });
            app.init_resource::<Seen>().add_systems(
                FixedUpdate,
                (|count: Res<Count>, mut seen: ResMut<Seen>| {
                    seen.0.push(count.0);
                })
                .after(tick_timers),
            );
            let step = app.world().resource::<FixedTime>().step();
            for &steps in frame_steps {
                app.insert_resource(TimeUpdateStrategy::ManualDuration(step * steps));
                app.update();
            }
            app.world_mut().remove_resource::<Seen>().unwrap_or_default().0
        }

        let hitch = run(&[3]);
        let short = run(&[1, 1, 1]);
        assert_eq!(
            hitch,
            [0, 1, 1],
            "the timer fires before fixed step two's observers"
        );
        assert_eq!(short, hitch, "frame partitioning does not change the firing step");
    }

    #[test]
    fn timers_that_elapse_together_fire_in_start_order() {
        #[derive(Resource, Default)]
        struct Order(Vec<u32>);

        let mut app = make_app();
        app.init_resource::<Order>();
        for id in [1, 2, 3] {
            schedule_after(app.world_mut(), 0.02, None, move |world| {
                world.resource_mut::<Order>().0.push(id);
            });
        }
        app.update();
        assert_eq!(app.world().resource::<Order>().0, [1, 2, 3]);
    }

    #[test]
    fn timer_callback_can_schedule_another_timer() {
        let mut app = make_app();
        schedule_after(app.world_mut(), 0.1, None, |world| {
            world.resource_mut::<Count>().0 += 1;
            schedule_after(world, 0.2, None, |world| {
                world.resource_mut::<Count>().0 += 10;
            });
        });

        for _ in 0..5 {
            app.update();
        }
        assert_eq!(
            app.world().resource::<Count>().0,
            11,
            "the chained timer fires after the first"
        );
        assert!(app.world().resource::<TimerHub>().is_empty());
    }

    #[test]
    fn a_timer_scheduled_from_a_callback_is_kept() {
        let mut app = make_app();
        schedule_after(app.world_mut(), 0.1, None, |world| {
            world.resource_mut::<Count>().0 += 1;
            schedule_after(world, 0.2, None, |world| {
                world.resource_mut::<Count>().0 += 10;
            });
        });
        for _ in 0..8 {
            app.update(); // 0.7 s of game time after the first frame
        }
        assert_eq!(
            app.world().resource::<Count>().0,
            11,
            "the timer the callback scheduled fired too"
        );
        assert!(app.world().resource::<TimerHub>().is_empty());
    }

    #[test]
    fn repeating_timer_rearms() {
        let mut app = make_app();
        schedule_every(app.world_mut(), 0.2, None, |world| {
            world.resource_mut::<Count>().0 += 1;
        });

        for _ in 0..10 {
            app.update(); // 1.0 s total
        }
        let count = app.world().resource::<Count>().0;
        assert_eq!(count, 5, "fires every 0.2 s over 1.0 s");
        assert_eq!(
            app.world().resource::<TimerHub>().len(),
            1,
            "repeating timer stays"
        );
    }

    #[test]
    fn owner_despawn_cancels() {
        let mut app = make_app();
        let owner = app.world_mut().spawn(()).id();
        schedule_every(app.world_mut(), 0.1, Some(owner), |world| {
            world.resource_mut::<Count>().0 += 1;
        });

        app.update(); // 0.1 s: fires
        assert_eq!(app.world().resource::<Count>().0, 1);

        app.world_mut().despawn(owner);
        app.update();
        app.update();
        assert_eq!(app.world().resource::<Count>().0, 1, "canceled on despawn");
        assert!(app.world().resource::<TimerHub>().is_empty());
    }

    #[test]
    fn paused_time_does_not_advance_timers() {
        let mut app = make_app();
        schedule_after(app.world_mut(), 0.3, None, |world| {
            world.resource_mut::<Count>().0 += 1;
        });
        app.world_mut().resource_mut::<Time>().paused = true;
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(
            app.world().resource::<Count>().0,
            0,
            "paused game time freezes timers"
        );
    }

    #[test]
    fn message_timer_delivers_to_scripts() {
        use crate::script::{Script, ScriptCtx, ScriptSlot};

        #[derive(Reflect, Default, Clone)]
        struct Tock;

        #[derive(Reflect, Default, Clone)]
        struct Receiver {
            got: bool,
        }

        impl Script for Receiver {
            fn on_message(&mut self, _ctx: &mut ScriptCtx, message: &dyn Reflect) {
                if message.as_partial_reflect().try_downcast_ref::<Tock>().is_some() {
                    self.got = true;
                }
            }
        }

        let mut app = make_app();
        let receiver = app
            .world_mut()
            .spawn(crate::script::Scripts::from_iter([ScriptSlot {
                type_path: "test::Receiver".into(),
                order: 0,
                instance: Some(Box::new(Receiver::default())),
                started: false,
                enabled: true,
            }]))
            .id();
        schedule_message_after(app.world_mut(), 0.2, None, receiver, Tock);

        for _ in 0..3 {
            app.update();
        }

        let got = ScriptCtx::new(app.world_mut(), receiver)
            .with_script::<Receiver, bool>(receiver, |r, _| r.got)
            .unwrap_or(false);
        assert!(got, "message timer delivered through the script queue");
    }
}
