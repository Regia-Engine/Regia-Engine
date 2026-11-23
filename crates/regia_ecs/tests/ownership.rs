// SPDX-License-Identifier: MPL-2.0
//! Ownership records: filtering per owner, counts, teardown and registration through the world.

use std::any::TypeId;

use regia_ecs::owner::{OwnedMessage, OwnedObserver, OwnedSystem, TypeOwnership, register_type_owned};
use regia_ecs::prelude::*;
use regia_ecs::reflect::AppTypeRegistry;

#[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
struct TestSchedule;

#[derive(Reflect, Default)]
struct Alpha(u8);

#[derive(Reflect, Default)]
struct Beta(u16);

struct Ping;

fn system_record(owner: TypeOwner, name: &str) -> OwnedSystem {
    OwnedSystem {
        owner,
        schedule: TestSchedule.intern(),
        key: Default::default(),
        name: name.into(),
    }
}

fn message_record(owner: TypeOwner) -> OwnedMessage {
    OwnedMessage {
        owner,
        type_id: TypeId::of::<Ping>(),
        type_name: "Ping",
    }
}

fn observer_record(owner: TypeOwner, entity: Entity) -> OwnedObserver {
    OwnedObserver { owner, entity }
}

#[test]
fn owner_defaults_and_ordering() {
    assert_eq!(TypeOwner::default(), TypeOwner::Engine);
    assert!(TypeOwner::Engine < TypeOwner::Game(0));
    assert!(TypeOwner::Game(1) < TypeOwner::Game(2));
    assert_ne!(OwnerSet(TypeOwner::Game(1)), OwnerSet(TypeOwner::Game(2)));
}

#[test]
fn set_current_returns_the_previous_owner() {
    let mut ownership = TypeOwnership::default();
    assert_eq!(ownership.current(), TypeOwner::Engine);
    assert_eq!(ownership.set_current(TypeOwner::Game(3)), TypeOwner::Engine);
    assert_eq!(ownership.current(), TypeOwner::Game(3));
    assert_eq!(ownership.set_current(TypeOwner::Game(4)), TypeOwner::Game(3));
}

#[test]
fn records_are_filtered_per_owner_and_counted() {
    let mut world = World::new();
    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let one = TypeOwner::Game(1);
    let two = TypeOwner::Game(2);
    let mut ownership = TypeOwnership::default();
    ownership.record_type(TypeId::of::<Alpha>(), "t::Alpha", one);
    ownership.record_type(TypeId::of::<Beta>(), "t::Beta", two);
    ownership.record_system(system_record(one, "first"));
    ownership.record_system(system_record(one, "second"));
    ownership.record_system(system_record(two, "third"));
    ownership.record_message(message_record(one));
    ownership.record_observer(observer_record(one, a));
    ownership.record_observer(observer_record(two, b));

    let names: Vec<_> = ownership.systems_of(one).map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["first", "second"], "registration order is kept");
    assert_eq!(ownership.systems_of(two).count(), 1);
    assert_eq!(ownership.messages_of(one).count(), 1);
    assert_eq!(ownership.messages_of(two).count(), 0);
    assert_eq!(
        ownership.observers_of(one).map(|o| o.entity).collect::<Vec<_>>(),
        [a]
    );
    assert_eq!(
        ownership.observers_of(two).map(|o| o.entity).collect::<Vec<_>>(),
        [b]
    );
    // Types + systems + messages + observers.
    assert_eq!(ownership.owned_count(one), 1 + 2 + 1 + 1);
    // Type + system + observer; `two` has no message.
    assert_eq!(ownership.owned_count(two), 1 + 1 + 1);
    assert_eq!(ownership.owned_count(TypeOwner::Engine), 0);
}

#[test]
fn forget_removes_only_that_owners_records() {
    let mut world = World::new();
    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let one = TypeOwner::Game(1);
    let two = TypeOwner::Game(2);
    let mut ownership = TypeOwnership::default();
    ownership.record_type(TypeId::of::<Alpha>(), "t::Alpha", one);
    ownership.record_type(TypeId::of::<Beta>(), "t::Beta", two);
    ownership.record_system(system_record(one, "s1"));
    ownership.record_system(system_record(two, "s2"));
    ownership.record_message(message_record(one));
    ownership.record_message(message_record(two));
    ownership.record_observer(observer_record(one, a));
    ownership.record_observer(observer_record(two, b));
    let before_two = ownership.owned_count(two);

    ownership.set_current(one);
    ownership.forget(one);
    assert_eq!(
        ownership.current(),
        TypeOwner::Engine,
        "forgetting the current owner resets it"
    );
    assert_eq!(ownership.owned_count(one), 0);
    assert_eq!(ownership.type_owner(TypeId::of::<Alpha>()), None);
    assert_eq!(ownership.owned_count(two), before_two, "other owners untouched");
    assert_eq!(ownership.type_owner(TypeId::of::<Beta>()), Some(two));

    ownership.forget(one);
    assert_eq!(ownership.owned_count(one), 0, "forgetting twice is harmless");
    ownership.forget(TypeOwner::Game(99));
    assert_eq!(
        ownership.owned_count(two),
        before_two,
        "forgetting an unknown owner is harmless"
    );
}

#[test]
fn first_recorded_type_owner_wins() {
    let mut ownership = TypeOwnership::default();
    ownership.record_type(TypeId::of::<Alpha>(), "t::Alpha", TypeOwner::Game(1));
    ownership.record_type(TypeId::of::<Alpha>(), "t::Alpha", TypeOwner::Game(2));
    ownership.record_type(TypeId::of::<Alpha>(), "t::Alpha", TypeOwner::Engine);
    assert_eq!(
        ownership.type_owner(TypeId::of::<Alpha>()),
        Some(TypeOwner::Game(1))
    );
    assert_eq!(ownership.owned_count(TypeOwner::Game(2)), 0);
}

#[test]
fn types_of_is_sorted_by_type_path() {
    let one = TypeOwner::Game(1);
    let mut ownership = TypeOwnership::default();
    ownership.record_type(TypeId::of::<Beta>(), "t::Beta", one);
    ownership.record_type(TypeId::of::<Alpha>(), "t::Alpha", one);
    ownership.record_type(TypeId::of::<u8>(), "u8", TypeOwner::Engine);
    let paths: Vec<_> = ownership
        .types_of(one)
        .into_iter()
        .map(|(_, t)| t.type_path)
        .collect();
    assert_eq!(paths, ["t::Alpha", "t::Beta"]);
    assert!(ownership.types_of(TypeOwner::Game(5)).is_empty());
}

#[test]
fn register_type_owned_creates_both_resources() {
    let mut world = World::new();
    assert!(world.get_resource::<TypeOwnership>().is_none());
    assert!(world.get_resource::<AppTypeRegistry>().is_none());
    register_type_owned::<Alpha>(&mut world);
    assert!(world.get_resource::<AppTypeRegistry>().is_some());
    let ownership = world.resource::<TypeOwnership>();
    assert_eq!(
        ownership.type_owner(TypeId::of::<Alpha>()),
        Some(TypeOwner::Engine)
    );
}

#[test]
fn re_registering_keeps_the_first_owner() {
    let mut world = World::new();
    register_type_owned::<Alpha>(&mut world);
    world
        .resource_mut::<TypeOwnership>()
        .set_current(TypeOwner::Game(2));
    register_type_owned::<Alpha>(&mut world);
    register_type_owned::<Beta>(&mut world);
    let ownership = world.resource::<TypeOwnership>();
    assert_eq!(
        ownership.type_owner(TypeId::of::<Alpha>()),
        Some(TypeOwner::Engine)
    );
    assert_eq!(
        ownership.type_owner(TypeId::of::<Beta>()),
        Some(TypeOwner::Game(2))
    );
    assert!(
        world
            .resource::<AppTypeRegistry>()
            .read()
            .contains(TypeId::of::<Beta>())
    );
}

#[test]
fn teardown_of_one_game_leaves_the_other_owned() {
    let mut world = World::new();
    world
        .get_resource_or_init::<TypeOwnership>()
        .set_current(TypeOwner::Game(1));
    register_type_owned::<Alpha>(&mut world);
    world
        .resource_mut::<TypeOwnership>()
        .set_current(TypeOwner::Game(2));
    register_type_owned::<Beta>(&mut world);
    let mut ownership = world.resource_mut::<TypeOwnership>();
    assert_eq!(ownership.owned_count(TypeOwner::Game(1)), 1);
    assert_eq!(ownership.owned_count(TypeOwner::Game(2)), 1);
    ownership.forget(TypeOwner::Game(1));
    assert_eq!(ownership.owned_count(TypeOwner::Game(1)), 0);
    assert_eq!(ownership.type_owner(TypeId::of::<Alpha>()), None);
    assert_eq!(
        ownership.type_owner(TypeId::of::<Beta>()),
        Some(TypeOwner::Game(2))
    );
}
