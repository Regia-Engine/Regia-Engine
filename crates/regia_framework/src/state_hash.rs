// SPDX-License-Identifier: MPL-2.0
//! World state hashing for determinism checks (`docs/06-testing-and-ci.md` § 6, ADR-537,
//! `book/agents/determinism.md`).
//!
//! [`world_snapshot`] is the world's observable simulation state: every reflected component of
//! every entity as canonical text, keyed `"<entity>/<Component>"`. Entities are keyed by their
//! `Name` (entity ids are not part of the state), and entity references inside values print as
//! those keys. [`snapshot_hash`] folds a snapshot into one FNV-1a value, and [`differing_keys`]
//! names the components two snapshots disagree on.
//!
//! # Canonical text
//!
//! The text is written from the reflected shape, not from `Debug`: struct fields in declaration
//! order, lists and arrays in order, and **map and set entries sorted by their own canonical
//! text**. A component holding a `HashMap` therefore hashes the same however the map happens to
//! iterate, so the hash measures the state and never the hasher's random seed. Floats print with
//! Rust's shortest round-trip form, which is distinct for distinct values (`0.0` and `-0.0`
//! included); every NaN prints as `NaN`. An opaque type without a reflected `Debug` prints as its
//! type path only: its contents are outside the hash.
//!
//! Resources are not included. A scene that keeps simulation state in a resource (a score, an RNG)
//! adds it to the snapshot itself with [`insert_resource_text`].

use std::collections::BTreeMap;
use std::fmt::Write as _;

use regia_app::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, PartialReflect, ReflectComponent, ReflectRef, ReflectResource};
use regia_scene::components::Name;

/// Every reflected component of every entity as `"<entity>/<Component>" -> canonical text`.
pub type StateSnapshot = BTreeMap<String, String>;

/// Key prefix of the resource entries added by [`insert_resource_text`].
pub const RESOURCE_KEY_PREFIX: &str = "$resource/";

const ENTITY_MARKER_START: char = '\u{1}';
const ENTITY_MARKER_END: char = '\u{2}';
// In marked text, map and set entries are wrapped in these so `replace_entities` can sort them
// again after entity ids became keys: sorting by raw ids would make the order follow ids.
const UNORDERED_OPEN: char = '\u{3}';
const UNORDERED_SEP: char = '\u{4}';
const UNORDERED_CLOSE: char = '\u{5}';

/// The canonical text of a reflected value (see the module docs).
#[must_use]
pub fn canonical_text(value: &dyn PartialReflect) -> String {
    canonical_text_impl(value, false)
}

/// Canonical text with reflected `Entity` values marked for later key replacement.
pub(crate) fn canonical_text_with_entity_markers(value: &dyn PartialReflect) -> String {
    canonical_text_impl(value, true)
}

fn canonical_text_impl(value: &dyn PartialReflect, mark_entities: bool) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value, mark_entities);
    out
}

fn write_canonical(out: &mut String, value: &dyn PartialReflect, mark_entities: bool) {
    match value.reflect_ref() {
        ReflectRef::Struct(s) => {
            out.push('{');
            for (index, (name, field)) in s.iter_fields().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(name);
                out.push(':');
                write_canonical(out, field, mark_entities);
            }
            out.push('}');
        }
        ReflectRef::TupleStruct(t) => write_sequence(out, '(', ')', t.iter_fields(), mark_entities),
        ReflectRef::Tuple(t) => write_sequence(out, '(', ')', t.iter_fields(), mark_entities),
        ReflectRef::List(list) => write_sequence(out, '[', ']', list.iter(), mark_entities),
        ReflectRef::Array(array) => write_sequence(out, '[', ']', array.iter(), mark_entities),
        ReflectRef::Map(map) => {
            let entries: Vec<String> = map
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}=>{}",
                        canonical_text_impl(key, mark_entities),
                        canonical_text_impl(value, mark_entities)
                    )
                })
                .collect();
            write_unordered(out, entries, mark_entities);
        }
        ReflectRef::Set(set) => {
            let entries: Vec<String> = set
                .iter()
                .map(|value| canonical_text_impl(value, mark_entities))
                .collect();
            write_unordered(out, entries, mark_entities);
        }
        ReflectRef::Enum(e) => {
            out.push_str(e.variant_name());
            if e.field_len() > 0 {
                out.push('(');
                for (index, field) in e.iter_fields().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    if let Some(name) = field.name() {
                        out.push_str(name);
                        out.push(':');
                    }
                    write_canonical(out, field.value(), mark_entities);
                }
                out.push(')');
            }
        }
        // Only a value reflected as `Entity` is a reference. Strings and other opaque values
        // stay literal even when their debug text happens to equal an entity's display text.
        _ => {
            if mark_entities && let Some(entity) = value.try_downcast_ref::<Entity>() {
                out.push(ENTITY_MARKER_START);
                let _ = write!(out, "{entity}");
                out.push(ENTITY_MARKER_END);
            } else {
                let _ = write!(out, "{value:?}");
            }
        }
    }
}

/// Map or set entries, sorted by their text. Marked text keeps them delimited so
/// [`replace_entities`] sorts them again once entity references are keys.
fn write_unordered(out: &mut String, mut entries: Vec<String>, mark_entities: bool) {
    entries.sort_unstable();
    if mark_entities {
        out.push(UNORDERED_OPEN);
        for (index, entry) in entries.iter().enumerate() {
            if index > 0 {
                out.push(UNORDERED_SEP);
            }
            out.push_str(entry);
        }
        out.push(UNORDERED_CLOSE);
    } else {
        out.push('{');
        out.push_str(&entries.join(","));
        out.push('}');
    }
}

fn write_sequence<'a>(
    out: &mut String,
    open: char,
    close: char,
    items: impl Iterator<Item = &'a dyn PartialReflect>,
    mark_entities: bool,
) {
    out.push(open);
    for (index, item) in items.enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_canonical(out, item, mark_entities);
    }
    out.push(close);
}

/// The world's observable state (see the module docs). Entities are keyed by `Name`; an unnamed
/// entity by the set of its component types (and a rank among entities with the same set, ordered
/// by content), so the key does not depend on entity ids. Entity references inside values print
/// as those keys.
#[must_use]
pub fn world_snapshot(world: &World) -> StateSnapshot {
    world_snapshot_where(world, |_| true)
}

/// [`world_snapshot`] restricted to the components whose short type name passes `keep` (for
/// example only a game's gameplay components, leaving out asset handles and render state).
/// Entities with no kept component are left out, and unnamed entities are keyed and ranked by
/// their kept components only.
#[must_use]
pub fn world_snapshot_where(world: &World, keep: impl Fn(&str) -> bool) -> StateSnapshot {
    world_snapshot_of(world, |_| true, keep)
}

/// [`world_snapshot_where`] over the entities `keep_entity` accepts only (for example the entities
/// a demo tracks, `crate::demo_world`). Entity references to entities outside the set print as
/// their raw id text.
#[must_use]
pub fn world_snapshot_of(
    world: &World,
    keep_entity: impl Fn(EntityRef<'_>) -> bool,
    keep: impl Fn(&str) -> bool,
) -> StateSnapshot {
    world_snapshot_keyed(world, keep_entity, keep).0
}

/// [`world_snapshot_of`] plus the map from each kept entity's id text to its key, for printing
/// entity references in other values (demo resources) the same way.
pub(crate) fn world_snapshot_keyed(
    world: &World,
    keep_entity: impl Fn(EntityRef<'_>) -> bool,
    keep: impl Fn(&str) -> bool,
) -> (StateSnapshot, BTreeMap<String, String>) {
    let Some(registry) = world.get_resource::<AppTypeRegistry>() else {
        return (StateSnapshot::new(), BTreeMap::new());
    };
    let registry = registry.read();
    // Reflected components of each entity: (short type name, canonical text).
    let mut per_entity: Vec<(Entity, Vec<(String, String)>)> = Vec::new();
    let infos = world.components();
    for entity in world.iter_entities() {
        if !keep_entity(entity) {
            continue;
        }
        let mut components = Vec::new();
        // Only the entity's own components (not every registered type): a snapshot of a large
        // world stays cheap enough to take at every checkpoint.
        for &component_id in entity.archetype().components() {
            let Some(registration) = infos
                .get_info(component_id)
                .and_then(|info| info.type_id())
                .and_then(|type_id| registry.get(type_id))
            else {
                continue;
            };
            if registration.data::<ReflectResource>().is_some() {
                continue;
            }
            let Some(reflect) = registration.data::<ReflectComponent>() else {
                continue;
            };
            let short = registration.type_info().type_path_table().short_path();
            if !keep(short) {
                continue;
            }
            if let Some(value) = reflect.reflect(entity) {
                components.push((
                    short.to_string(),
                    canonical_text_with_entity_markers(value.as_partial_reflect()),
                ));
            }
        }
        if !components.is_empty() {
            components.sort();
            per_entity.push((entity.id(), components));
        }
    }
    // Keys. Ranks inside a group are ordered by content with entity references masked, so the
    // order does not follow entity ids.
    let masked: BTreeMap<String, String> = per_entity
        .iter()
        .map(|(entity, _)| (entity.to_string(), "_".to_string()))
        .collect();
    let mut key_of: BTreeMap<Entity, String> = BTreeMap::new();
    let mut groups: BTreeMap<String, Vec<(String, u64, Entity)>> = BTreeMap::new();
    let mut labelled: Vec<(String, String)> = Vec::with_capacity(per_entity.len());
    for (entity, components) in &per_entity {
        let content: String = components
            .iter()
            .map(|(t, v)| format!("{t}={};", replace_entities(v, &masked)))
            .collect();
        let label = if let Some(name) = world.get::<Name>(*entity) {
            name.as_str().to_string()
        } else {
            let types: Vec<&str> = components.iter().map(|(t, _)| t.as_str()).collect();
            format!("@{:016x}", fnv1a(types.join(",").as_bytes()))
        };
        labelled.push((label, content));
    }
    let classes = reference_classes(&per_entity, &labelled);
    for (((label, content), class), (entity, _)) in labelled.into_iter().zip(classes).zip(&per_entity) {
        groups.entry(label).or_default().push((content, class, *entity));
    }
    for (label, mut members) in groups {
        // Content first; a tie goes to the reference class (canonical, see `reference_classes`)
        // and only entities that class cannot tell apart fall back to the id.
        members.sort();
        // A unique name is its own key; duplicate names and unnamed entities get a rank.
        let ranked = members.len() > 1 || label.starts_with('@');
        for (rank, (_, _, entity)) in members.into_iter().enumerate() {
            let key = if ranked {
                format!("{label}#{rank}")
            } else {
                label.clone()
            };
            key_of.insert(entity, key);
        }
    }
    let by_text: BTreeMap<String, String> = key_of
        .iter()
        .map(|(entity, key)| (entity.to_string(), key.clone()))
        .collect();
    let mut out = StateSnapshot::new();
    for (entity, components) in per_entity {
        let Some(key) = key_of.get(&entity) else {
            continue;
        };
        for (component, value) in components {
            out.insert(format!("{key}/{component}"), replace_entities(&value, &by_text));
        }
    }
    (out, by_text)
}

/// A class per entity of `per_entity` that depends on content and references but never on entity
/// ids, so entities whose masked contents tie are ranked by how they reference each other (audit
/// framework5 #7). `labelled` holds each entity's group label and masked content.
///
/// Colour refinement: the first class hashes the label and the masked content. Each round
/// rehashes every entity that still shares its class with another, from its class, its
/// components with each reference printed as the referenced entity's class, and the sorted
/// classes (with component names) of the entities that reference it; it stops when a round splits
/// no class. Entities the refinement cannot tell apart (symmetric ones, such as two entities that
/// each reference only themselves) keep one class, and their order cannot change the snapshot.
fn reference_classes(
    per_entity: &[(Entity, Vec<(String, String)>)],
    labelled: &[(String, String)],
) -> Vec<u64> {
    let mut classes: Vec<u64> = labelled
        .iter()
        .map(|(label, content)| fnv1a(format!("{label}\n{content}").as_bytes()))
        .collect();
    let distinct = |classes: &[u64]| classes.iter().collect::<std::collections::BTreeSet<_>>().len();
    let mut count = distinct(&classes);
    if count == classes.len() {
        return classes;
    }
    // Who references whom: incoming[j] lists (referrer, component) once per reference.
    let index: BTreeMap<String, usize> = per_entity
        .iter()
        .enumerate()
        .map(|(i, (entity, _))| (entity.to_string(), i))
        .collect();
    let mut incoming: Vec<Vec<(usize, &str)>> = vec![Vec::new(); per_entity.len()];
    for (i, (_, components)) in per_entity.iter().enumerate() {
        for (t, v) in components {
            // Only the marker spans `replace_entities` treats as references. An alphanumeric
            // run that happens to equal an entity's `Display` text is user data, not an edge,
            // and must not change an unnamed entity's rank.
            let mut rest = v.as_str();
            while let Some(start) = rest.find(ENTITY_MARKER_START) {
                let marked = &rest[start + ENTITY_MARKER_START.len_utf8()..];
                let Some(end) = marked.find(ENTITY_MARKER_END) else {
                    break;
                };
                if let Some(&j) = index.get(&marked[..end]) {
                    incoming[j].push((i, t.as_str()));
                }
                rest = &marked[end + ENTITY_MARKER_END.len_utf8()..];
            }
        }
    }
    // Each splitting round adds a class, so there are at most as many rounds as entities.
    for _ in 0..per_entity.len() {
        let mut sizes: BTreeMap<u64, usize> = BTreeMap::new();
        for class in &classes {
            *sizes.entry(*class).or_default() += 1;
        }
        let names: BTreeMap<String, String> = per_entity
            .iter()
            .zip(&classes)
            .map(|((entity, _), class)| (entity.to_string(), format!("{class:016x}")))
            .collect();
        let next: Vec<u64> = per_entity
            .iter()
            .zip(&classes)
            .zip(&incoming)
            .map(|(((_, components), &class), referrers)| {
                if sizes.get(&class).is_none_or(|n| *n < 2) {
                    return class;
                }
                let mut text = format!("{class:016x}|");
                for (t, v) in components {
                    let _ = write!(text, "{t}={};", replace_entities(v, &names));
                }
                let mut from: Vec<(u64, &str)> = referrers.iter().map(|&(i, t)| (classes[i], t)).collect();
                from.sort_unstable();
                for (class, t) in from {
                    let _ = write!(text, "<{t}:{class:016x}");
                }
                fnv1a(text.as_bytes())
            })
            .collect();
        let next_count = distinct(&next);
        classes = next;
        if next_count == count || next_count == classes.len() {
            break;
        }
        count = next_count;
    }
    classes
}

/// Adds the reflected resource whose short type name is `short_name` to `snapshot` under
/// `"$resource/<short_name>"`. Returns false (and adds nothing) when no such reflected resource
/// is in the world.
pub fn insert_resource_text(world: &World, snapshot: &mut StateSnapshot, short_name: &str) -> bool {
    let Some(registry) = world.get_resource::<AppTypeRegistry>() else {
        return false;
    };
    let registry = registry.read();
    for registration in registry.iter() {
        if registration.type_info().type_path_table().short_path() != short_name {
            continue;
        }
        // Resources are stored on entities (bevy 0.20): read the value through the resource's
        // reflected component.
        if registration.data::<ReflectResource>().is_none() {
            continue;
        }
        let Some(component) = registration.data::<ReflectComponent>() else {
            continue;
        };
        if let Some(value) = world.iter_entities().find_map(|entity| component.reflect(entity)) {
            snapshot.insert(
                format!("{RESOURCE_KEY_PREFIX}{short_name}"),
                canonical_text(value.as_partial_reflect()),
            );
            return true;
        }
    }
    false
}

/// Replaces marked reflected `Entity` values with their snapshot keys. Unmarked text, including
/// strings that look exactly like an entity's `Display` text, stays unchanged. References outside
/// the keyed set lose their marker and keep their raw id text. Map and set entries are sorted
/// after the replacement, so their order follows the keys and never the entity ids.
pub(crate) fn replace_entities(text: &str, keys: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let _ = replace_until(&mut rest, keys, &mut out, false);
    out
}

/// Which delimiter ended a nested [`replace_until`] run.
enum UnorderedEnd {
    Sep,
    Close,
}

/// Copies `rest` into `out` with entity markers replaced and unordered containers sorted after
/// replacement (so map and set order follows keys, not ids). Inside a container (`nested`) it
/// stops at that container's separator or close and returns which; at the top level a stray
/// delimiter is copied literally and `None` is returned at the end of the text.
fn replace_until(
    rest: &mut &str,
    keys: &BTreeMap<String, String>,
    out: &mut String,
    nested: bool,
) -> Option<UnorderedEnd> {
    const SPECIAL: [char; 4] = [
        ENTITY_MARKER_START,
        UNORDERED_OPEN,
        UNORDERED_SEP,
        UNORDERED_CLOSE,
    ];
    loop {
        let Some(at) = rest.find(SPECIAL) else {
            out.push_str(rest);
            *rest = "";
            return None;
        };
        let (prefix, special) = rest.split_at(at);
        out.push_str(prefix);
        let Some(c) = special.chars().next() else {
            *rest = "";
            return None;
        };
        let after = &special[c.len_utf8()..];
        match c {
            ENTITY_MARKER_START => {
                let Some(end) = after.find(ENTITY_MARKER_END) else {
                    // Markers are internal, but a custom opaque `Debug` can emit arbitrary text.
                    // Preserve an unmatched marker rather than discarding user data.
                    out.push_str(special);
                    *rest = "";
                    return None;
                };
                let entity = &after[..end];
                if let Some(key) = keys.get(entity) {
                    out.push('<');
                    out.push_str(key);
                    out.push('>');
                } else {
                    out.push_str(entity);
                }
                *rest = &after[end + ENTITY_MARKER_END.len_utf8()..];
            }
            UNORDERED_OPEN => {
                *rest = after;
                let mut entries = Vec::new();
                let closed = loop {
                    let mut entry = String::new();
                    let end = replace_until(rest, keys, &mut entry, true);
                    entries.push(entry);
                    match end {
                        Some(UnorderedEnd::Sep) => {}
                        Some(UnorderedEnd::Close) => break true,
                        None => break false,
                    }
                };
                if entries.len() == 1 && entries[0].is_empty() {
                    entries.clear();
                }
                entries.sort_unstable();
                out.push('{');
                out.push_str(&entries.join(","));
                if closed {
                    out.push('}');
                }
            }
            UNORDERED_SEP | UNORDERED_CLOSE if nested => {
                *rest = after;
                return Some(if c == UNORDERED_SEP {
                    UnorderedEnd::Sep
                } else {
                    UnorderedEnd::Close
                });
            }
            _ => {
                out.push(c);
                *rest = after;
            }
        }
    }
}

/// FNV-1a over bytes: stable across runs and platforms.
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// A hash of a whole snapshot (every `key=value` line in key order).
#[must_use]
pub fn snapshot_hash(snapshot: &StateSnapshot) -> u64 {
    let mut text = String::new();
    for (key, value) in snapshot {
        let _ = writeln!(text, "{key}={value}");
    }
    fnv1a(text.as_bytes())
}

/// One hash per snapshot key, for comparing two runs key by key without keeping their text.
#[must_use]
pub fn key_hashes(snapshot: &StateSnapshot) -> BTreeMap<String, u64> {
    snapshot
        .iter()
        .map(|(key, value)| (key.clone(), fnv1a(value.as_bytes())))
        .collect()
}

/// The keys whose value differs between two snapshots (including keys present in one only),
/// sorted, at most `limit`.
#[must_use]
pub fn differing_keys(a: &StateSnapshot, b: &StateSnapshot, limit: usize) -> Vec<String> {
    let mut keys: Vec<String> = a
        .iter()
        .filter(|(k, v)| b.get(*k) != Some(v))
        .map(|(k, _)| k.clone())
        .chain(b.keys().filter(|k| !a.contains_key(*k)).cloned())
        .collect();
    keys.sort();
    keys.dedup();
    keys.truncate(limit);
    keys
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    #[derive(Reflect, Default)]
    struct Inner {
        weights: HashMap<String, f32>,
        tags: HashSet<u32>,
        order: Vec<i32>,
    }

    #[derive(Component, Reflect, Default)]
    #[reflect(Component)]
    struct Probe {
        speed: f32,
        target: Option<Entity>,
        text: String,
        inner: Inner,
    }

    #[derive(Component, Reflect, Default)]
    #[reflect(Component)]
    struct EntityContainers {
        list: Vec<Entity>,
        map: HashMap<String, Entity>,
        set: HashSet<Entity>,
        strings: HashMap<String, String>,
    }

    #[derive(Resource, Reflect, Default)]
    #[reflect(Resource)]
    struct Score(u32);

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<AppTypeRegistry>();
        {
            let registry = world.resource::<AppTypeRegistry>();
            let mut registry = registry.write();
            registry.register::<Probe>();
            registry.register::<EntityContainers>();
            registry.register::<Score>();
        }
        world
    }

    fn inner(entries: &[(&str, f32)], tags: &[u32]) -> Inner {
        Inner {
            weights: entries.iter().map(|(k, v)| ((*k).to_string(), *v)).collect(),
            tags: tags.iter().copied().collect(),
            order: vec![3, 1, 2],
        }
    }

    #[test]
    fn map_and_set_order_never_reaches_the_text() {
        let forward = inner(&[("a", 1.0), ("b", 2.0), ("c", 3.0)], &[1, 2, 3, 4, 5]);
        let backward = inner(&[("c", 3.0), ("b", 2.0), ("a", 1.0)], &[5, 4, 3, 2, 1]);
        assert_eq!(canonical_text(&forward), canonical_text(&backward));
        assert_eq!(
            canonical_text(&forward),
            "{weights:{\"a\"=>1.0,\"b\"=>2.0,\"c\"=>3.0},tags:{1,2,3,4,5},order:[3,1,2]}"
        );
        // List order is state and stays in the text.
        let mut reordered = inner(&[("a", 1.0)], &[]);
        reordered.order = vec![1, 2, 3];
        assert_ne!(
            canonical_text(&reordered),
            canonical_text(&inner(&[("a", 1.0)], &[]))
        );
    }

    #[test]
    fn distinct_floats_give_distinct_text() {
        assert_ne!(canonical_text(&0.0f32), canonical_text(&-0.0f32));
        assert_ne!(
            canonical_text(&0.1f32),
            canonical_text(&f32::from_bits(0.1f32.to_bits() + 1))
        );
    }

    #[test]
    fn entities_are_keyed_by_name_and_references_follow_them() {
        let mut a = world();
        let target = a.spawn((Name::new("goal"), Probe::default())).id();
        a.spawn((
            Name::new("seeker"),
            Probe {
                speed: 2.5,
                target: Some(target),
                ..Probe::default()
            },
        ));
        // The same scene with different entity ids: an unrelated entity (with no reflected
        // component) spawned first.
        let mut b = world();
        let _spare = b.spawn_empty().id();
        let target = b.spawn((Name::new("goal"), Probe::default())).id();
        b.spawn((
            Name::new("seeker"),
            Probe {
                speed: 2.5,
                target: Some(target),
                ..Probe::default()
            },
        ));
        let (snap_a, snap_b) = (world_snapshot(&a), world_snapshot(&b));
        assert_eq!(snap_a, snap_b, "entity ids are not state");
        assert!(
            snap_a["seeker/Probe"].contains("<goal>"),
            "{}",
            snap_a["seeker/Probe"]
        );
        assert_eq!(snapshot_hash(&snap_a), snapshot_hash(&snap_b));
    }

    /// Spawns unnamed `Probe` entities, one per entry of `targets` in that order; entry `i`
    /// targets the entity spawned `targets[i]`-th.
    fn unnamed_graph(targets: &[usize]) -> World {
        let mut world = world();
        let ids: Vec<Entity> = targets
            .iter()
            .map(|_| world.spawn(Probe::default()).id())
            .collect();
        for (i, &t) in targets.iter().enumerate() {
            if let Some(mut probe) = world.get_mut::<Probe>(ids[i]) {
                probe.target = Some(ids[t]);
            }
        }
        world
    }

    /// Unnamed entities whose masked contents tie are ranked by the references they hold, not by
    /// entity id (audit framework5 #7): the same reference graph spawned in another order hashes
    /// the same.
    #[test]
    fn tied_unnamed_entities_rank_by_their_references_not_their_ids() {
        // x -> y, y -> y: x spawned first, then y spawned first.
        let a = unnamed_graph(&[1, 1]);
        let b = unnamed_graph(&[0, 0]);
        let (snap_a, snap_b) = (world_snapshot(&a), world_snapshot(&b));
        assert_eq!(snap_a, snap_b, "entity ids are not state");
        assert_eq!(snapshot_hash(&snap_a), snapshot_hash(&snap_b));
        // A three-entity chain into a self loop, in every spawn order.
        let orders: [[usize; 3]; 6] = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
        let mut hashes = Vec::new();
        for order in orders {
            // Logical node n (0 -> 1 -> 2 -> 2) is spawned at position order[n].
            let mut targets = [0usize; 3];
            for (n, next) in [1usize, 2, 2].into_iter().enumerate() {
                targets[order[n]] = order[next];
            }
            hashes.push(snapshot_hash(&world_snapshot(&unnamed_graph(&targets))));
        }
        assert!(hashes.windows(2).all(|w| w[0] == w[1]), "{hashes:x?}");
        // Two self loops stay one state however the ranks fall.
        assert_eq!(
            world_snapshot(&unnamed_graph(&[0, 1])),
            world_snapshot(&unnamed_graph(&[0, 1]))
        );
    }

    #[test]
    fn a_string_equal_to_an_entity_id_does_not_change_unnamed_ranks() {
        fn spawn_ids() -> (String, String) {
            let mut world = world();
            let a = world.spawn(Probe::default()).id();
            let b = world.spawn(Probe::default()).id();
            (a.to_string(), b.to_string())
        }
        fn keys(token: &str) -> StateSnapshot {
            let mut world = world();
            let a = world.spawn(Probe::default()).id();
            let b = world.spawn(Probe::default()).id();
            if let Some(mut probe) = world.get_mut::<Probe>(a) {
                probe.target = Some(a);
            }
            if let Some(mut probe) = world.get_mut::<Probe>(b) {
                probe.target = Some(b);
            }
            world.spawn((
                Name::new("holder"),
                Probe {
                    text: token.to_string(),
                    ..Probe::default()
                },
            ));
            world_snapshot_keyed(&world, |_| true, |_| true).1
        }

        let (id_a, id_b) = spawn_ids();
        let neutral = keys("not-an-id");
        assert_eq!(
            keys(&id_a),
            neutral,
            "the string {id_a} is not a reference, so ranks stay with the neutral string"
        );
        assert_eq!(
            keys(&id_b),
            neutral,
            "the string {id_b} is not a reference, so ranks stay with the neutral string"
        );
    }

    #[test]
    fn entity_id_text_inside_a_string_is_not_an_entity_reference() {
        fn snapshot(unrelated_index: usize, token: Option<&str>) -> (StateSnapshot, String) {
            let mut world = world();
            for _ in 0..unrelated_index {
                world.spawn_empty();
            }
            let unrelated = world.spawn((Name::new("unrelated"), Probe::default())).id();
            let token = token.map_or_else(|| unrelated.to_string(), str::to_owned);
            world.spawn((
                Name::new("holder"),
                Probe {
                    text: token.clone(),
                    ..Probe::default()
                },
            ));
            (world_snapshot(&world), token)
        }

        let (with_entity_5, token) = snapshot(5, None);
        let (with_entity_6, _) = snapshot(6, Some(&token));
        assert_eq!(with_entity_5["holder/Probe"], with_entity_6["holder/Probe"]);
        assert!(with_entity_5["holder/Probe"].contains(&format!("text:\"{token}\"")));
        assert_eq!(snapshot_hash(&with_entity_5), snapshot_hash(&with_entity_6));
    }

    #[test]
    fn entity_sets_order_by_key_not_by_entity_id() {
        // The same state, spawned in two orders: a set of two entities must print identically.
        fn snapshot(a_first: bool) -> StateSnapshot {
            let mut world = world();
            let (a, b) = if a_first {
                let a = world.spawn((Name::new("a"), Probe::default())).id();
                let b = world.spawn((Name::new("b"), Probe::default())).id();
                (a, b)
            } else {
                let b = world.spawn((Name::new("b"), Probe::default())).id();
                let a = world.spawn((Name::new("a"), Probe::default())).id();
                (a, b)
            };
            world.spawn((
                Name::new("holder"),
                EntityContainers {
                    list: vec![a, b],
                    map: HashMap::from([("x".to_string(), b), ("y".to_string(), a)]),
                    set: HashSet::from([a, b]),
                    strings: HashMap::new(),
                },
            ));
            world_snapshot(&world)
        }
        let first = snapshot(true);
        let second = snapshot(false);
        assert_eq!(
            first["holder/EntityContainers"],
            second["holder/EntityContainers"]
        );
        assert!(
            first["holder/EntityContainers"].contains("set:{<a>,<b>}"),
            "{first:?}"
        );
        assert_eq!(snapshot_hash(&first), snapshot_hash(&second));
    }

    #[test]
    fn unordered_markers_never_reach_the_snapshot_text() {
        let mut world = world();
        let goal = world.spawn((Name::new("goal"), Probe::default())).id();
        world.spawn((
            Name::new("holder"),
            EntityContainers {
                list: Vec::new(),
                map: HashMap::new(),
                set: HashSet::from([goal]),
                strings: HashMap::from([("k".to_string(), "v".to_string())]),
            },
        ));
        let snapshot = world_snapshot(&world);
        let text = &snapshot["holder/EntityContainers"];
        assert!(
            !text.contains(['\u{1}', '\u{2}', '\u{3}', '\u{4}', '\u{5}']),
            "{text:?}"
        );
        assert!(text.contains("map:{}"), "{text}");
        assert!(text.contains("set:{<goal>}"), "{text}");
        assert!(text.contains("strings:{\"k\"=>\"v\"}"), "{text}");
    }

    #[test]
    fn nested_entity_references_are_marked_but_nested_strings_stay_literal() {
        fn snapshot(spares: usize, token: Option<&str>) -> (StateSnapshot, String) {
            let mut world = world();
            for _ in 0..spares {
                world.spawn_empty();
            }
            let target = world.spawn((Name::new("goal"), Probe::default())).id();
            let token = token.map_or_else(|| target.to_string(), str::to_owned);
            world.spawn((
                Name::new("holder"),
                EntityContainers {
                    list: vec![target],
                    map: HashMap::from([("goal".to_string(), target)]),
                    set: HashSet::from([target]),
                    strings: HashMap::from([(token.clone(), token.clone())]),
                },
            ));
            (world_snapshot(&world), token)
        }

        let (first, token) = snapshot(0, None);
        let (second, _) = snapshot(1, Some(&token));
        assert_eq!(first, second);
        let text = &first["holder/EntityContainers"];
        assert_eq!(text.matches("<goal>").count(), 3, "{text}");
        assert!(text.contains(&format!("\"{token}\"")), "{text}");
    }

    #[test]
    fn differing_keys_name_the_changed_component() {
        let mut world = world();
        let seeker = world.spawn((Name::new("seeker"), Probe::default())).id();
        let before = world_snapshot(&world);
        if let Some(mut probe) = world.get_mut::<Probe>(seeker) {
            probe.speed = 1.0;
        }
        let after = world_snapshot(&world);
        assert_ne!(snapshot_hash(&before), snapshot_hash(&after));
        assert_eq!(
            differing_keys(&before, &after, 8),
            vec!["seeker/Probe".to_string()]
        );
        let hashes = key_hashes(&after);
        assert_eq!(hashes.len(), after.len());
    }

    #[test]
    fn resources_join_the_snapshot_by_short_name() {
        let mut world = world();
        world.insert_resource(Score(7));
        let mut snapshot = world_snapshot(&world);
        assert!(insert_resource_text(&world, &mut snapshot, "Score"));
        assert_eq!(snapshot["$resource/Score"], "(7)");
        assert!(!insert_resource_text(&world, &mut snapshot, "Missing"));
    }
}
