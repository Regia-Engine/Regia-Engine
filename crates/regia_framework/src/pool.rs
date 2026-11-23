// SPDX-License-Identifier: MPL-2.0
//! Prefab pool (`docs/specs/14-scene-and-framework.md` § B6): recycle pre-spawned entities for
//! frequent spawns such as projectiles.
//!
//! A [`PrefabPool`] resource keeps a free list per key. [`PrefabPool::acquire`] pops a free entity
//! (reactivating it) or spawns a fresh one through a factory closure; [`PrefabPool::recycle`]
//! returns an entity to the pool by tagging it [`PooledInactive`] instead of despawning it. Game
//! systems skip `PooledInactive` entities (or remove their visible/physical components on recycle),
//! so the entity count stays flat under continuous spawn pressure — no allocator churn, no
//! per-shot spawn cost.
//!
//! The pool is deliberately dumb: it manages entity identity and lifecycle only. Resetting a
//! pooled entity (position, velocity, visibility) is the caller's job, because the "template" is
//! game-specific. `Pooled { key }` records which pool an entity belongs to.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_ecs::ecs::lifecycle::HookContext;
use regia_ecs::ecs::world::DeferredWorld;

/// Marks an entity as owned by a [`PrefabPool`] (bookkeeping; `key` names the pool). Removing it
/// (or despawning the entity) takes the entity out of the world's [`PrefabPool`] resource.
#[derive(Component, Reflect, Debug, Clone, PartialEq, Eq)]
#[reflect(Component)]
#[component(on_remove = pooled_removed)]
pub struct Pooled {
    /// The pool key the entity belongs to.
    pub key: String,
}

/// Marks a pooled entity as currently free (inactive). Gameplay systems must skip these.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct PooledInactive;

/// Pools of reusable entities, keyed by name.
///
/// `acquire` never fails: when the free list is empty it calls the factory. `recycle` is a no-op
/// on an entity that is not a [`Pooled`] member of that key; for an id despawned while acquired it
/// only stops counting it as live.
#[derive(Resource, Debug, Default)]
pub struct PrefabPool {
    free: BTreeMap<String, Vec<Entity>>,
    /// The acquired (active) entities of each key.
    live: BTreeMap<String, BTreeSet<Entity>>,
}

/// `Pooled` `on_remove` hook: an entity despawned (or no longer pooled) leaves the world's pool,
/// so [`PrefabPool::live_len`] does not count it and `acquire` never hands it out.
fn pooled_removed(mut world: DeferredWorld<'_>, context: HookContext) {
    let Some(key) = world.get::<Pooled>(context.entity).map(|p| p.key.clone()) else {
        return;
    };
    if let Some(mut pool) = world.get_resource_mut::<PrefabPool>() {
        pool.forget(&key, context.entity);
    }
}

impl PrefabPool {
    /// Returns a free entity from `key`, spawning a fresh one via `factory` when the free list is
    /// empty. The returned entity has `Pooled { key }` and is *not* `PooledInactive` (it is active
    /// and ready for the caller to reset). `factory` is called exactly once per missing entity.
    pub fn acquire(
        &mut self,
        world: &mut World,
        key: &str,
        factory: impl FnOnce(&mut World) -> Entity,
    ) -> Entity {
        // Skip free ids that were despawned while pooled (level unload, manual despawn).
        while let Some(entity) = self.free.get_mut(key).and_then(|list| list.pop()) {
            if let Ok(mut entity_mut) = world.get_entity_mut(entity) {
                entity_mut.remove::<PooledInactive>();
                if let Some(live) = self.live.get_mut(key) {
                    live.insert(entity);
                } else {
                    self.live.insert(key.to_string(), [entity].into_iter().collect());
                }
                return entity;
            }
        }
        let entity = factory(world);
        world.entity_mut(entity).insert(Pooled { key: key.to_string() });
        if let Some(live) = self.live.get_mut(key) {
            live.insert(entity);
        } else {
            self.live.insert(key.to_string(), [entity].into_iter().collect());
        }
        entity
    }

    /// Returns `entity` to pool `key`: tags it [`PooledInactive`] and records it on the free list.
    /// Callers are responsible for hiding/resetting the entity (removing physics handles, mesh
    /// visibility, etc.) so it does not keep simulating while pooled.
    ///
    /// An entity that is not `Pooled { key }` (never pooled, or another key's) is left alone. An
    /// id despawned while acquired is only taken off the live count.
    pub fn recycle(&mut self, world: &mut World, key: &str, entity: Entity) {
        let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
            // Despawned while acquired (a level unload): it is not live any more.
            self.forget(key, entity);
            return;
        };
        if entity_mut.get::<Pooled>().is_none_or(|p| p.key != key) {
            return; // not a member of this pool
        }
        if entity_mut.contains::<PooledInactive>() {
            return; // already free: never list an id twice
        }
        entity_mut.insert(PooledInactive);
        if let Some(list) = self.free.get_mut(key) {
            list.push(entity);
        } else {
            self.free.insert(key.to_string(), vec![entity]);
        }
        if let Some(live) = self.live.get_mut(key) {
            live.remove(&entity);
        }
    }

    /// Takes `entity` out of pool `key` (live set and free list): it was despawned or unpooled.
    fn forget(&mut self, key: &str, entity: Entity) {
        if let Some(live) = self.live.get_mut(key) {
            live.remove(&entity);
        }
        if let Some(free) = self.free.get_mut(key) {
            free.retain(|e| *e != entity);
        }
    }

    /// How many free (inactive) entities pool `key` currently holds.
    pub fn free_len(&self, key: &str) -> usize {
        self.free.get(key).map(Vec::len).unwrap_or(0)
    }

    /// How many entities of pool `key` are currently acquired (active): acquired, not recycled,
    /// and not despawned (a despawn is seen by the world's pool at once, by a pool held elsewhere
    /// when the id is recycled).
    pub fn live_len(&self, key: &str) -> usize {
        self.live.get(key).map_or(0, BTreeSet::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_spawns_and_marks_the_entity() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let mut pool = PrefabPool::default();

        let entity = pool.acquire(app.world_mut(), "bullet", |world| world.spawn(()).id());
        assert!(app.world().get::<Pooled>(entity).is_some());
        assert!(app.world().get::<PooledInactive>(entity).is_none());
        assert_eq!(pool.live_len("bullet"), 1);
        assert_eq!(pool.free_len("bullet"), 0);
    }

    #[test]
    fn recycle_returns_the_entity_to_the_free_list_and_reuse_reactivates() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let mut pool = PrefabPool::default();

        let first = pool.acquire(app.world_mut(), "bullet", |world| world.spawn(()).id());
        pool.recycle(app.world_mut(), "bullet", first);
        assert_eq!(pool.free_len("bullet"), 1);
        assert_eq!(pool.live_len("bullet"), 0);
        assert!(app.world().get::<PooledInactive>(first).is_some());

        // The recycled entity is reused: no second factory call.
        let second = pool.acquire(app.world_mut(), "bullet", |_| panic!("factory must not run"));
        assert_eq!(second, first, "the free entity is reused");
        assert!(app.world().get::<PooledInactive>(second).is_none());
        assert_eq!(pool.free_len("bullet"), 0);
        assert_eq!(pool.live_len("bullet"), 1);
    }

    #[test]
    fn recycle_on_a_despawned_entity_is_a_no_op() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let mut pool = PrefabPool::default();
        let entity = pool.acquire(app.world_mut(), "b", |world| world.spawn(()).id());
        app.world_mut().despawn(entity);
        pool.recycle(app.world_mut(), "b", entity); // must not panic
        assert_eq!(pool.free_len("b"), 0);
    }

    #[test]
    fn acquire_skips_despawned_free_entities_and_runs_the_factory() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let mut pool = PrefabPool::default();
        let stale = pool.acquire(app.world_mut(), "b", |world| world.spawn(()).id());
        pool.recycle(app.world_mut(), "b", stale);
        app.world_mut().despawn(stale);

        let fresh = pool.acquire(app.world_mut(), "b", |world| world.spawn(()).id());
        assert_ne!(fresh, stale);
        assert!(app.world().get_entity(fresh).is_ok());
        assert_eq!(pool.live_len("b"), 1);
        assert_eq!(pool.free_len("b"), 0);
    }

    #[test]
    fn double_recycle_does_not_alias_the_entity() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let mut pool = PrefabPool::default();
        let entity = pool.acquire(app.world_mut(), "b", |world| world.spawn(()).id());
        pool.recycle(app.world_mut(), "b", entity);
        pool.recycle(app.world_mut(), "b", entity);
        assert_eq!(pool.free_len("b"), 1);

        let first = pool.acquire(app.world_mut(), "b", |world| world.spawn(()).id());
        let second = pool.acquire(app.world_mut(), "b", |world| world.spawn(()).id());
        assert_eq!(first, entity);
        assert_ne!(second, first, "the second acquire gets a distinct entity");
    }

    /// `recycle` of an entity that was never pooled, or that belongs to another key, changes
    /// nothing (audit framework5 #10): no tag, no free-list entry, no count change.
    #[test]
    fn recycle_refuses_a_stray_or_another_key_s_entity() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let mut pool = PrefabPool::default();
        let bullet = pool.acquire(app.world_mut(), "bullet", |world| world.spawn(()).id());
        let rocket = pool.acquire(app.world_mut(), "rocket", |world| world.spawn(()).id());
        let stray = app.world_mut().spawn(()).id();

        pool.recycle(app.world_mut(), "bullet", stray);
        pool.recycle(app.world_mut(), "bullet", rocket);
        assert_eq!(pool.free_len("bullet"), 0);
        assert_eq!(pool.live_len("bullet"), 1, "the bullet is still out");
        assert_eq!(pool.live_len("rocket"), 1, "the rocket is still out");
        assert!(app.world().get::<PooledInactive>(stray).is_none());
        assert!(app.world().get::<PooledInactive>(rocket).is_none());
        let next = pool.acquire(app.world_mut(), "bullet", |world| world.spawn(()).id());
        assert!(next != stray && next != rocket && next != bullet);
    }

    /// An acquired entity that is despawned instead of recycled stops counting as live (audit
    /// framework5 #10): through the `Pooled` hook while the pool is the world's resource, and
    /// through `recycle` of the despawned id for a pool held elsewhere.
    #[test]
    fn a_despawned_acquired_entity_is_not_live() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.init_resource::<PrefabPool>();
        let a = app
            .world_mut()
            .resource_scope(|world, mut pool: Mut<PrefabPool>| {
                pool.acquire(world, "bullet", |w| w.spawn(()).id())
            });
        let b = app
            .world_mut()
            .resource_scope(|world, mut pool: Mut<PrefabPool>| {
                pool.acquire(world, "bullet", |w| w.spawn(()).id())
            });
        assert_eq!(app.world().resource::<PrefabPool>().live_len("bullet"), 2);
        // A level unload despawns both.
        app.world_mut().despawn(a);
        app.world_mut().despawn(b);
        assert_eq!(app.world().resource::<PrefabPool>().live_len("bullet"), 0);

        // A pool kept outside the world learns of it when the id is recycled.
        let mut pool = PrefabPool::default();
        let c = pool.acquire(app.world_mut(), "bullet", |world| world.spawn(()).id());
        app.world_mut().despawn(c);
        assert_eq!(pool.live_len("bullet"), 1, "not told yet");
        pool.recycle(app.world_mut(), "bullet", c);
        assert_eq!(pool.live_len("bullet"), 0);
        assert_eq!(pool.free_len("bullet"), 0);
    }

    #[test]
    fn pools_are_independent_by_key() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let mut pool = PrefabPool::default();
        let _a = pool.acquire(app.world_mut(), "rocket", |world| world.spawn(()).id());
        let _b = pool.acquire(app.world_mut(), "bullet", |world| world.spawn(()).id());
        assert_eq!(pool.live_len("rocket"), 1);
        assert_eq!(pool.live_len("bullet"), 1);
        assert_eq!(pool.free_len("rocket"), 0);
    }

    /// Recycle and the following acquire of a warm pool do not allocate the key.
    #[test]
    fn reuse_does_not_allocate_the_pool_key() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let mut pool = PrefabPool::default();
        let entity = pool.acquire(app.world_mut(), "bullet", |world| world.spawn(()).id());
        pool.recycle(app.world_mut(), "bullet", entity);
        let entity = pool.acquire(app.world_mut(), "bullet", |_| panic!("factory must not run"));
        pool.recycle(app.world_mut(), "bullet", entity);
        let entity = pool.acquire(app.world_mut(), "bullet", |_| panic!("factory must not run"));
        let allocated = crate::test_alloc::count(|| {
            pool.recycle(app.world_mut(), "bullet", entity);
            let again = pool.acquire(app.world_mut(), "bullet", |_| panic!("factory must not run"));
            assert_eq!(again, entity);
        });
        assert_eq!(allocated, 0, "reuse allocated {allocated} times");
        assert_eq!(pool.live_len("bullet"), 1);
        assert_eq!(pool.free_len("bullet"), 0);
    }
}
