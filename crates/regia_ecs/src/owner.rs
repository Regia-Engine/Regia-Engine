// SPDX-License-Identifier: MPL-2.0
//! Registration ownership (`docs/specs/10-core.md` § 2.4, `docs/specs/20-hot-reload.md` §§ 3–4).
//!
//! Every type registration records the [`TypeOwner`] that was current when it was made. A system,
//! message or observer records an owner only when that owner is [`TypeOwner::Game`]; an engine
//! registration of those three is not stored. Hot reload (M11) uses the records to snapshot and tear down everything a game
//! plugin registered before the old library is dropped: types by `TypeId`, systems by
//! [`OwnerSet`] (`Schedule::remove_systems_in_set`), observers by entity, messages by `TypeId`.
//! [`TypeOwnership::owned_count`] must reach zero after teardown.

use std::any::TypeId;
use std::collections::HashMap;

use bevy_ecs::prelude::*;
use bevy_ecs::reflect::AppTypeRegistry;
use bevy_ecs::schedule::{InternedScheduleLabel, SystemKey};
use bevy_reflect::{GetTypeRegistration, Reflect};

/// Who made a registration.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub enum TypeOwner {
    /// The engine (and statically linked game code in shipping builds).
    #[default]
    Engine,
    /// A hot-reloadable game plugin, by plugin id.
    Game(u32),
}

/// System set every system added while a [`TypeOwner::Game`] owner is current is placed in, so the
/// whole set can be removed with `Schedule::remove_systems_in_set(OwnerSet(owner), ..)`.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OwnerSet(pub TypeOwner);

/// A type registration and its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedType {
    /// Who registered it first.
    pub owner: TypeOwner,
    /// Full type path.
    pub type_path: &'static str,
}

/// A system added to a schedule by a game owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSystem {
    /// Owner (always a [`TypeOwner::Game`]).
    pub owner: TypeOwner,
    /// Schedule the system was added to.
    pub schedule: InternedScheduleLabel,
    /// Key of the system in that schedule's graph.
    pub key: SystemKey,
    /// System name (`my_game::systems::open_doors`).
    pub name: String,
}

/// A message type registered by a game owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedMessage {
    /// Owner (always a [`TypeOwner::Game`]).
    pub owner: TypeOwner,
    /// `TypeId` of the message type.
    pub type_id: TypeId,
    /// Rust type name, for logs.
    pub type_name: &'static str,
}

/// An observer entity spawned by a game owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedObserver {
    /// Owner (always a [`TypeOwner::Game`]).
    pub owner: TypeOwner,
    /// The observer entity; despawning it removes the observer.
    pub entity: Entity,
}

/// Ownership records of an app (a resource). Also tracks the owner new registrations are recorded
/// under.
#[derive(Resource, Debug, Default)]
pub struct TypeOwnership {
    current: TypeOwner,
    types: HashMap<TypeId, OwnedType>,
    systems: Vec<OwnedSystem>,
    messages: Vec<OwnedMessage>,
    observers: Vec<OwnedObserver>,
}

impl TypeOwnership {
    /// The owner new registrations are recorded under.
    pub fn current(&self) -> TypeOwner {
        self.current
    }

    /// Sets the owner new registrations are recorded under; returns the previous one.
    ///
    /// ```
    /// use regia_ecs::owner::{TypeOwner, TypeOwnership};
    ///
    /// let mut ownership = TypeOwnership::default();
    /// assert_eq!(ownership.set_current(TypeOwner::Game(1)), TypeOwner::Engine);
    /// assert_eq!(ownership.current(), TypeOwner::Game(1));
    /// ```
    pub fn set_current(&mut self, owner: TypeOwner) -> TypeOwner {
        std::mem::replace(&mut self.current, owner)
    }

    /// Owner of a registered type, if it was registered through the ownership-aware APIs.
    pub fn type_owner(&self, type_id: TypeId) -> Option<TypeOwner> {
        self.types.get(&type_id).map(|t| t.owner)
    }

    /// Records that `owner` registered the type (first registration wins).
    ///
    /// ```
    /// use std::any::TypeId;
    /// use regia_ecs::owner::{TypeOwner, TypeOwnership};
    ///
    /// let mut ownership = TypeOwnership::default();
    /// let id = TypeId::of::<u32>();
    /// ownership.record_type(id, "u32", TypeOwner::Game(7));
    /// ownership.record_type(id, "u32", TypeOwner::Engine); // ignored: first wins
    /// assert_eq!(ownership.type_owner(id), Some(TypeOwner::Game(7)));
    /// assert_eq!(ownership.owned_count(TypeOwner::Game(7)), 1);
    /// ownership.forget(TypeOwner::Game(7));
    /// assert_eq!(ownership.owned_count(TypeOwner::Game(7)), 0);
    /// ```
    pub fn record_type(&mut self, type_id: TypeId, type_path: &'static str, owner: TypeOwner) {
        self.types
            .entry(type_id)
            .or_insert(OwnedType { owner, type_path });
    }

    /// Records a system added by a game owner.
    pub fn record_system(&mut self, system: OwnedSystem) {
        self.systems.push(system);
    }

    /// Records a message type registered by a game owner.
    pub fn record_message(&mut self, message: OwnedMessage) {
        self.messages.push(message);
    }

    /// Records an observer spawned by a game owner.
    pub fn record_observer(&mut self, observer: OwnedObserver) {
        self.observers.push(observer);
    }

    /// Types registered by `owner`, as `(TypeId, record)` pairs sorted by type path.
    pub fn types_of(&self, owner: TypeOwner) -> Vec<(TypeId, &OwnedType)> {
        let mut types: Vec<_> = self
            .types
            .iter()
            .filter(|(_, t)| t.owner == owner)
            .map(|(id, t)| (*id, t))
            .collect();
        types.sort_by_key(|(_, t)| t.type_path);
        types
    }

    /// Systems added by `owner`.
    pub fn systems_of(&self, owner: TypeOwner) -> impl Iterator<Item = &OwnedSystem> {
        self.systems.iter().filter(move |s| s.owner == owner)
    }

    /// Message types registered by `owner`.
    pub fn messages_of(&self, owner: TypeOwner) -> impl Iterator<Item = &OwnedMessage> {
        self.messages.iter().filter(move |m| m.owner == owner)
    }

    /// Observers spawned by `owner`.
    pub fn observers_of(&self, owner: TypeOwner) -> impl Iterator<Item = &OwnedObserver> {
        self.observers.iter().filter(move |o| o.owner == owner)
    }

    /// Number of live registrations (types, systems, messages, observers) owned by `owner`. Hot
    /// reload asserts this is zero after teardown.
    pub fn owned_count(&self, owner: TypeOwner) -> usize {
        self.types.values().filter(|t| t.owner == owner).count()
            + self.systems_of(owner).count()
            + self.messages_of(owner).count()
            + self.observers_of(owner).count()
    }

    /// Forgets every record of `owner` (called by the teardown once the registrations themselves are
    /// removed). If `owner` is the current owner, new registrations fall back to
    /// [`TypeOwner::Engine`] so nothing is stamped with a torn-down plugin id.
    pub fn forget(&mut self, owner: TypeOwner) {
        self.types.retain(|_, t| t.owner != owner);
        self.systems.retain(|s| s.owner != owner);
        self.messages.retain(|m| m.owner != owner);
        self.observers.retain(|o| o.owner != owner);
        if self.current == owner {
            self.current = TypeOwner::Engine;
        }
    }
}

/// Registers `T` (and the types it depends on) in the world's [`AppTypeRegistry`], recording every
/// type that was not registered before under the current owner of [`TypeOwnership`]. Inserts both
/// resources if missing.
///
/// A type that was already registered keeps its owner; one registered earlier without a record
/// (the registry's built-in primitives, direct registry writes) is recorded as
/// [`TypeOwner::Engine`], so a game re-registering `u32` never makes it game-owned. A dependency
/// first introduced by a game type (say `Vec<GameEnum>`) is game-owned and goes away with it.
pub fn register_type_owned<T: GetTypeRegistration>(world: &mut World) {
    let registry = world.get_resource_or_init::<AppTypeRegistry>().clone();
    let owner = world.get_resource_or_init::<TypeOwnership>().current();
    let mut registry = registry.write();
    if let Some(existing) = registry.get(TypeId::of::<T>()) {
        world.resource_mut::<TypeOwnership>().record_type(
            TypeId::of::<T>(),
            existing.type_info().type_path(),
            TypeOwner::Engine,
        );
        return;
    }
    let before: std::collections::HashSet<TypeId> = registry.iter().map(|r| r.type_id()).collect();
    registry.register::<T>();
    let mut ownership = world.resource_mut::<TypeOwnership>();
    for registration in registry.iter() {
        if !before.contains(&registration.type_id()) {
            ownership.record_type(
                registration.type_id(),
                registration.type_info().type_path(),
                owner,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Reflect, Default)]
    struct EngineThing(f32);

    #[derive(Reflect, Default)]
    struct GameThing {
        inner: GameInner,
        engine: EngineThing,
    }

    #[derive(Reflect, Default)]
    struct GameInner(u8);

    #[test]
    fn registrations_record_owner_including_new_dependencies() {
        let mut world = World::new();
        register_type_owned::<EngineThing>(&mut world);
        world
            .resource_mut::<TypeOwnership>()
            .set_current(TypeOwner::Game(7));
        register_type_owned::<GameThing>(&mut world);
        register_type_owned::<u32>(&mut world);
        register_type_owned::<EngineThing>(&mut world);
        let ownership = world.resource::<TypeOwnership>();
        assert_eq!(
            ownership.type_owner(TypeId::of::<u32>()),
            Some(TypeOwner::Engine),
            "built-in registrations stay engine-owned"
        );
        assert_eq!(
            ownership.type_owner(TypeId::of::<EngineThing>()),
            Some(TypeOwner::Engine)
        );
        assert_eq!(
            ownership.type_owner(TypeId::of::<GameThing>()),
            Some(TypeOwner::Game(7))
        );
        assert_eq!(
            ownership.type_owner(TypeId::of::<GameInner>()),
            Some(TypeOwner::Game(7)),
            "dependency first registered by the game is game-owned"
        );
        let game: Vec<_> = ownership
            .types_of(TypeOwner::Game(7))
            .into_iter()
            .map(|(_, t)| t.type_path)
            .collect();
        assert_eq!(game.len(), 2);
        assert_eq!(ownership.owned_count(TypeOwner::Game(7)), 2);
        assert!(
            world
                .resource::<AppTypeRegistry>()
                .read()
                .contains(TypeId::of::<GameInner>())
        );
        let mut ownership = world.resource_mut::<TypeOwnership>();
        ownership.forget(TypeOwner::Game(7));
        assert_eq!(ownership.owned_count(TypeOwner::Game(7)), 0);
    }

    #[test]
    fn forget_resets_current_owner() {
        let mut world = World::new();
        register_type_owned::<EngineThing>(&mut world);
        let mut ownership = world.resource_mut::<TypeOwnership>();
        ownership.set_current(TypeOwner::Game(3));
        ownership.forget(TypeOwner::Game(3));
        assert_eq!(ownership.current(), TypeOwner::Engine);
        ownership.set_current(TypeOwner::Game(4));
        ownership.forget(TypeOwner::Game(3));
        assert_eq!(ownership.current(), TypeOwner::Game(4));
    }
}
