// SPDX-License-Identifier: MPL-2.0
//! Tests written from the mutation run of `regia_ecs` (ADR-3389): each one pins a behavior a mutant broke
//! while the older tests still passed.

use regia_ecs::attrs::EditorAttrs;
use regia_ecs::describe::describe_type;
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Reflect, Serialize, Deserialize, Default)]
#[reflect(Serialize, Deserialize, Default)]
struct BothWays(u32);

#[derive(Reflect, Serialize, Default)]
#[reflect(Serialize, Default)]
struct WriteOnly(u32);

#[derive(Reflect, Deserialize, Default)]
#[reflect(Deserialize, Default)]
struct ReadOnlyData(u32);

#[test]
fn the_serde_flag_needs_both_directions() {
    let mut registry = TypeRegistry::default();
    registry.register::<BothWays>();
    registry.register::<WriteOnly>();
    registry.register::<ReadOnlyData>();
    let serde = |name: &str| describe_type(&registry, name).expect("registered").data.serde;
    assert!(serde("BothWays"));
    assert!(
        !serde("WriteOnly"),
        "serialize-only is not its own serde in both directions"
    );
    assert!(
        !serde("ReadOnlyData"),
        "deserialize-only is not its own serde in both directions"
    );
}

#[test]
fn editor_attrs_are_empty_only_with_nothing_set() {
    assert!(EditorAttrs::default().is_empty());
    let set = [
        EditorAttrs {
            hidden: true,
            ..Default::default()
        },
        EditorAttrs {
            read_only: true,
            ..Default::default()
        },
        EditorAttrs {
            step: Some(0.5),
            ..Default::default()
        },
        EditorAttrs {
            tooltip: Some("tip"),
            ..Default::default()
        },
        EditorAttrs {
            range: Some(0.0..=1.0),
            ..Default::default()
        },
    ];
    for attrs in set {
        assert!(!attrs.is_empty(), "{attrs:?} has an attribute set");
    }
}

#[derive(Reflect, Debug, PartialEq)]
struct Source(u32);

#[derive(Reflect, Debug, PartialEq)]
struct Target(u32);

/// Registers neither `FromReflect` nor `Default`: only a value that already is one can become one.
#[derive(Reflect, Debug, PartialEq)]
#[reflect(from_reflect = false)]
struct Opaque(u32);

#[test]
fn into_concrete_converts_a_value_of_another_type_through_from_reflect() {
    use regia_ecs::serialize::into_concrete;
    let mut registry = TypeRegistry::default();
    registry.register::<Target>();
    let registration = registry
        .get(std::any::TypeId::of::<Target>())
        .expect("registered");
    let out = into_concrete(Box::new(Source(7)), registration).expect("a same-shaped tuple struct converts");
    assert_eq!(
        out.downcast_ref::<Target>(),
        Some(&Target(7)),
        "the result is the registered type"
    );
}

#[test]
fn into_concrete_keeps_a_value_that_already_is_the_registered_type() {
    use regia_ecs::reflect::TypeRegistration;
    use regia_ecs::serialize::into_concrete;
    let registration = TypeRegistration::of::<Opaque>();
    let out = into_concrete(Box::new(Opaque(3)), &registration).expect("the value is already concrete");
    assert_eq!(out.downcast_ref::<Opaque>(), Some(&Opaque(3)));
}

#[derive(Component)]
struct Marker;

#[test]
fn spawn_order_is_empty_until_a_tracked_component_arrives() {
    use regia_ecs::spawn_order::{SpawnOrder, track_spawn_order};
    let mut world = World::new();
    assert!(track_spawn_order::<Marker>(&mut world));
    assert!(world.resource::<SpawnOrder<Marker>>().is_empty());
    let e = world.spawn(Marker).id();
    let order = world.resource::<SpawnOrder<Marker>>();
    assert!(!order.is_empty());
    assert_eq!(order.len(), 1);
    assert_eq!(order.get(e), Some(0));
    world.despawn(e);
    assert!(world.resource::<SpawnOrder<Marker>>().is_empty());
}

#[derive(serde::Serialize)]
struct Layout {
    name: &'static str,
    pair: (u8, u8),
    list: Vec<u8>,
}

#[test]
fn canonical_layout_is_pinned() {
    let value = Layout {
        name: "a\tb",
        pair: (1, 2),
        list: vec![3, 4],
    };
    let text = ron::ser::to_string_pretty(&value, regia_ecs::serialize::canonical_pretty_config())
        .expect("serializes");
    assert_eq!(
        text,
        "(\n    name: \"a\\tb\",\n    pair: (1, 2),\n    list: [\n        3,\n        4,\n    ],\n)"
    );
}
