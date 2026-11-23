// SPDX-License-Identifier: MPL-2.0
//! The unique-name marker: an entity whose [`Name`] is unique in its world, so game code can find
//! it by name wherever it moves in the hierarchy (`docs/specs/19-editor.md` § 2, ADR-7772).
//!
//! [`UniqueName`] is a saved, field-less component (additive to the level format: an older level
//! simply has none, and an older engine skips the unknown type with a warning). The editor refuses
//! a second entity with the same name and the marker (rename, duplicate); at runtime
//! [`find_unique`] looks the entity up by name.

use regia_ecs::prelude::*;

use crate::components::Name;

/// Marks an entity whose [`Name`] is unique among the marked entities of its world.
///
/// Saved with the level like any reflected component. The editor keeps the rule (a rename to a
/// taken name is refused with `unique_name_taken`; a duplicate gets the next free name); game code
/// finds the entity with [`find_unique`].
#[derive(Component, Reflect, Clone, Copy, Default, Debug, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
pub struct UniqueName;

/// The marked entity named `name` (exact, case-sensitive), wherever it sits in the hierarchy.
///
/// Walks the world's entities once (O(entities)); cache the result when calling it every frame.
/// When a broken level holds two marked entities with the same name (an edit outside the editor),
/// the older one (lower generation, then lower index) is returned.
///
/// # Example
///
/// ```
/// use regia_ecs::prelude::*;
/// use regia_scene::components::Name;
/// use regia_scene::unique_name::{UniqueName, find_unique};
///
/// let mut world = World::new();
/// let door = world.spawn((Name::new("MainDoor"), UniqueName)).id();
/// world.spawn(Name::new("MainDoor")); // not marked: not a match
/// assert_eq!(find_unique(&world, "MainDoor"), Some(door));
/// assert_eq!(find_unique(&world, "maindoor"), None);
/// ```
#[must_use]
pub fn find_unique(world: &World, name: &str) -> Option<Entity> {
    world
        .iter_entities()
        .filter(|entity| entity.contains::<UniqueName>())
        .filter(|entity| entity.get::<Name>().is_some_and(|n| n.as_str() == name))
        .map(|entity| entity.id())
        .min_by_key(|entity| (entity.generation().to_bits(), entity.index_u32()))
}

/// Another marked entity (not `except`) already named `name`, if any: the rule the editor checks
/// before a rename or a marker toggle.
#[must_use]
pub fn unique_conflict(world: &World, name: &str, except: Option<Entity>) -> Option<Entity> {
    world
        .iter_entities()
        .filter(|entity| Some(entity.id()) != except)
        .filter(|entity| entity.contains::<UniqueName>())
        .find(|entity| entity.get::<Name>().is_some_and(|n| n.as_str() == name))
        .map(|entity| entity.id())
}

/// The first of `base`, `base 2`, `base 3`, … that no marked entity other than `except` uses (a
/// trailing ` <number>` on `base` is replaced, so `Door 2` continues at `Door 3`).
#[must_use]
pub fn next_free_name(world: &World, base: &str, except: Option<Entity>) -> String {
    if unique_conflict(world, base, except).is_none() {
        return base.to_string();
    }
    let (stem, start) = match base.rsplit_once(' ') {
        Some((stem, n)) if !stem.is_empty() && n.parse::<u32>().is_ok() => {
            (stem, n.parse::<u32>().unwrap_or(1).saturating_add(1))
        }
        _ => (base, 2),
    };
    let mut n = start.max(2);
    loop {
        let candidate = format!("{stem} {n}");
        if unique_conflict(world, &candidate, except).is_none() {
            return candidate;
        }
        n = n.saturating_add(1);
        if n == u32::MAX {
            return candidate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_unique_follows_the_entity_through_reparenting() {
        let mut world = World::new();
        let room = world.spawn(Name::new("Room")).id();
        let key = world.spawn((Name::new("GoldKey"), UniqueName)).id();
        assert_eq!(find_unique(&world, "GoldKey"), Some(key));
        // Moving it under another parent changes nothing about the lookup.
        world.entity_mut(key).insert(ChildOf(room));
        assert_eq!(find_unique(&world, "GoldKey"), Some(key));
        // An unmarked entity of the same name is never returned.
        world.entity_mut(key).remove::<UniqueName>();
        world.spawn(Name::new("GoldKey"));
        assert_eq!(find_unique(&world, "GoldKey"), None);
    }

    #[test]
    fn conflicts_and_next_free_names() {
        let mut world = World::new();
        let a = world.spawn((Name::new("Door"), UniqueName)).id();
        let b = world.spawn((Name::new("Door 2"), UniqueName)).id();
        assert_eq!(unique_conflict(&world, "Door", None), Some(a));
        assert_eq!(unique_conflict(&world, "Door", Some(a)), None);
        assert_eq!(unique_conflict(&world, "Gate", None), None);
        assert_eq!(next_free_name(&world, "Door", None), "Door 3");
        assert_eq!(next_free_name(&world, "Door 2", Some(b)), "Door 2");
        assert_eq!(next_free_name(&world, "Gate", None), "Gate");
    }
}
