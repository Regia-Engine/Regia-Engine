// SPDX-License-Identifier: MPL-2.0
//! Spawn order of a component that does not depend on entity ids (ADR-1347).
//!
//! `Commands::spawn` takes an entity id from a shared atomic allocator when the command is
//! *recorded*, not when it is applied. Two systems that spawn in the same schedule pass and have
//! no order between them run in parallel on the multi-threaded executor, so which of them gets
//! the lower ids depends on thread timing: on the compute pool size, on the machine and on the
//! run. The commands themselves are applied in a fixed order (system order, then command order),
//! so the order in which a component *arrives in the world* is repeatable while the ids are not.
//!
//! That fixed apply order holds for single-queue `Commands` only. `ParallelCommands` (also in
//! the bevy prelude) collects one queue per thread and applies them in `ThreadLocal` iteration
//! order — the order threads first touched the queues, not a thread index — so the cross-thread
//! arrival order is thread-timing order again. Components whose sort feeds a deterministic
//! simulation must not be inserted through `ParallelCommands`.
//!
//! Code that must process new entities in a repeatable order (rapier insertion, the script pass)
//! sorts them with [`SpawnOrder::key`] instead of by `Entity`. [`track_spawn_order`] installs an
//! `on_add` hook that numbers each arrival of `C` and an `on_remove` hook that forgets it, so
//! the resource only holds live entities.
//!
//! The key falls back to the entity id (index, then bits) for entities the hook never saw: ones
//! that had `C` before tracking started, or every entity when another plugin owns `C`'s hooks.
//! That fallback is not deterministic across runs (parallel spawns reserve ids in thread-timing
//! order), so a caller whose sort feeds a deterministic simulation must use
//! [`try_track_spawn_order`] and treat an error as fatal instead of calling [`track_spawn_order`].

use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;

use bevy_ecs::lifecycle::HookContext;
use bevy_ecs::prelude::{Component, Entity, Resource, World};
use bevy_ecs::reflect::ReflectResource;
use bevy_ecs::world::DeferredWorld;
use bevy_reflect::Reflect;
use bevy_reflect::std_traits::ReflectDefault;

/// Sort key of one entity: the arrival number of `C` (or `u64::MAX` when unknown), then the
/// entity index, then the entity bits.
pub type SpawnKey = (u64, u32, u64);

/// The arrival order of component `C` on entities, kept by the hooks [`track_spawn_order`]
/// installs. Live entities only: removing `C` (or despawning) drops the entry.
///
/// Reflected (the counter, the arrival numbers and the tracking flag; `C` is only a marker), so a
/// host that registers `SpawnOrder<C>` for its `C` can put the order in a world snapshot: a
/// restore that drops it would sort restored entities by entity id instead (ADR-8723).
#[derive(Resource, Debug, Reflect)]
#[reflect(Resource, Default)]
pub struct SpawnOrder<C: Component> {
    next: u64,
    seq: HashMap<Entity, u64>,
    tracking: bool,
    #[reflect(ignore)]
    _component: PhantomData<fn() -> C>,
}

impl<C: Component> Default for SpawnOrder<C> {
    fn default() -> Self {
        Self {
            next: 0,
            seq: HashMap::new(),
            tracking: false,
            _component: PhantomData,
        }
    }
}

impl<C: Component> SpawnOrder<C> {
    /// The arrival number of `C` on `entity`, if the hook saw it arrive.
    pub fn get(&self, entity: Entity) -> Option<u64> {
        self.seq.get(&entity).copied()
    }

    /// The sort key of `entity`: arrival number first, entity id as the fallback and tie-break.
    /// Sorting by it gives the order in which `C` was added to the world.
    ///
    /// Entities the hook never saw (tracking failed or started late) sort by entity id, which
    /// parallel spawns reserve in thread-timing order, so that fallback is not deterministic
    /// across runs.
    pub fn key(&self, entity: Entity) -> SpawnKey {
        (
            self.get(entity).unwrap_or(u64::MAX),
            entity.index().index(),
            entity.to_bits(),
        )
    }

    /// Sorts `items` by the [`SpawnOrder::key`] of the entity `entity_of` returns.
    pub fn sort_by_spawn<T>(&self, items: &mut [T], entity_of: impl Fn(&T) -> Entity) {
        items.sort_by_cached_key(|item| self.key(entity_of(item)));
    }

    /// Entities currently numbered.
    pub fn len(&self) -> usize {
        self.seq.len()
    }

    /// True when no entity is numbered.
    pub fn is_empty(&self) -> bool {
        self.seq.is_empty()
    }

    /// True when the hooks are installed. False means every key falls back to the entity id.
    pub fn is_tracking(&self) -> bool {
        self.tracking
    }

    fn note(&mut self, entity: Entity) {
        let n = self.next;
        self.next += 1;
        self.seq.insert(entity, n);
    }
}

fn note_added<C: Component>(mut world: DeferredWorld<'_>, context: HookContext) {
    if let Some(mut order) = world.get_resource_mut::<SpawnOrder<C>>() {
        order.note(context.entity);
    }
}

fn forget_removed<C: Component>(mut world: DeferredWorld<'_>, context: HookContext) {
    if let Some(mut order) = world.get_resource_mut::<SpawnOrder<C>>() {
        order.seq.remove(&context.entity);
    }
}

/// Why spawn-order tracking could not start ([`try_track_spawn_order`]).
///
/// Both variants leave [`SpawnOrder::key`] falling back to the entity id, which is not
/// deterministic across runs when the ids come from parallel `Commands::spawn` calls (they are
/// reserved at command-record time, in thread-timing order). Callers whose sort feeds a
/// deterministic simulation must treat either variant as fatal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackSpawnOrderError {
    /// An entity already holds `C`; bevy only lets component hooks be installed before the
    /// component's first use. Spawn-order tracking must be requested before anything inserts `C`.
    ComponentInUse,
    /// `C` already has an `on_add` or `on_remove` hook owned by someone else; bevy has one slot
    /// per hook. Compose the other owner with the tracking hooks, or track another component.
    HooksTaken,
}

/// Inserts the [`SpawnOrder<C>`] resource (if missing) and installs the hooks that keep it,
/// like [`track_spawn_order`], but returns *why* tracking could not start instead of only
/// warning. Idempotent: returns `Ok(())` whenever tracking is on.
pub fn try_track_spawn_order<C: Component>(world: &mut World) -> Result<(), TrackSpawnOrderError> {
    if world
        .get_resource::<SpawnOrder<C>>()
        .is_some_and(SpawnOrder::is_tracking)
    {
        return Ok(());
    }
    if world.get_resource::<SpawnOrder<C>>().is_none() {
        world.insert_resource(SpawnOrder::<C>::default());
    }
    let in_use = world
        .component_id::<C>()
        .is_some_and(|id| world.archetypes().iter().any(|archetype| archetype.contains(id)));
    if in_use {
        return Err(TrackSpawnOrderError::ComponentInUse);
    }
    let hooks = world.register_component_hooks::<C>();
    // Probe a copy first: installing `on_remove` and then failing `on_add` would leave the
    // component's only `on_remove` slot taken by `forget_removed` while tracking stays off.
    let mut probe = hooks.clone();
    let installed =
        probe.try_on_remove(forget_removed::<C>).is_some() && probe.try_on_add(note_added::<C>).is_some();
    if installed {
        *hooks = probe;
    } else {
        return Err(TrackSpawnOrderError::HooksTaken);
    }
    if let Some(mut order) = world.get_resource_mut::<SpawnOrder<C>>() {
        order.tracking = true;
    }
    Ok(())
}

/// Inserts the [`SpawnOrder<C>`] resource (if missing) and installs the hooks that keep it.
///
/// Returns true when tracking is on. It is off (and keys fall back to entity ids) when `C`
/// already has an `on_add` or `on_remove` hook, or when an entity already holds `C`: bevy only
/// lets hooks be installed before the component is first used. Calling it again is harmless.
///
/// The fallback order is the entity id, which parallel `Commands::spawn` calls reserve in
/// thread-timing order, so it is not deterministic across runs. When the sort feeds anything
/// that must replay identically (a physics simulation), call [`try_track_spawn_order`] instead
/// and treat an error as fatal.
pub fn track_spawn_order<C: Component>(world: &mut World) -> bool {
    match try_track_spawn_order::<C>(world) {
        Ok(()) => true,
        Err(TrackSpawnOrderError::ComponentInUse) => {
            tracing::warn!(
                target: "regia_ecs",
                "spawn order of {} is not tracked: entities already hold it; ordering falls back to entity ids",
                std::any::type_name::<C>()
            );
            false
        }
        Err(TrackSpawnOrderError::HooksTaken) => {
            tracing::warn!(
                target: "regia_ecs",
                "spawn order of {} is not tracked: the component already has hooks; ordering falls back to entity ids",
                std::any::type_name::<C>()
            );
            false
        }
    }
}

/// The arrival order of the entities of several components against ONE shared counter, kept by
/// the hooks [`track_shared_spawn_order`] installs (live entities only).
///
/// Where [`SpawnOrder`] numbers each component type from its own counter — so the sequence
/// numbers of two different components are not comparable — a shared counter numbers every
/// arrival of every tracked component in the order the hooks saw it. A [`SharedSpawnOrder::key`]
/// from it therefore orders a *mixed* population (a cloth and a soft body competing in one
/// particle budget) by true spawn arrival, while a per-type [`SpawnOrder`] cannot: its two
/// counters both start at 0, and an equal-distance tie would fall through to the entity id,
/// which parallel `Commands` reserve in thread-timing order (ADR-1347).
///
/// An entity that gains a second tracked component is numbered again (its first entry is
/// replaced): track only components no entity ever holds together.
#[derive(Resource, Debug, Default)]
pub struct SharedSpawnOrder {
    next: u64,
    seq: HashMap<Entity, u64>,
    tracked: HashSet<TypeId>,
}

impl SharedSpawnOrder {
    /// The shared arrival number of `entity`, if a hook saw it arrive.
    pub fn get(&self, entity: Entity) -> Option<u64> {
        self.seq.get(&entity).copied()
    }

    /// The sort key of `entity`: the shared arrival number first, the entity id as the fallback
    /// and tie-break (the same shape as [`SpawnOrder::key`]). Entities the hooks never saw sort
    /// by entity id, which is not deterministic across runs (parallel `Commands` reserve ids in
    /// thread-timing order).
    pub fn key(&self, entity: Entity) -> SpawnKey {
        (
            self.get(entity).unwrap_or(u64::MAX),
            entity.index().index(),
            entity.to_bits(),
        )
    }

    /// True once at least one component's hooks are installed.
    pub fn is_tracking(&self) -> bool {
        !self.tracked.is_empty()
    }

    /// Entities currently numbered.
    pub fn len(&self) -> usize {
        self.seq.len()
    }

    /// True when no entity is numbered.
    pub fn is_empty(&self) -> bool {
        self.seq.is_empty()
    }

    fn note(&mut self, entity: Entity) {
        let n = self.next;
        self.next += 1;
        self.seq.insert(entity, n);
    }
}

fn note_added_shared(mut world: DeferredWorld<'_>, context: HookContext) {
    if let Some(mut order) = world.get_resource_mut::<SharedSpawnOrder>() {
        order.note(context.entity);
    }
}

fn forget_removed_shared(mut world: DeferredWorld<'_>, context: HookContext) {
    if let Some(mut order) = world.get_resource_mut::<SharedSpawnOrder>() {
        order.seq.remove(&context.entity);
    }
}

/// Inserts the [`SharedSpawnOrder`] resource (if missing) and installs the hooks that keep it,
/// like [`try_track_spawn_order`] but numbering `C`'s arrivals on the shared counter. Idempotent
/// per component: returns `Ok(())` whenever `C` is already tracked.
///
/// The same failure modes as [`try_track_spawn_order`] apply; a caller whose cross-type sort
/// feeds a deterministic simulation must treat an error as fatal, because the remaining
/// fallback is the entity id, which is not deterministic across runs.
pub fn try_track_shared_spawn_order<C: Component>(world: &mut World) -> Result<(), TrackSpawnOrderError> {
    if world.get_resource::<SharedSpawnOrder>().is_none() {
        world.insert_resource(SharedSpawnOrder::default());
    }
    if world
        .get_resource::<SharedSpawnOrder>()
        .is_some_and(|order| order.tracked.contains(&TypeId::of::<C>()))
    {
        return Ok(());
    }
    let in_use = world
        .component_id::<C>()
        .is_some_and(|id| world.archetypes().iter().any(|archetype| archetype.contains(id)));
    if in_use {
        return Err(TrackSpawnOrderError::ComponentInUse);
    }
    let hooks = world.register_component_hooks::<C>();
    // Probe a copy first, exactly as `try_track_spawn_order` does: installing `on_remove` and
    // then failing `on_add` would leave the component's only `on_remove` slot taken while
    // tracking stays off.
    let mut probe = hooks.clone();
    let installed =
        probe.try_on_remove(forget_removed_shared).is_some() && probe.try_on_add(note_added_shared).is_some();
    if installed {
        *hooks = probe;
    } else {
        return Err(TrackSpawnOrderError::HooksTaken);
    }
    if let Some(mut order) = world.get_resource_mut::<SharedSpawnOrder>() {
        order.tracked.insert(TypeId::of::<C>());
    }
    Ok(())
}

/// Inserts the [`SharedSpawnOrder`] resource (if missing) and installs the hooks that keep it,
/// numbering `C`'s arrivals on the shared counter. Returns true when `C` is tracked; the same
/// fallback and warnings as [`track_spawn_order`] apply. Calling it again is harmless.
pub fn track_shared_spawn_order<C: Component>(world: &mut World) -> bool {
    match try_track_shared_spawn_order::<C>(world) {
        Ok(()) => true,
        Err(TrackSpawnOrderError::ComponentInUse) => {
            tracing::warn!(
                target: "regia_ecs",
                "shared spawn order of {} is not tracked: entities already hold it; ordering falls back to entity ids",
                std::any::type_name::<C>()
            );
            false
        }
        Err(TrackSpawnOrderError::HooksTaken) => {
            tracing::warn!(
                target: "regia_ecs",
                "shared spawn order of {} is not tracked: the component already has hooks; ordering falls back to entity ids",
                std::any::type_name::<C>()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::*;

    #[derive(Component)]
    struct Body;

    #[derive(Component)]
    struct Other;

    fn sorted(world: &World, mut entities: Vec<Entity>) -> Vec<Entity> {
        let order = world.resource::<SpawnOrder<Body>>();
        order.sort_by_spawn(&mut entities, |e| *e);
        entities
    }

    #[test]
    fn arrival_order_wins_over_entity_ids() {
        let mut world = World::new();
        assert!(track_spawn_order::<Body>(&mut world));
        // Reserve two ids, then add the component to the higher one first.
        let low = world.spawn(Other).id();
        let high = world.spawn(Other).id();
        assert!(low.index().index() < high.index().index());
        world.entity_mut(high).insert(Body);
        world.entity_mut(low).insert(Body);
        assert_eq!(sorted(&world, vec![low, high]), vec![high, low]);
        let order = world.resource::<SpawnOrder<Body>>();
        assert_eq!(order.get(high), Some(0));
        assert_eq!(order.get(low), Some(1));
    }

    #[test]
    fn commands_apply_in_recorded_order_whatever_ids_they_reserved() {
        let mut world = World::new();
        track_spawn_order::<Body>(&mut world);
        // Two command queues, as two parallel systems would have: the second queue reserves
        // its id first (it "ran first"), but the first queue is applied first.
        let mut first = bevy_ecs::world::CommandQueue::default();
        let mut second = bevy_ecs::world::CommandQueue::default();
        let b = Commands::new(&mut second, &world).spawn(Body).id();
        let a = Commands::new(&mut first, &world).spawn(Body).id();
        first.apply(&mut world);
        second.apply(&mut world);
        assert_eq!(sorted(&world, vec![b, a]), vec![a, b]);
    }

    #[test]
    fn removal_and_despawn_forget_the_entity() {
        let mut world = World::new();
        track_spawn_order::<Body>(&mut world);
        let a = world.spawn(Body).id();
        let b = world.spawn(Body).id();
        assert_eq!(world.resource::<SpawnOrder<Body>>().len(), 2);
        world.entity_mut(a).remove::<Body>();
        assert!(world.despawn(b));
        assert!(world.resource::<SpawnOrder<Body>>().is_empty());
        // Re-adding numbers it again, after everything before.
        world.entity_mut(a).insert(Body);
        assert_eq!(world.resource::<SpawnOrder<Body>>().get(a), Some(2));
    }

    #[test]
    fn untracked_entities_sort_last_by_id() {
        let mut world = World::new();
        let early = world.spawn(Body).id();
        // `Body` is in use: tracking cannot start, every key falls back to the id.
        assert!(!track_spawn_order::<Body>(&mut world));
        assert!(!world.resource::<SpawnOrder<Body>>().is_tracking());
        let late = world.spawn(Body).id();
        assert_eq!(world.resource::<SpawnOrder<Body>>().get(late), None);
        assert_eq!(sorted(&world, vec![late, early]), vec![early, late]);
    }

    #[derive(Component)]
    struct HasAddHook;

    #[test]
    fn failed_tracking_leaves_the_remove_hook_free() {
        let mut world = World::new();
        world
            .register_component_hooks::<HasAddHook>()
            .on_add(|_world, _context| {});
        assert!(!track_spawn_order::<HasAddHook>(&mut world));
        // The game's own remove hook must still be installable.
        assert!(
            world
                .register_component_hooks::<HasAddHook>()
                .try_on_remove(|_world, _context| {})
                .is_some()
        );
    }

    #[test]
    fn tracking_twice_is_harmless() {
        let mut world = World::new();
        assert!(track_spawn_order::<Body>(&mut world));
        assert!(track_spawn_order::<Body>(&mut world));
        let a = world.spawn(Body).id();
        assert_eq!(world.resource::<SpawnOrder<Body>>().get(a), Some(0));
    }

    #[test]
    fn try_track_reports_why_tracking_cannot_start() {
        // An entity already holds the component.
        let mut world = World::new();
        world.spawn(Body);
        assert_eq!(
            try_track_spawn_order::<Body>(&mut world),
            Err(TrackSpawnOrderError::ComponentInUse)
        );
        assert!(!world.resource::<SpawnOrder<Body>>().is_tracking());

        // Another owner holds a hook slot.
        let mut world = World::new();
        world
            .register_component_hooks::<HasAddHook>()
            .on_add(|_world, _context| {});
        assert_eq!(
            try_track_spawn_order::<HasAddHook>(&mut world),
            Err(TrackSpawnOrderError::HooksTaken)
        );
        assert!(!world.resource::<SpawnOrder<HasAddHook>>().is_tracking());
        // The probe must not have consumed the remove slot (regression: failed tracking leaves
        // the remove hook free).
        assert!(
            world
                .register_component_hooks::<HasAddHook>()
                .try_on_remove(|_world, _context| {})
                .is_some()
        );

        // Happy path, idempotent.
        let mut world = World::new();
        assert_eq!(try_track_spawn_order::<Body>(&mut world), Ok(()));
        assert_eq!(try_track_spawn_order::<Body>(&mut world), Ok(()));
        let a = world.spawn(Body).id();
        assert_eq!(world.resource::<SpawnOrder<Body>>().get(a), Some(0));
    }

    /// The audit's fail-open scenario: one `Body` exists (tracking can never start), two command
    /// queues reserve ids so the second queue gets the lower id, and the first queue is applied
    /// first. The strict API must fail loudly here, because the remaining fallback sorts by
    /// thread-timing ids: `b` (lower id, arrived second) sorts before `a` (arrived first), and a
    /// rerun on another machine can flip them.
    #[test]
    fn untracked_world_fails_strict_tracking_instead_of_blessing_id_order() {
        let mut world = World::new();
        world.spawn(Body);
        let mut first = bevy_ecs::world::CommandQueue::default();
        let mut second = bevy_ecs::world::CommandQueue::default();
        let b = Commands::new(&mut second, &world).spawn(Body).id();
        let a = Commands::new(&mut first, &world).spawn(Body).id();
        assert!(
            b.index().index() < a.index().index(),
            "second queue reserved the lower id"
        );
        first.apply(&mut world);
        second.apply(&mut world);
        assert_eq!(
            try_track_spawn_order::<Body>(&mut world),
            Err(TrackSpawnOrderError::ComponentInUse)
        );
        assert!(!world.resource::<SpawnOrder<Body>>().is_tracking());
        // The hazard, pinned: the fallback sorts by id, not arrival.
        assert_eq!(sorted(&world, vec![a, b]), vec![b, a]);
    }

    /// A mixed population shares ONE counter: the sequence numbers of two different components
    /// are comparable, so a cross-type tie (a cloth and a soft body in one budget) resolves by
    /// true spawn arrival, not by entity id (framework3 review high 1).
    #[test]
    fn shared_counter_numbers_a_mixed_population_in_arrival_order() {
        let mut world = World::new();
        assert!(track_shared_spawn_order::<Body>(&mut world));
        assert!(track_shared_spawn_order::<Other>(&mut world));
        // Reserve two ids, then interleave the arrivals: Body on the higher id first, then
        // Other on the lower id.
        let low = world.spawn_empty().id();
        let high = world.spawn_empty().id();
        assert!(low.index().index() < high.index().index());
        world.entity_mut(high).insert(Body);
        world.entity_mut(low).insert(Other);
        let order = world.resource::<SharedSpawnOrder>();
        assert_eq!(order.get(high), Some(0));
        assert_eq!(order.get(low), Some(1));
        // The keys order across types by arrival, whatever the entity ids.
        assert!(order.key(high) < order.key(low));
        // Both components tracked: idempotent per component.
        assert!(track_shared_spawn_order::<Body>(&mut world));
        assert!(track_shared_spawn_order::<Other>(&mut world));
        // A component already in use cannot start tracking.
        let mut world = World::new();
        world.spawn(Body);
        assert_eq!(
            try_track_shared_spawn_order::<Body>(&mut world),
            Err(TrackSpawnOrderError::ComponentInUse)
        );
        // Removal forgets the entity.
        let mut world = World::new();
        track_shared_spawn_order::<Body>(&mut world);
        let a = world.spawn(Body).id();
        assert!(world.despawn(a));
        assert!(world.resource::<SharedSpawnOrder>().is_empty());
    }

    /// The module docs' `ParallelCommands` caveat, pinned with the real thing: two systems spawn
    /// through `ParallelCommands` from two threads; each queues a log entry right behind its
    /// spawn, so the log records the order in which the per-thread queues were applied (the
    /// `ThreadLocal` walk). The arrival numbers — and hence the spawn keys — must increase
    /// along that walk on every run, never follow the entity-id reservation order or a thread
    /// index. (Muse review of fix-ecs-determinism, finding 2: the previous test applied two
    /// hand-built queues and never touched `ParallelCommands`.)
    #[test]
    fn parallel_commands_arrival_numbers_follow_the_apply_walk() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        /// Both spawners wait until the other has started, so on a multi-threaded host they
        /// really do record their commands from two threads. A fixed spin count (no wall
        /// clock): on a single-threaded host the wait laps harmlessly and the systems run back
        /// to back, which exercises the same code path with one thread queue.
        static STARTED: AtomicUsize = AtomicUsize::new(0);

        fn both_running() {
            STARTED.fetch_add(1, AtomicOrdering::SeqCst);
            for _ in 0..1_000_000 {
                if STARTED.load(AtomicOrdering::SeqCst) >= 2 {
                    break;
                }
                std::hint::spin_loop();
            }
        }

        /// One entry per applied queue: the spawning system's label and the entity it spawned.
        #[derive(Resource, Default)]
        struct ApplyLog(Vec<(char, Entity)>);

        fn spawn_labeled(which: char) -> impl FnMut(ParallelCommands) {
            move |commands: ParallelCommands| {
                both_running();
                commands.command_scope(|mut queue| {
                    let entity = queue.spawn(Body).id();
                    queue.queue(move |world: &mut World| {
                        world.resource_mut::<ApplyLog>().0.push((which, entity));
                    });
                });
            }
        }

        let mut world = World::new();
        track_spawn_order::<Body>(&mut world);
        world.init_resource::<ApplyLog>();
        STARTED.store(0, AtomicOrdering::SeqCst);
        let mut schedule = Schedule::default();
        schedule.add_systems((spawn_labeled('a'), spawn_labeled('b')));
        schedule.run(&mut world);

        let log = std::mem::take(&mut world.resource_mut::<ApplyLog>().0);
        assert_eq!(log.len(), 2, "both spawners applied: {log:?}");
        let order = world.resource::<SpawnOrder<Body>>();
        assert!(
            order.is_tracking() && log.iter().all(|(_, e)| order.get(*e).is_some()),
            "every ParallelCommands arrival must be numbered: {log:?}"
        );
        let numbers: Vec<u64> = log
            .iter()
            .map(|(_, e)| order.get(*e).expect("numbered"))
            .collect();
        assert!(
            numbers.windows(2).all(|w| w[0] < w[1]),
            "arrival numbers must follow the apply walk {log:?}, got {numbers:?}"
        );
        // The walk, not the id reservation, decided the numbering: the two entities were
        // spawned concurrently, so whichever reserved the lower id is timing-dependent and
        // must not be assumed to have arrived first.
    }
}
