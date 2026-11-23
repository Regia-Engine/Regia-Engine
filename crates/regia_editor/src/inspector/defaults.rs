// SPDX-License-Identifier: MPL-2.0
//! The value an inspector field is compared to (spec addendum § 7.1, P-116): the prefab's value
//! for a field of a prefab instance, otherwise the component type's reflected `Default`.
//!
//! One helper for every "is this field changed?" question, so the Inspector's **Modified only**
//! filter, the per-field reset marker and `editor.inspect_selection { modified_only }` agree.
//! [`baseline`] finds the value to compare against; [`differs`] compares one field;
//! [`modified_paths`] lists every changed leaf of a component.

use std::collections::BTreeSet;

use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_ecs::reflect::std_traits::ReflectDefault;
use regia_ecs::reflect::{AppTypeRegistry, PartialReflect, ReflectPath, ReflectRef};
use regia_scene::prefab::{BakedValue, PrefabInstance, PrefabLibrary, PrefabLink};

/// How deep [`modified_paths`] walks into nested structs (the mixed-value walk's bound).
const MAX_DEPTH: usize = 8;

/// Where a [`baseline`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineSource {
    /// The prefab template's value of this entity's component (a prefab instance).
    Prefab,
    /// The component type's reflected `Default`.
    TypeDefault,
}

/// The value `entity`'s component `component` (a type path, or a short name) is compared to: the
/// prefab template's component when `entity` is inside a prefab instance whose template is loaded
/// and has it, otherwise the type's reflected `Default`. `None` when neither exists (a type without
/// `#[reflect(Default)]` that is not from a prefab).
#[must_use]
pub fn baseline(
    world: &World,
    entity: Entity,
    component: &str,
) -> Option<(BaselineSource, Box<dyn PartialReflect>)> {
    let registry = world.get_resource::<AppTypeRegistry>()?.clone();
    let registry = registry.read();
    let registration = registry
        .get_with_type_path(component)
        .or_else(|| registry.get_with_short_type_path(component))?;
    let full = registration.type_info().type_path();
    let short = registration.type_info().type_path_table().short_path();
    if let Some(value) = prefab_value(world, entity, full, short) {
        return Some((BaselineSource::Prefab, value));
    }
    let default = registration.data::<ReflectDefault>()?.default();
    Some((BaselineSource::TypeDefault, default.into_partial_reflect()))
}

/// The prefab template's value of `entity`'s component, when `entity` is part of an instance.
fn prefab_value(world: &World, entity: Entity, full: &str, short: &str) -> Option<Box<dyn PartialReflect>> {
    let link = world.get::<PrefabLink>(entity)?;
    let root = instance_root(world, entity)?;
    let instance = world.get::<PrefabInstance>(root)?;
    let library = world.get_resource::<PrefabLibrary>()?;
    let template = library.get(regia_core::id::Guid::from_u128(instance.prefab))?;
    let entry = template
        .entities
        .iter()
        .find(|candidate| candidate.local_id.as_u128() == link.local_id)?;
    let (_, baked) = entry
        .components
        .iter()
        .find(|(type_path, _)| type_path == full || type_path == short)?;
    let BakedValue::Reflected(value) = baked else {
        return None;
    };
    super::reflect_ui::clone_value(value.as_partial_reflect())
}

/// The prefab instance root above `entity` (the nearest ancestor with [`PrefabInstance`], or
/// `entity` itself when it is the root).
fn instance_root(world: &World, entity: Entity) -> Option<Entity> {
    let mut current = entity;
    for _ in 0..64 {
        if world.get::<PrefabInstance>(current).is_some() {
            return Some(current);
        }
        current = world.get::<ChildOf>(current)?.parent();
    }
    None
}

/// Whether the field at reflect `path` of `value` differs from the same field of `baseline`
/// (`""` compares the whole value). A field the baseline lacks, or one reflection cannot compare,
/// counts as changed.
#[must_use]
pub fn differs(value: &dyn PartialReflect, baseline: &dyn PartialReflect, path: &str) -> bool {
    let (left, right) = if path.is_empty() {
        (Some(value), Some(baseline))
    } else {
        (
            path.reflect_element(value).ok(),
            path.reflect_element(baseline).ok(),
        )
    };
    match (left, right) {
        (Some(left), Some(right)) => left.reflect_partial_eq(right) != Some(true),
        _ => true,
    }
}

/// Every changed leaf of `value` against `baseline`, as reflect paths (`intensity`,
/// `translation.x`, `settings.0`). Struct and tuple-struct fields are walked; any other value
/// (a list, a map, an enum, a number) is one leaf. Equal values give an empty set.
#[must_use]
pub fn modified_paths(value: &dyn PartialReflect, baseline: &dyn PartialReflect) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk(value, Some(baseline), "", 0, &mut out);
    out
}

fn join(path: &str, segment: &str) -> String {
    if path.is_empty() {
        segment.to_owned()
    } else {
        format!("{path}.{segment}")
    }
}

fn walk(
    value: &dyn PartialReflect,
    baseline: Option<&dyn PartialReflect>,
    path: &str,
    depth: usize,
    out: &mut BTreeSet<String>,
) {
    let Some(baseline) = baseline else {
        if !path.is_empty() {
            out.insert(path.to_owned());
        }
        return;
    };
    if depth < MAX_DEPTH {
        match (value.reflect_ref(), baseline.reflect_ref()) {
            (ReflectRef::Struct(left), ReflectRef::Struct(right)) => {
                for index in 0..left.field_len() {
                    let (Some(name), Some(field)) = (left.name_at(index), left.field_at(index)) else {
                        continue;
                    };
                    walk(field, right.field(name), &join(path, name), depth + 1, out);
                }
                return;
            }
            (ReflectRef::TupleStruct(left), ReflectRef::TupleStruct(right)) => {
                for index in 0..left.field_len() {
                    let Some(field) = left.field(index) else {
                        continue;
                    };
                    walk(
                        field,
                        right.field(index),
                        &join(path, &index.to_string()),
                        depth + 1,
                        out,
                    );
                }
                return;
            }
            _ => {}
        }
    }
    if value.reflect_partial_eq(baseline) != Some(true) {
        out.insert(path.to_owned());
    }
}

/// Every leaf of `value` as reflect paths, walked as [`modified_paths`] walks: what counts as
/// changed when a component has no baseline (no prefab template and no reflected `Default`), as
/// [`differs`] treats a missing baseline (review finding 20 of ADR-7758: Modified only must not
/// hide such a component).
#[must_use]
pub fn all_leaf_paths(value: &dyn PartialReflect) -> BTreeSet<String> {
    fn leaves(value: &dyn PartialReflect, path: &str, depth: usize, out: &mut BTreeSet<String>) {
        if depth < MAX_DEPTH {
            match value.reflect_ref() {
                ReflectRef::Struct(s) => {
                    for index in 0..s.field_len() {
                        if let (Some(name), Some(field)) = (s.name_at(index), s.field_at(index)) {
                            leaves(field, &join(path, name), depth + 1, out);
                        }
                    }
                    return;
                }
                ReflectRef::TupleStruct(t) => {
                    for index in 0..t.field_len() {
                        if let Some(field) = t.field(index) {
                            leaves(field, &join(path, &index.to_string()), depth + 1, out);
                        }
                    }
                    return;
                }
                _ => {}
            }
        }
        out.insert(path.to_owned());
    }
    let mut out = BTreeSet::new();
    leaves(value, "", 0, &mut out);
    out
}

/// [`modified_paths`] of `entity`'s live component against its [`baseline`] (every leaf,
/// [`all_leaf_paths`], when there is no baseline); `None` when the entity lacks the component.
#[must_use]
pub fn modified_fields(world: &World, entity: Entity, component: &str) -> Option<BTreeSet<String>> {
    let base = baseline(world, entity, component).map(|(_, base)| base);
    let registry = world.get_resource::<AppTypeRegistry>()?.clone();
    let registry = registry.read();
    let registration = registry
        .get_with_type_path(component)
        .or_else(|| registry.get_with_short_type_path(component))?;
    let reflect = registration.data::<regia_ecs::reflect::ReflectComponent>()?;
    let live = reflect.reflect(world.get_entity(entity).ok()?)?;
    Some(match base {
        Some(base) => modified_paths(live.as_partial_reflect(), base.as_ref()),
        None => all_leaf_paths(live.as_partial_reflect()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Component, Reflect, Clone, Debug, PartialEq)]
    #[reflect(Component, Default)]
    struct Lamp {
        intensity: f32,
        color: [f32; 3],
        inner: Inner,
    }

    #[derive(Reflect, Clone, Debug, Default, PartialEq)]
    struct Inner {
        a: u32,
        b: bool,
    }

    impl Default for Lamp {
        fn default() -> Self {
            Self {
                intensity: 800.0,
                color: [1.0, 1.0, 1.0],
                inner: Inner::default(),
            }
        }
    }

    #[test]
    fn equal_values_have_no_modified_paths() {
        let lamp = Lamp::default();
        assert!(modified_paths(&lamp, &Lamp::default()).is_empty());
        assert!(!differs(&lamp, &Lamp::default(), "intensity"));
        assert!(!differs(&lamp, &Lamp::default(), ""));
    }

    #[test]
    fn changed_leaves_are_listed_by_path() {
        let lamp = Lamp {
            intensity: 5.0,
            inner: Inner { a: 0, b: true },
            ..Lamp::default()
        };
        let paths: Vec<String> = modified_paths(&lamp, &Lamp::default()).into_iter().collect();
        assert_eq!(paths, ["inner.b", "intensity"]);
        assert!(differs(&lamp, &Lamp::default(), "intensity"));
        assert!(differs(&lamp, &Lamp::default(), "inner"));
        assert!(!differs(&lamp, &Lamp::default(), "inner.a"));
        assert!(!differs(&lamp, &Lamp::default(), "color"));
    }

    #[test]
    fn the_type_default_is_the_baseline_outside_a_prefab() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.register_type::<Lamp>();
        let entity = app
            .world_mut()
            .spawn(Lamp {
                intensity: 1.0,
                ..Lamp::default()
            })
            .id();
        let (source, _) = baseline(app.world(), entity, "Lamp").expect("baseline");
        assert_eq!(source, BaselineSource::TypeDefault);
        let fields = modified_fields(app.world(), entity, "Lamp").expect("fields");
        assert_eq!(fields.into_iter().collect::<Vec<_>>(), ["intensity"]);
        // With no baseline every leaf counts as changed (review finding 20 of ADR-7758).
        let leaves = all_leaf_paths(&Lamp::default());
        assert!(
            leaves.contains("intensity") && leaves.contains("color"),
            "{leaves:?}"
        );
        assert!(leaves.iter().any(|p| p.starts_with("inner.")), "{leaves:?}");
    }
}
