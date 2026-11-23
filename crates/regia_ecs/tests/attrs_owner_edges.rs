// SPDX-License-Identifier: MPL-2.0
//! Boundary values of the editor attributes and re-registration after teardown of the ownership
//! records, through the public surface only.

use std::any::TypeId;

use regia_ecs::attrs::{Range, Step};
use regia_ecs::describe::field_attrs;
use regia_ecs::owner::{TypeOwner, TypeOwnership, register_type_owned};
use regia_ecs::prelude::*;

#[derive(Reflect, Default)]
#[reflect(Default)]
struct Bounds {
    #[reflect(@Range(2.0..=2.0))]
    degenerate: f32,
    #[reflect(@Range(-0.0..=0.0), @Step(f64::MIN_POSITIVE))]
    signed_zero: f32,
    #[reflect(@Range(f64::NEG_INFINITY..=0.0))]
    open_below: f32,
    #[reflect(@Range(f64::MIN..=f64::MAX), @Step(f64::MAX))]
    widest: f32,
    #[reflect(@Step(f64::INFINITY))]
    infinite_step: f32,
    #[reflect(@Step(-0.0))]
    negative_zero_step: f32,
}

#[test]
fn range_and_step_boundaries_are_classified_exactly() {
    let attrs = |name: &str| field_attrs::<Bounds>(name).unwrap_or_default();
    assert_eq!(
        attrs("degenerate").range,
        Some(2.0..=2.0),
        "start == end is a valid range"
    );
    let zero = attrs("signed_zero");
    assert_eq!(zero.range, Some(-0.0..=0.0));
    assert_eq!(
        zero.step,
        Some(f64::MIN_POSITIVE),
        "the smallest positive step is kept"
    );
    assert_eq!(attrs("open_below").range, None, "an infinite endpoint is dropped");
    let widest = attrs("widest");
    assert_eq!(widest.range, Some(f64::MIN..=f64::MAX));
    assert_eq!(widest.step, Some(f64::MAX));
    assert_eq!(attrs("infinite_step").step, None);
    assert_eq!(
        attrs("negative_zero_step").step,
        None,
        "-0.0 is not greater than zero"
    );
}

#[derive(Reflect, Default)]
struct GameOnly(u8);

#[test]
fn types_can_be_registered_again_under_a_new_owner_after_teardown() {
    let mut world = World::new();
    let id = TypeId::of::<GameOnly>();
    world
        .get_resource_or_init::<TypeOwnership>()
        .set_current(TypeOwner::Game(1));
    register_type_owned::<GameOnly>(&mut world);
    assert_eq!(
        world.resource::<TypeOwnership>().type_owner(id),
        Some(TypeOwner::Game(1))
    );

    // Teardown forgets the records but the registry still holds the type: a later registration
    // finds it registered and files it under the engine, never under a stale plugin id.
    world.resource_mut::<TypeOwnership>().forget(TypeOwner::Game(1));
    assert_eq!(world.resource::<TypeOwnership>().type_owner(id), None);
    world
        .resource_mut::<TypeOwnership>()
        .set_current(TypeOwner::Game(2));
    register_type_owned::<GameOnly>(&mut world);
    let ownership = world.resource::<TypeOwnership>();
    assert_eq!(ownership.type_owner(id), Some(TypeOwner::Engine));
    assert_eq!(ownership.owned_count(TypeOwner::Game(1)), 0);
    assert_eq!(ownership.owned_count(TypeOwner::Game(2)), 0);
}

#[test]
fn forgetting_the_engine_owner_keeps_it_current_and_is_idempotent() {
    let mut ownership = TypeOwnership::default();
    ownership.record_type(TypeId::of::<u8>(), "u8", TypeOwner::Engine);
    ownership.forget(TypeOwner::Engine);
    ownership.forget(TypeOwner::Engine);
    assert_eq!(ownership.current(), TypeOwner::Engine);
    assert_eq!(ownership.owned_count(TypeOwner::Engine), 0);
    assert!(ownership.types_of(TypeOwner::Engine).is_empty());
}
