// SPDX-License-Identifier: MPL-2.0
//! The world seam mods see (`regia.find`, entity handle methods, `regia.spawn`).

use regia_core::math::Vec3;

/// Component values as they cross the world seam: `(property, value)` pairs, sorted by property
/// name.
pub(crate) type PropValues = Vec<(String, crate::ScriptValue)>;

/// World access for mod calls. Entities are the ECS entity bits (index and generation), so a
/// handle to a despawned entity, or to a slot reused by a new entity, is simply not alive.
///
/// Every method answers `None`/`false` for a dead entity; none may panic.
pub trait ModWorld {
    /// The entity named `name`; of several, the one with the lowest entity index, so the answer
    /// does not depend on the world's iteration order.
    fn find(&mut self, name: &str) -> Option<u64>;
    /// How many entities the lookups since the last take walked. The host charges them to the
    /// mod's instruction budget after the lookup, because the count hook cannot see host work. A
    /// world with a name index answers what it really did.
    fn take_scanned(&mut self) -> u64 {
        0
    }
    /// [`find`](Self::find) among the entities that are not in `except`. The default misses when
    /// the entity `find` answers is in `except`; a world that can look further should.
    fn find_except(&mut self, name: &str, except: &[u64]) -> Option<u64> {
        self.find(name).filter(|entity| !except.contains(entity))
    }
    /// Whether `entity` is alive.
    fn alive(&mut self, entity: u64) -> bool;
    /// The entity's name, when it is alive and named.
    fn name(&mut self, entity: u64) -> Option<String>;
    /// The entity's local translation, when it is alive and has a transform.
    fn position(&mut self, entity: u64) -> Option<Vec3>;
    /// Moves the entity; `false` when it is dead or has no transform.
    fn set_position(&mut self, entity: u64, position: Vec3) -> bool;
    /// Spawns a named entity at `position`, owned by mod `owner`.
    fn spawn(&mut self, owner: &str, name: &str, position: Vec3) -> Option<u64>;
    /// Despawns `entity` when it is alive and was spawned by mod `owner`; `false` otherwise.
    fn despawn(&mut self, owner: &str, entity: u64) -> bool;
    /// How many entities one [`find`](Self::find) looks at. The host charges one instruction per
    /// eight of them before the lookup runs (with [`take_scanned`](Self::take_scanned) charged after it).
    /// A world with a name index answers 1: the charge is for the probe, not the world's size.
    fn find_cost(&mut self) -> u64 {
        1
    }
    /// The live entities mod `owner` spawned, sorted. A world that does not track owners answers
    /// nothing: then a load or a reload leaves a mod's entities alone and saves none of them.
    fn owned(&mut self, _owner: &str) -> Vec<u64> {
        Vec::new()
    }

    // --- Script components (`regia.component`, ADR-3356) ---
    //
    // The host validates values against the declaration before calling these; `decl` is a
    // declared component name. Every method answers `false`/`None` for a dead entity and leaves
    // a live entity without the component alone.

    /// Whether the live entity carries component `decl`.
    fn has_component(&mut self, _entity: u64, _decl: &str) -> bool {
        false
    }
    /// Attaches component `decl` to the live entity with `initial` values (the declaration's
    /// defaults); `false` when the entity is dead or already carries it.
    fn attach_component(&mut self, _entity: u64, _decl: &str, _initial: PropValues) -> bool {
        false
    }
    /// Removes component `decl` from the live entity; `false` when it does not carry it.
    fn remove_component(&mut self, _entity: u64, _decl: &str) -> bool {
        false
    }
    /// The entity's values for component `decl`; `None` when it does not carry it.
    fn component_props(&mut self, _entity: u64, _decl: &str) -> Option<PropValues> {
        None
    }
    /// Replaces the entity's values for component `decl`; `false` when it does not carry it.
    fn set_component_props(&mut self, _entity: u64, _decl: &str, _props: PropValues) -> bool {
        false
    }
    /// Replaces one property of component `decl`. The default reads every property and writes
    /// them back, so a world with no single-field write still stores the value. A world that
    /// can write one field should override this and leave the others unread.
    fn set_component_prop(&mut self, entity: u64, decl: &str, key: &str, value: crate::ScriptValue) -> bool {
        let Some(mut props) = self.component_props(entity, decl) else {
            return false;
        };
        match props.iter_mut().find(|(name, _)| name == key) {
            Some((_, existing)) => *existing = value,
            None => props.push((key.to_owned(), value)),
        }
        self.set_component_props(entity, decl, props)
    }
    /// How many live entities mod `owner` spawned. The default lists them. A world that keeps
    /// a count should answer that and leave the list unread.
    fn owned_count(&mut self, owner: &str) -> usize {
        self.owned(owner).len()
    }
    /// Every live entity carrying component `decl`, sorted by bits.
    fn entities_with(&mut self, _decl: &str) -> Vec<u64> {
        Vec::new()
    }
    /// How many entities one [`entities_with`](Self::entities_with) looks at (for the budget
    /// charge, like [`find_cost`](Self::find_cost)).
    fn component_cost(&mut self) -> u64 {
        1
    }
}

/// A world with nothing in it: every lookup misses and nothing spawns. Used when a mod call has
/// no world (tests, the agent's throwaway sandbox before an app exists).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWorld;

impl ModWorld for NoWorld {
    fn find(&mut self, _name: &str) -> Option<u64> {
        None
    }
    fn alive(&mut self, _entity: u64) -> bool {
        false
    }
    fn name(&mut self, _entity: u64) -> Option<String> {
        None
    }
    fn position(&mut self, _entity: u64) -> Option<Vec3> {
        None
    }
    fn set_position(&mut self, _entity: u64, _position: Vec3) -> bool {
        false
    }
    fn spawn(&mut self, _owner: &str, _name: &str, _position: Vec3) -> Option<u64> {
        None
    }
    fn despawn(&mut self, _owner: &str, _entity: u64) -> bool {
        false
    }
}

/// The world one hot reload sees (`docs/specs/84-lua-mods.md` § 12): writes reach the real world
/// but are recorded, so a failed reload can undo them. A despawn of an entity from before the
/// reload is held back until [`commit`](Self::commit).
pub(crate) struct JournalWorld<'a> {
    inner: &'a mut dyn ModWorld,
    /// Entities of the mod being reloaded when the reload began.
    before: Vec<u64>,
    spawned: Vec<(String, u64)>,
    moved: Vec<(u64, Vec3)>,
    held: Vec<(String, u64)>,
    /// Rollback state of the component writes the trial made, in the order they happened:
    /// rolled back newest first, so a component changed and removed in one trial restores its
    /// pre-trial values, and one added and removed nets to nothing.
    writes: Vec<ComponentWrite>,
}

/// One journaled component write ([`JournalWorld`]).
enum ComponentWrite {
    /// `attach_component` to undo: (entity, decl).
    Attached(u64, String),
    /// `remove_component` to undo: (entity, decl, values at removal).
    Removed(u64, String, PropValues),
    /// `set_component_props` to undo: (entity, decl, previous values).
    Set(u64, String, PropValues),
}

impl<'a> JournalWorld<'a> {
    /// A journal over `inner` for a reload of mod `owner`.
    pub(crate) fn new(inner: &'a mut dyn ModWorld, owner: &str) -> Self {
        let before = inner.owned(owner);
        Self {
            inner,
            before,
            spawned: Vec::new(),
            moved: Vec::new(),
            held: Vec::new(),
            writes: Vec::new(),
        }
    }

    fn is_held(&self, entity: u64) -> bool {
        self.held.iter().any(|(_, held)| *held == entity)
    }

    /// Makes the reload's writes final.
    pub(crate) fn commit(self) {
        for (owner, entity) in self.held {
            self.inner.despawn(&owner, entity);
        }
    }

    /// Undoes the reload's writes, newest first.
    pub(crate) fn rollback(self) {
        for write in self.writes.into_iter().rev() {
            match write {
                ComponentWrite::Attached(entity, decl) => {
                    self.inner.remove_component(entity, &decl);
                }
                ComponentWrite::Removed(entity, decl, props) => {
                    self.inner.attach_component(entity, &decl, props);
                }
                ComponentWrite::Set(entity, decl, props) => {
                    self.inner.set_component_props(entity, &decl, props);
                }
            }
        }
        for (entity, position) in self.moved.into_iter().rev() {
            self.inner.set_position(entity, position);
        }
        for (owner, entity) in self.spawned.into_iter().rev() {
            self.inner.despawn(&owner, entity);
        }
    }
}

impl ModWorld for JournalWorld<'_> {
    fn find(&mut self, name: &str) -> Option<u64> {
        let held: Vec<u64> = self.held.iter().map(|(_, entity)| *entity).collect();
        self.inner.find_except(name, &held)
    }

    fn alive(&mut self, entity: u64) -> bool {
        !self.is_held(entity) && self.inner.alive(entity)
    }

    fn name(&mut self, entity: u64) -> Option<String> {
        if self.is_held(entity) {
            return None;
        }
        self.inner.name(entity)
    }

    fn position(&mut self, entity: u64) -> Option<Vec3> {
        if self.is_held(entity) {
            return None;
        }
        self.inner.position(entity)
    }

    fn set_position(&mut self, entity: u64, position: Vec3) -> bool {
        if self.is_held(entity) {
            return false;
        }
        let Some(old) = self.inner.position(entity) else {
            return false;
        };
        let moved = self.inner.set_position(entity, position);
        if moved {
            self.moved.push((entity, old));
        }
        moved
    }

    fn spawn(&mut self, owner: &str, name: &str, position: Vec3) -> Option<u64> {
        let entity = self.inner.spawn(owner, name, position)?;
        self.spawned.push((owner.to_owned(), entity));
        Some(entity)
    }

    fn despawn(&mut self, owner: &str, entity: u64) -> bool {
        if let Some(index) = self.spawned.iter().position(|(_, e)| *e == entity) {
            let (spawner, _) = &self.spawned[index];
            if spawner != owner || !self.inner.despawn(owner, entity) {
                return false;
            }
            self.spawned.swap_remove(index);
            return true;
        }
        if !self.before.contains(&entity) || self.is_held(entity) {
            return false;
        }
        self.held.push((owner.to_owned(), entity));
        true
    }

    fn find_cost(&mut self) -> u64 {
        self.inner.find_cost()
    }

    fn take_scanned(&mut self) -> u64 {
        self.inner.take_scanned()
    }

    fn owned(&mut self, owner: &str) -> Vec<u64> {
        let mut owned = self.inner.owned(owner);
        owned.retain(|entity| !self.is_held(*entity));
        owned
    }

    fn has_component(&mut self, entity: u64, decl: &str) -> bool {
        !self.is_held(entity) && self.inner.has_component(entity, decl)
    }

    fn attach_component(
        &mut self,
        entity: u64,
        decl: &str,
        initial: Vec<(String, crate::ScriptValue)>,
    ) -> bool {
        if self.is_held(entity) || self.inner.has_component(entity, decl) {
            return false;
        }
        if self.inner.attach_component(entity, decl, initial) {
            self.writes
                .push(ComponentWrite::Attached(entity, decl.to_owned()));
            true
        } else {
            false
        }
    }

    fn remove_component(&mut self, entity: u64, decl: &str) -> bool {
        if self.is_held(entity) {
            return false;
        }
        let Some(props) = self.inner.component_props(entity, decl) else {
            return false;
        };
        if self.inner.remove_component(entity, decl) {
            self.writes
                .push(ComponentWrite::Removed(entity, decl.to_owned(), props));
            true
        } else {
            false
        }
    }

    fn component_props(&mut self, entity: u64, decl: &str) -> Option<Vec<(String, crate::ScriptValue)>> {
        if self.is_held(entity) {
            return None;
        }
        self.inner.component_props(entity, decl)
    }

    fn set_component_props(
        &mut self,
        entity: u64,
        decl: &str,
        props: Vec<(String, crate::ScriptValue)>,
    ) -> bool {
        if self.is_held(entity) {
            return false;
        }
        let Some(previous) = self.inner.component_props(entity, decl) else {
            return false;
        };
        if self.inner.set_component_props(entity, decl, props) {
            self.writes
                .push(ComponentWrite::Set(entity, decl.to_owned(), previous));
            true
        } else {
            false
        }
    }

    fn entities_with(&mut self, decl: &str) -> Vec<u64> {
        self.inner
            .entities_with(decl)
            .into_iter()
            .filter(|entity| !self.is_held(*entity))
            .collect()
    }

    fn component_cost(&mut self) -> u64 {
        self.inner.component_cost()
    }
}
