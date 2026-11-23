// SPDX-License-Identifier: MPL-2.0
//! Error paths of `apply_override`: a missing target, malformed paths, wrong value types.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::Transform;
use regia_scene::components::Name;
use regia_scene::prefab::apply_override;

const TRANSFORM: &str = "regia_scene::transform::Transform";

fn world_with_entity() -> (World, Entity) {
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        regia_scene::components::register_primitives(&mut registry);
        registry.register::<Transform>();
        registry.register::<Name>();
    }
    let entity = world
        .spawn(Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)))
        .id();
    (world, entity)
}

fn apply(world: &mut World, target: Entity, component: &str, path: &str, value: &str) -> Result<(), String> {
    let handle = world.resource::<AppTypeRegistry>().0.clone();
    let registry = handle.read();
    apply_override(world, target, component, path, value, &registry)
}

const WHOLE: &str = "(translation: (9.0, 9.0, 9.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0))";

/// A whole-component override aimed at an entity that is gone is an error, like a field override.
#[test]
fn overriding_a_despawned_target_is_an_error_for_both_forms() {
    let (mut world, entity) = world_with_entity();
    world.despawn(entity);
    assert!(
        apply(&mut world, entity, TRANSFORM, "", WHOLE).is_err(),
        "whole component"
    );
    assert!(
        apply(&mut world, entity, TRANSFORM, ".translation", "(1.0, 1.0, 1.0)").is_err(),
        "field patch"
    );
}

/// `""` and `"."` replace the whole component; `".."` is not a spelling of either and changes nothing.
#[test]
fn only_empty_and_single_dot_mean_the_whole_component() {
    let (mut world, entity) = world_with_entity();
    assert!(apply(&mut world, entity, TRANSFORM, "..", WHOLE).is_err());
    assert_eq!(
        world.get::<Transform>(entity).map(|t| t.translation),
        Some(Vec3::new(1.0, 2.0, 3.0))
    );
    apply(&mut world, entity, TRANSFORM, ".", WHOLE).expect("dot");
    assert_eq!(
        world.get::<Transform>(entity).map(|t| t.translation),
        Some(Vec3::splat(9.0))
    );
    apply(
        &mut world,
        entity,
        TRANSFORM,
        "",
        &WHOLE.replace("9.0, 9.0, 9.0", "4.0, 4.0, 4.0"),
    )
    .expect("empty");
    assert_eq!(
        world.get::<Transform>(entity).map(|t| t.translation),
        Some(Vec3::splat(4.0))
    );
}

/// Bad paths, bad values and unknown or missing components are errors and leave the value alone.
#[test]
fn bad_overrides_change_nothing() {
    let (mut world, entity) = world_with_entity();
    let before = world.get::<Transform>(entity).copied();
    for (component, path, value) in [
        (TRANSFORM, ".nope", "1.0"),
        (TRANSFORM, ".translation.w", "1.0"),
        (TRANSFORM, ".translation[7]", "1.0"),
        (TRANSFORM, ".translation", "\"text\""),
        (TRANSFORM, ".translation", ""),
        (TRANSFORM, "", "(translation:"),
        ("regia_scene::nothing::Here", "", "()"),
        ("regia_scene::components::Name", ".0", "\"x\""),
    ] {
        let result = apply(&mut world, entity, component, path, value);
        assert!(result.is_err(), "{component} {path:?} {value:?} -> {result:?}");
    }
    assert_eq!(world.get::<Transform>(entity).copied(), before);
    // A field patch on a component the entity does not carry is an error naming the component.
    let err = apply(&mut world, entity, "regia_scene::components::Name", ".0", "\"x\"").expect_err("no Name");
    assert!(err.contains("Name"), "{err}");
}

/// A valid field patch touches only that field.
#[test]
fn a_field_patch_changes_only_that_field() {
    let (mut world, entity) = world_with_entity();
    apply(&mut world, entity, TRANSFORM, ".translation.y", "50.0").expect("patch");
    let t = world.get::<Transform>(entity).copied().expect("transform");
    assert_eq!(t.translation, Vec3::new(1.0, 50.0, 3.0));
    assert_eq!(t.scale, Vec3::ONE);
    // Leading dot optional.
    apply(&mut world, entity, TRANSFORM, "translation.x", "-5.0").expect("patch");
    assert_eq!(
        world.get::<Transform>(entity).map(|t| t.translation.x),
        Some(-5.0)
    );
}
